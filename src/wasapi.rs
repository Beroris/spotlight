//! Plays audio through Windows' audio engine directly.
//!
//! Audio libraries keep Windows' whole buffer full. On a device whose engine runs
//! every 10 ms that holds about 20 ms that can no longer be changed, so every pause
//! and volume change waits for it. This keeps little more than one engine cycle
//! queued. If the PC gets too busy to keep that up, the margin grows by itself, so
//! a loaded machine trades a few milliseconds for audio that doesn't break up.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
};

use anyhow::{anyhow, Context, Result};
use windows::{
    core::{w, HSTRING, PWSTR},
    Win32::{
        Devices::Properties::DEVPKEY_Device_FriendlyName,
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        Media::Audio::{
            eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
            MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            DEVICE_STATE_ACTIVE, WAVEFORMATEX,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, StructuredStorage::PropVariantClear,
                CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
            },
            Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject},
        },
    },
};

const CHANNELS: u16 = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// Room to grow the margin into, in 100 ns units: 80 ms.
const BUFFER_DURATION: i64 = 800_000;
/// No word from the engine for this long means the device is gone.
const STALL_TIMEOUT_MS: u32 = 1_000;

/// Times the queue ran out, which is what makes the margin grow.
pub static UNDERRUNS: AtomicU64 = AtomicU64::new(0);

/// How many frames to keep queued at first: one engine cycle and a quarter.
pub fn starting_margin(period_frames: u32) -> u32 {
    period_frames * 5 / 4
}

/// The margin after running out: half a cycle more, up to four cycles.
pub fn grown_margin(margin: u32, period_frames: u32, capacity: u32) -> u32 {
    (margin + period_frames / 2).min(period_frames * 4).min(capacity)
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe {
        // Fails harmlessly if this thread already set COM up.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        Ok(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?)
    }
}

fn device_id(device: &IMMDevice) -> Result<String> {
    unsafe {
        let raw: PWSTR = device.GetId()?;
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.as_ptr() as *const _));
        Ok(id?)
    }
}

fn friendly_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let mut value = store.GetValue(&DEVPKEY_Device_FriendlyName as *const _ as *const _)?;
        let inner = &value.as_raw().Anonymous.Anonymous;
        let text = *(&inner.Anonymous as *const _ as *const *const u16);
        let name = if text.is_null() {
            String::new()
        } else {
            PWSTR(text as *mut u16).to_string().unwrap_or_default()
        };
        let _ = PropVariantClear(&mut value);
        Ok(name)
    }
}

/// An output device: Windows' id for it and the name people know it by.
pub struct Device {
    pub id: String,
    pub name: String,
}

/// The output devices Windows has active.
pub fn devices() -> Vec<Device> {
    let list = || -> Result<Vec<Device>> {
        unsafe {
            let collection = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut devices = Vec::new();
            for index in 0..collection.GetCount()? {
                let device = collection.Item(index)?;
                devices.push(Device { id: device_id(&device)?, name: friendly_name(&device)? });
            }
            Ok(devices)
        }
    };
    list().unwrap_or_default()
}

/// The id of the device Windows sends audio to by default.
pub fn default_device_id() -> Option<String> {
    let find = || -> Result<String> {
        unsafe { device_id(&enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?) }
    };
    find().ok()
}

/// A stream ready to play on one device. It stops when dropped.
pub struct Stream {
    pub rate: u32,
    broken: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    begin: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Stream {
    /// Connects to the device. Nothing plays until `begin`. `make_filler` is given
    /// the device's sample rate and returns what fills each buffer with interleaved
    /// stereo.
    pub fn open<F, M>(device_id: String, make_filler: M) -> Result<Self>
    where
        F: FnMut(&mut [f32]),
        M: FnOnce(u32) -> F + Send + 'static,
    {
        let broken = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (opened_tx, opened_rx) = mpsc::channel();
        let (begin_tx, begin_rx) = mpsc::channel();
        let thread = {
            let (broken, stop) = (Arc::clone(&broken), Arc::clone(&stop));
            thread::Builder::new().name("spotlight-audio-output".to_string()).spawn(move || {
                let connection = match Connection::open(&device_id) {
                    Ok(connection) => {
                        let _ = opened_tx.send(Ok(connection.rate));
                        connection
                    }
                    Err(error) => {
                        let _ = opened_tx.send(Err(error));
                        return;
                    }
                };
                if begin_rx.recv().is_err() {
                    return;
                }
                let filler = make_filler(connection.rate);
                if connection.play(filler, &stop).is_err() {
                    broken.store(true, Ordering::SeqCst);
                }
            })?
        };
        let rate = opened_rx.recv().context("audio thread ended early")??;
        Ok(Self { rate, broken, stop, begin: Some(begin_tx), thread: Some(thread) })
    }

    pub fn begin(&mut self) {
        if let Some(begin) = self.begin.take() {
            let _ = begin.send(());
        }
    }

    /// True once the stream has died, for example because the device went away.
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::SeqCst)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.begin = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Connection {
    client: IAudioClient,
    render: IAudioRenderClient,
    rate: u32,
    period_frames: u32,
    capacity: u32,
}

impl Connection {
    fn open(device_id: &str) -> Result<Self> {
        unsafe {
            let device = enumerator()?.GetDevice(&HSTRING::from(device_id))?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let rate = (*mix).nSamplesPerSec;
            CoTaskMemFree(Some(mix as *const _));
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: CHANNELS,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * CHANNELS as u32 * 4,
                nBlockAlign: CHANNELS * 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            // Windows adapts stereo to the device, whatever its channels and format.
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION,
                0,
                &format,
                None,
            )?;
            let mut period = 0i64;
            client.GetDevicePeriod(Some(&mut period), None)?;
            let period_frames = ((rate as i64 * period) / 10_000_000).max(1) as u32;
            let capacity = client.GetBufferSize()?;
            let render = client.GetService()?;
            Ok(Self { client, render, rate, period_frames, capacity })
        }
    }

    fn write(&self, frames: u32, filler: &mut impl FnMut(&mut [f32])) -> Result<()> {
        unsafe {
            let data = self.render.GetBuffer(frames)?;
            let samples = (frames * CHANNELS as u32) as usize;
            filler(std::slice::from_raw_parts_mut(data as *mut f32, samples));
            self.render.ReleaseBuffer(frames, 0)?;
        }
        Ok(())
    }

    fn play(&self, mut filler: impl FnMut(&mut [f32]), stop: &AtomicBool) -> Result<()> {
        unsafe {
            let mut task = 0u32;
            let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
            let wake = CreateEventW(None, false, false, None)?;
            let result = (|| -> Result<()> {
                self.client.SetEventHandle(wake)?;
                let mut margin = starting_margin(self.period_frames).min(self.capacity);
                self.write(margin, &mut filler)?;
                self.client.Start()?;
                loop {
                    if WaitForSingleObject(wake, STALL_TIMEOUT_MS) != WAIT_OBJECT_0 {
                        return if stop.load(Ordering::SeqCst) {
                            Ok(())
                        } else {
                            Err(anyhow!("the audio device stopped responding"))
                        };
                    }
                    if stop.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    let queued = self.client.GetCurrentPadding()?;
                    if queued == 0 {
                        UNDERRUNS.fetch_add(1, Ordering::Relaxed);
                        margin = grown_margin(margin, self.period_frames, self.capacity);
                    }
                    if queued < margin {
                        self.write(margin - queued, &mut filler)?;
                    }
                }
            })();
            let _ = self.client.Stop();
            let _ = CloseHandle(wake);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_margin_starts_small_and_only_grows_when_needed_up_to_a_limit() {
        // 10 ms at 48 kHz is 480 frames.
        assert_eq!(starting_margin(480), 600);
        assert_eq!(grown_margin(600, 480, 3840), 840);
        let mut margin = 600;
        for _ in 0..20 {
            margin = grown_margin(margin, 480, 3840);
        }
        assert_eq!(margin, 1920, "stops at four cycles");
        assert_eq!(grown_margin(600, 480, 700), 700, "never more than the buffer holds");
    }
}
