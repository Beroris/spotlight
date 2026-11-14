//! Plays audio through the system's own audio engine, on anything but Windows.
//!
//! Here the device asks for audio as it needs it, a few milliseconds at a time, so there's no
//! queue to keep short as `wasapi` has to on Windows. What matters is the same: a pause or a
//! volume change reaches the very next audio the device takes, and a stream with nothing to
//! play stops, so it doesn't keep the computer awake.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex, PoisonError,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const CHANNELS: usize = 2;

/// Times the queue ran out. The system's engine doesn't say, so this stays at none.
pub static UNDERRUNS: AtomicU64 = AtomicU64::new(0);

/// What a stream plays.
pub trait Feed {
    /// Fills the buffer with interleaved stereo. Returns whether any of it was something to
    /// hear, so a stream doesn't stop with fresh music still on its way out.
    fn fill(&mut self, buffer: &mut [f32]) -> bool;
    /// True while there's nothing to hear, such as when paused or when nothing is queued.
    fn has_nothing_to_play(&self) -> bool;
}

/// After this long with nothing to play, a stream stops until `wake_streams`. A running stream
/// keeps the device, and so the computer, from going to sleep.
const IDLE_BEFORE_STOPPING_MS: u32 = 5_000;
/// Streams stopped for lack of anything to play.
static RESTING: AtomicUsize = AtomicUsize::new(0);
/// Counts what a stream's thread should look at: music to play again, a stream gone quiet or
/// broken, or one closing.
static NEWS: Mutex<u64> = Mutex::new(0);
static NEWS_CAME: Condvar = Condvar::new();

#[cfg(test)]
pub fn resting_streams() -> usize {
    RESTING.load(Ordering::SeqCst)
}

/// Wakes every stream's thread. Taking the lock means it can't be missed between a thread's
/// last look and its wait.
fn tell_streams() {
    *NEWS.lock().unwrap_or_else(PoisonError::into_inner) += 1;
    NEWS_CAME.notify_all();
}

/// Tells resting streams to look whether they have something to play again. Call it after
/// queueing audio or unpausing.
pub fn wake_streams() {
    if RESTING.load(Ordering::SeqCst) > 0 {
        tell_streams();
    }
}

/// An output device. It's known by its name, which is all the system gives to tell them apart.
pub struct Device {
    pub id: String,
    pub name: String,
}

/// The output devices the system has.
#[cfg(target_os = "linux")]
pub fn devices() -> Vec<Device> {
    crate::audio_devices_linux::devices()
}

/// The output devices the system has.
#[cfg(not(target_os = "linux"))]
pub fn devices() -> Vec<Device> {
    let Ok(found) = cpal::default_host().output_devices() else {
        return Vec::new();
    };
    found.filter_map(|device| device.name().ok()).map(|name| Device { id: name.clone(), name }).collect()
}

/// The id of the device the system sends audio to by default.
pub fn default_device_id() -> Option<String> {
    cpal::default_host().default_output_device()?.name().ok()
}

fn find_device(device_id: &str) -> Result<cpal::Device> {
    // Looking through ALSA's list opens every device on it, so the default isn't looked for.
    #[cfg(target_os = "linux")]
    if let Some(device) = cpal::default_host().default_output_device() {
        if device.name().is_ok_and(|name| name == device_id) {
            return Ok(device);
        }
    }
    cpal::default_host()
        .output_devices()
        .context("could not list the audio devices")?
        .find(|device| device.name().is_ok_and(|name| name == device_id))
        .ok_or_else(|| anyhow!("the audio device is no longer there"))
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
        F: Feed + Send + 'static,
        M: FnOnce(u32) -> F + Send + 'static,
    {
        let broken = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (opened_tx, opened_rx) = mpsc::channel();
        let (begin_tx, begin_rx) = mpsc::channel();
        let thread = {
            let (broken, stop) = (Arc::clone(&broken), Arc::clone(&stop));
            // The system's stream has to stay on the thread that made it.
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
        tell_streams();
        self.begin = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Connection {
    device: cpal::Device,
    rate: u32,
    /// How many channels the device has. Stereo goes to its first two.
    channels: usize,
}

impl Connection {
    fn open(device_id: &str) -> Result<Self> {
        let device = find_device(device_id)?;
        let config = device.default_output_config().context("the audio device has no output format")?;
        Ok(Self { device, rate: config.sample_rate().0, channels: usize::from(config.channels()).max(1) })
    }

    fn play<F: Feed + Send + 'static>(&self, feed: F, stop: &AtomicBool) -> Result<()> {
        let feed = Arc::new(Mutex::new(feed));
        let failed = Arc::new(AtomicBool::new(false));
        // Frames in a row with nothing to hear in them.
        let quiet_frames = Arc::new(AtomicU32::new(0));
        let idle_limit = self.rate / 1_000 * IDLE_BEFORE_STOPPING_MS;
        let config = cpal::StreamConfig {
            channels: self.channels as u16,
            sample_rate: cpal::SampleRate(self.rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let stream = self.device.build_output_stream(
            &config,
            {
                let (feed, quiet_frames) = (Arc::clone(&feed), Arc::clone(&quiet_frames));
                let channels = self.channels;
                let mut stereo = Vec::new();
                move |buffer: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut feed = feed.lock().unwrap_or_else(PoisonError::into_inner);
                    let heard = if channels == CHANNELS {
                        feed.fill(buffer)
                    } else {
                        fill_other_layout(&mut *feed, buffer, channels, &mut stereo)
                    };
                    drop(feed);
                    let frames = (buffer.len() / channels) as u32;
                    if heard {
                        quiet_frames.store(0, Ordering::Relaxed);
                    } else {
                        let before = quiet_frames.load(Ordering::Relaxed);
                        let now = before.saturating_add(frames);
                        quiet_frames.store(now, Ordering::Relaxed);
                        // Said once, as it goes quiet for long enough to stop.
                        if before < idle_limit && now >= idle_limit {
                            tell_streams();
                        }
                    }
                }
            },
            {
                let failed = Arc::clone(&failed);
                move |_error| {
                    failed.store(true, Ordering::SeqCst);
                    tell_streams();
                }
            },
            None,
        )?;
        stream.play()?;
        let mut news = NEWS.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if failed.load(Ordering::SeqCst) {
                return Err(anyhow!("the audio device stopped responding"));
            }
            if quiet_frames.load(Ordering::Relaxed) >= idle_limit {
                drop(news);
                // Only silence is left on its way out, so the stream can stop.
                stream.pause()?;
                if !rest_until_needed(&feed, stop) {
                    return Ok(());
                }
                quiet_frames.store(0, Ordering::Relaxed);
                stream.play()?;
                news = NEWS.lock().unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            // The timeout is only a backstop: every change that matters wakes it.
            news = NEWS_CAME.wait_timeout(news, Duration::from_secs(1)).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

/// Fills a buffer for a device that isn't stereo: both sides mixed for one speaker, or the
/// first two channels of more, with the rest left silent.
fn fill_other_layout(feed: &mut impl Feed, buffer: &mut [f32], channels: usize, stereo: &mut Vec<f32>) -> bool {
    let frames = buffer.len() / channels;
    stereo.clear();
    stereo.resize(frames * CHANNELS, 0.0);
    let heard = feed.fill(stereo);
    for (out, pair) in buffer.chunks_exact_mut(channels).zip(stereo.chunks_exact(CHANNELS)) {
        if channels == 1 {
            out[0] = (pair[0] + pair[1]) * 0.5;
        } else {
            out[..CHANNELS].copy_from_slice(pair);
            out[CHANNELS..].fill(0.0);
        }
    }
    heard
}

/// Waits until the feed has something to play. False if the stream is stopping instead.
fn rest_until_needed<F: Feed>(feed: &Mutex<F>, stop: &AtomicBool) -> bool {
    RESTING.fetch_add(1, Ordering::SeqCst);
    let mut news = NEWS.lock().unwrap_or_else(PoisonError::into_inner);
    let needed = loop {
        if stop.load(Ordering::SeqCst) {
            break false;
        }
        if !feed.lock().unwrap_or_else(PoisonError::into_inner).has_nothing_to_play() {
            break true;
        }
        news = NEWS_CAME.wait_timeout(news, Duration::from_secs(1)).unwrap_or_else(PoisonError::into_inner).0;
    };
    drop(news);
    RESTING.fetch_sub(1, Ordering::SeqCst);
    needed
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ramp(f32);

    impl Feed for Ramp {
        fn fill(&mut self, buffer: &mut [f32]) -> bool {
            for sample in buffer {
                self.0 += 1.0;
                *sample = self.0;
            }
            true
        }

        fn has_nothing_to_play(&self) -> bool {
            false
        }
    }

    #[test]
    fn one_speaker_gets_both_sides_mixed() {
        let mut buffer = [0.0; 3];
        assert!(fill_other_layout(&mut Ramp(0.0), &mut buffer, 1, &mut Vec::new()));
        assert_eq!(buffer, [1.5, 3.5, 5.5]);
    }

    #[test]
    fn more_than_two_speakers_get_stereo_on_the_first_two() {
        let mut buffer = [9.0; 8];
        assert!(fill_other_layout(&mut Ramp(0.0), &mut buffer, 4, &mut Vec::new()));
        assert_eq!(buffer, [1.0, 2.0, 0.0, 0.0, 3.0, 4.0, 0.0, 0.0]);
    }
}
