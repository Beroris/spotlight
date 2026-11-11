//! Plays audio through Windows' audio engine directly.
//!
//! Audio libraries keep Windows' whole buffer full. On a device whose engine runs
//! every 10 ms that holds about 20 ms that can no longer be changed, so every pause
//! and volume change waits for it. This keeps little more than one engine cycle
//! queued. If the PC gets too busy to keep that up, the margin grows by itself, so
//! a loaded machine trades a few milliseconds for audio that doesn't break up.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex, PoisonError,
    },
    thread::{self, JoinHandle},
    time::Duration,
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

/// What a stream plays.
pub trait Feed {
    /// Fills the buffer with interleaved stereo. Returns whether any of it was something to
    /// hear, so a stream doesn't stop with fresh music still in Windows' buffer.
    fn fill(&mut self, buffer: &mut [f32]) -> bool;
    /// True while there's nothing to hear, such as when paused or when nothing is queued.
    fn has_nothing_to_play(&self) -> bool;
}

/// After this long with nothing to play, a stream stops until `wake_streams`. A running stream
/// wakes its thread every engine cycle, and keeps Windows from going to sleep by itself.
const IDLE_BEFORE_STOPPING_MS: u32 = 5_000;
/// Streams stopped for lack of anything to play.
static RESTING: AtomicUsize = AtomicUsize::new(0);
static WAKES: Mutex<u64> = Mutex::new(0);
static WOKEN: Condvar = Condvar::new();

#[cfg(test)]
pub fn resting_streams() -> usize {
    RESTING.load(Ordering::SeqCst)
}

/// A feed that always has something to play, so its stream never rests.
#[cfg(test)]
pub struct AlwaysPlaying<F>(pub F);

#[cfg(test)]
impl<F: FnMut(&mut [f32])> Feed for AlwaysPlaying<F> {
    fn fill(&mut self, buffer: &mut [f32]) -> bool {
        (self.0)(buffer);
        true
    }

    fn has_nothing_to_play(&self) -> bool {
        false
    }
}

/// Tells resting streams to look whether they have something to play again. Call it after
/// queueing audio or unpausing.
pub fn wake_streams() {
    if RESTING.load(Ordering::SeqCst) > 0 {
        *WAKES.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        WOKEN.notify_all();
    }
}

/// Waits until the feed has something to play. False if the stream is stopping instead.
fn rest_until_needed(feed: &impl Feed, stop: &AtomicBool) -> bool {
    RESTING.fetch_add(1, Ordering::SeqCst);
    let mut wakes = WAKES.lock().unwrap_or_else(PoisonError::into_inner);
    let needed = loop {
        if stop.load(Ordering::SeqCst) {
            break false;
        }
        if !feed.has_nothing_to_play() {
            break true;
        }
        // The timeout is only a backstop: every change that matters wakes it.
        wakes = WOKEN.wait_timeout(wakes, Duration::from_secs(1)).unwrap_or_else(PoisonError::into_inner).0;
    };
    drop(wakes);
    RESTING.fetch_sub(1, Ordering::SeqCst);
    needed
}

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
    /// Connects to the device. Nothing plays until `begin`. `make_feed` is given the
    /// device's sample rate and returns what the stream plays.
    pub fn open<F, M>(device_id: String, make_feed: M) -> Result<Self>
    where
        F: Feed,
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
                let feed = make_feed(connection.rate);
                if connection.play(feed, &stop).is_err() {
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
        wake_streams();
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

    /// Returns whether what went in was something to hear.
    fn write(&self, frames: u32, feed: &mut impl Feed) -> Result<bool> {
        unsafe {
            let data = self.render.GetBuffer(frames)?;
            let samples = (frames * CHANNELS as u32) as usize;
            let heard = feed.fill(std::slice::from_raw_parts_mut(data as *mut f32, samples));
            self.render.ReleaseBuffer(frames, 0)?;
            Ok(heard)
        }
    }

    fn play(&self, mut feed: impl Feed, stop: &AtomicBool) -> Result<()> {
        unsafe {
            let mut task = 0u32;
            let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
            let wake = CreateEventW(None, false, false, None)?;
            let result = (|| -> Result<()> {
                self.client.SetEventHandle(wake)?;
                let mut margin = starting_margin(self.period_frames).min(self.capacity);
                self.write(margin, &mut feed)?;
                self.client.Start()?;
                let idle_limit = self.rate * IDLE_BEFORE_STOPPING_MS / 1_000;
                let mut idle_frames = 0u32;
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
                        let heard = self.write(margin - queued, &mut feed)?;
                        idle_frames = if heard { 0 } else { idle_frames.saturating_add(margin - queued) };
                    }
                    if idle_frames >= idle_limit {
                        // Only silence is left in Windows' buffer, so it can go.
                        self.client.Stop()?;
                        self.client.Reset()?;
                        if !rest_until_needed(&feed, stop) {
                            return Ok(());
                        }
                        idle_frames = 0;
                        self.write(margin, &mut feed)?;
                        self.client.Start()?;
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
