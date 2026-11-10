//! Audio output with a high-quality sample-rate converter.
//!
//! Spotify audio is 44.1 kHz, but Windows shared-mode devices commonly run at
//! 48 kHz. librespot's stock output would interpolate linearly: cheap, but it
//! dulls the top end and adds aliasing at any bitrate. Here the conversion uses a
//! windowed-sinc filter, so Windows receives audio already at the device's rate.
//!
//! Pause, volume and skips act here, on the audio about to reach Windows, instead
//! of on what the player decodes next. Otherwise each would wait for the buffer.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use librespot_playback::{
    audio_backend::{Sink, SinkError, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use crate::wasapi;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

pub const SOURCE_RATE: u32 = 44_100;
const CHANNELS: usize = 2;
const CHUNK_FRAMES: usize = 1024;
/// How often to look for a chosen device that wasn't there, such as a mixer
/// that has been switched off.
const MISSING_DEVICE_RECHECK: Duration = Duration::from_secs(5);

/// The device picked in Settings. Empty means the Windows default.
static CHOSEN_DEVICE: Mutex<String> = Mutex::new(String::new());
/// Counts choices, so the player thread can notice one without taking the lock.
static CHOICE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Play through this device from now on, without restarting the player.
pub fn choose_device(name: &str) {
    *CHOSEN_DEVICE.lock().unwrap() = name.to_string();
    CHOICE_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// The names of the output devices Windows has active. This can take a while,
/// so keep it off the UI thread.
pub fn device_names() -> Vec<String> {
    let mut names: Vec<String> = crate::wasapi::devices().into_iter().map(|device| device.name).collect();
    names.sort();
    names.dedup();
    names
}

/// Finds a saved device. Windows can renumber a device when its driver is
/// reinstalled ("2- Music (...)"), so the number doesn't have to match.
pub fn find_device(chosen: &str, names: &[String]) -> Option<usize> {
    names.iter().position(|name| name == chosen).or_else(|| {
        let wanted = without_windows_number(chosen);
        names.iter().position(|name| without_windows_number(name).eq_ignore_ascii_case(wanted))
    })
}

fn without_windows_number(name: &str) -> &str {
    let rest = name.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() == name.len() {
        return name;
    }
    rest.trim_start().strip_prefix("- ").unwrap_or(name)
}

/// Converts interleaved stereo from 44.1 kHz to the output rate. It keeps
/// partial chunks between calls, so packet boundaries leave no seams.
pub struct StereoResampler {
    /// `None` when the device already runs at 44.1 kHz: no conversion at all.
    filter: Option<SincFixedIn<f64>>,
    pending: [Vec<f64>; CHANNELS],
}

impl StereoResampler {
    pub fn new(output_rate: u32) -> Result<Self> {
        let filter = if output_rate == SOURCE_RATE {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(
                SincFixedIn::<f64>::new(
                    output_rate as f64 / SOURCE_RATE as f64,
                    1.0,
                    params,
                    CHUNK_FRAMES,
                    CHANNELS,
                )
                .map_err(|error| anyhow!("could not create resampler: {error}"))?,
            )
        };
        Ok(Self { filter, pending: [Vec::new(), Vec::new()] })
    }

    /// Forgets the audio so far, so nothing from before a skip reaches the output.
    pub fn reset(&mut self) {
        if let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
        self.pending[0].clear();
        self.pending[1].clear();
    }

    /// Feed interleaved stereo; returns whatever interleaved output is ready.
    pub fn process(&mut self, interleaved: &[f64]) -> Result<Vec<f32>> {
        let Some(filter) = self.filter.as_mut() else {
            return Ok(interleaved.iter().map(|sample| *sample as f32).collect());
        };
        for frame in interleaved.chunks_exact(CHANNELS) {
            self.pending[0].push(frame[0]);
            self.pending[1].push(frame[1]);
        }
        let mut output = Vec::new();
        while self.pending[0].len() >= filter.input_frames_next() {
            let needed = filter.input_frames_next();
            let chunk = [&self.pending[0][..needed], &self.pending[1][..needed]];
            let converted = filter
                .process(&chunk, None)
                .map_err(|error| anyhow!("resampling failed: {error}"))?;
            output.reserve(converted[0].len() * CHANNELS);
            for (left, right) in converted[0].iter().zip(&converted[1]) {
                output.push(*left as f32);
                output.push(*right as f32);
            }
            self.pending[0].drain(..needed);
            self.pending[1].drain(..needed);
        }
        Ok(output)
    }
}

/// Audio waiting to reach the device, as interleaved chunks at the device's rate.
static QUEUE: Mutex<VecDeque<Vec<f32>>> = Mutex::new(VecDeque::new());
static QUEUED_SAMPLES: AtomicUsize = AtomicUsize::new(0);
/// Set by the app the moment pause is pressed. The player hears about it later.
static PAUSED: AtomicBool = AtomicBool::new(false);
/// The volume as a linear factor, stored as the bits of an f32. Starts at 1.0.
static GAIN: AtomicU32 = AtomicU32::new(0x3f80_0000);
/// When the user last moved the volume, in ms since the clock's start.
static USER_VOLUME_AT: AtomicU64 = AtomicU64::new(0);
/// Set from a skip or seek until the player has acted on it. Whatever it decodes
/// in between belongs to the old position and must not be heard.
static SKIPPING: AtomicBool = AtomicBool::new(false);
static SKIP_AT: AtomicU64 = AtomicU64::new(0);
static SKIP_COUNT: AtomicU64 = AtomicU64::new(0);

/// How much audio to keep ready. Enough to ride out a busy moment on the player
/// thread. Pause, volume and skips don't wait for it.
const BUFFER_MS: usize = 300;
/// Fades that keep pause, mute and skips from clicking.
const FADE_MS: f32 = 3.0;
/// How long a skip may silence the player if the player never acts on it.
const SKIP_TIMEOUT_MS: u64 = 3_000;
/// The player's own volume messages echo the user's for a moment after a change.
/// Applying an older one would make the volume jump back.
const USER_VOLUME_HOLD_MS: u64 = 500;

fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    // Starts above zero, so "never" is always long ago.
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + USER_VOLUME_HOLD_MS + SKIP_TIMEOUT_MS
}

/// Silences or resumes the output at once. Call this before telling the player.
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::SeqCst);
}

/// Applies a volume the user just chose.
pub fn set_user_gain(gain: f32) {
    USER_VOLUME_AT.store(clock_ms(), Ordering::SeqCst);
    GAIN.store(gain.to_bits(), Ordering::SeqCst);
}

/// Applies a volume that came through the player, like a change from a phone.
pub fn set_player_gain(gain: f32) {
    if clock_ms().saturating_sub(USER_VOLUME_AT.load(Ordering::SeqCst)) >= USER_VOLUME_HOLD_MS {
        GAIN.store(gain.to_bits(), Ordering::SeqCst);
    }
}

fn clear_queue() {
    QUEUE.lock().unwrap().clear();
    QUEUED_SAMPLES.store(0, Ordering::SeqCst);
}

/// Drops what's buffered, because the user asked for a different place in the
/// music. Silence lasts until `end_skip`, when the player has moved.
pub fn begin_skip() {
    SKIP_AT.store(clock_ms(), Ordering::SeqCst);
    SKIPPING.store(true, Ordering::SeqCst);
    SKIP_COUNT.fetch_add(1, Ordering::SeqCst);
    clear_queue();
}

pub fn end_skip() {
    SKIPPING.store(false, Ordering::SeqCst);
}

/// Feeds the device from the queue. It reads the controls on every frame, so a
/// change reaches the very next audio handed to Windows.
struct OutputFeed {
    gain: f32,
    /// How far the gain may move per frame.
    gain_step: f32,
    chunk: Vec<f32>,
    position: usize,
    /// True between a frame's two channels.
    mid_frame: bool,
    /// Holding still rather than consuming audio: paused, and faded out.
    holding: bool,
    seen_skip: u64,
    fading_out_old_audio: bool,
}

impl OutputFeed {
    fn new(rate: u32) -> Self {
        Self {
            gain: f32::from_bits(GAIN.load(Ordering::SeqCst)),
            gain_step: 1.0 / (rate as f32 * FADE_MS / 1000.0),
            chunk: Vec::new(),
            position: 0,
            mid_frame: false,
            holding: false,
            seen_skip: SKIP_COUNT.load(Ordering::SeqCst),
            fading_out_old_audio: false,
        }
    }

    fn begin_frame(&mut self) {
        let skips = SKIP_COUNT.load(Ordering::Relaxed);
        if skips != self.seen_skip {
            self.seen_skip = skips;
            self.fading_out_old_audio = true;
        }
        let stop = self.fading_out_old_audio || PAUSED.load(Ordering::Relaxed);
        let target = if stop { 0.0 } else { f32::from_bits(GAIN.load(Ordering::Relaxed)) };
        self.gain = if self.gain < target {
            (self.gain + self.gain_step).min(target)
        } else {
            (self.gain - self.gain_step).max(target)
        };
        if self.fading_out_old_audio && self.gain == 0.0 {
            self.chunk.clear();
            self.position = 0;
            self.fading_out_old_audio = false;
        }
        // Mute keeps the music moving; pause doesn't.
        self.holding = stop && self.gain == 0.0;
    }

    fn next_queued_sample(&mut self) -> f32 {
        if self.position >= self.chunk.len() {
            let Some(next) = QUEUE.lock().unwrap().pop_front() else {
                return 0.0;
            };
            QUEUED_SAMPLES.fetch_sub(next.len(), Ordering::SeqCst);
            self.chunk = next;
            self.position = 0;
        }
        let sample = self.chunk.get(self.position).copied().unwrap_or(0.0);
        self.position += 1;
        sample
    }
}

impl Iterator for OutputFeed {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        if !self.mid_frame {
            self.begin_frame();
        }
        self.mid_frame = !self.mid_frame;
        if self.holding {
            return Some(0.0);
        }
        Some(self.next_queued_sample() * self.gain)
    }
}

/// An open stream on one device.
struct Output {
    stream: wasapi::Stream,
    /// Playing somewhere other than the chosen device, because that wasn't there.
    on_fallback: bool,
}

/// Opens the chosen device. If `may_fall_back`, music keeps playing through the
/// Windows default, or failing that any device, when the chosen one can't open.
fn open_output(chosen: &str, may_fall_back: bool) -> Result<Output> {
    let devices = wasapi::devices();
    let names: Vec<String> = devices.iter().map(|device| device.name.clone()).collect();
    let wanted = if chosen.is_empty() { None } else { find_device(chosen, &names) };

    let mut candidates: Vec<String> = wanted.map(|index| devices[index].id.clone()).into_iter().collect();
    if may_fall_back {
        candidates.extend(wasapi::default_device_id());
        candidates.extend(devices.into_iter().map(|device| device.id));
    }

    let mut first_error = None;
    for (index, id) in candidates.into_iter().enumerate() {
        let opened = wasapi::Stream::open(id, |rate| {
            let mut feed = OutputFeed::new(rate);
            move |buffer: &mut [f32]| {
                for sample in buffer {
                    *sample = feed.next().unwrap_or(0.0);
                }
            }
        });
        match opened {
            Ok(stream) => {
                return Ok(Output {
                    stream,
                    on_fallback: !chosen.is_empty() && (wanted.is_none() || index > 0),
                });
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    Err(match first_error {
        Some(error) => anyhow!("could not open an audio device: {error}"),
        None => anyhow!("no audio device available"),
    })
}

pub struct HighQualitySink {
    /// Nothing is opened until there's something to play.
    output: Option<Output>,
    resampler: StereoResampler,
    resampler_rate: u32,
    seen_choice: u64,
    seen_skip: u64,
    last_missing_check: Instant,
}

pub fn new_sink() -> Box<dyn Sink> {
    // A new player starts from nothing. What the last one left isn't wanted.
    clear_queue();
    set_paused(false);
    end_skip();
    Box::new(HighQualitySink {
        output: None,
        resampler: StereoResampler { filter: None, pending: [Vec::new(), Vec::new()] },
        resampler_rate: SOURCE_RATE,
        seen_choice: 0,
        seen_skip: SKIP_COUNT.load(Ordering::SeqCst),
        last_missing_check: Instant::now(),
    })
}

impl HighQualitySink {
    fn needs_new_output(&self) -> bool {
        match &self.output {
            Some(output) => {
                output.stream.is_broken() || self.seen_choice != CHOICE_COUNT.load(Ordering::SeqCst)
            }
            None => true,
        }
    }

    fn install(&mut self, mut output: Output) -> Result<()> {
        if output.stream.rate != self.resampler_rate {
            self.resampler = StereoResampler::new(output.stream.rate)?;
            self.resampler_rate = output.stream.rate;
            // What's queued was made for the old rate.
            clear_queue();
        }
        // The old stream goes first, so only one feed ever reads the queue. What's
        // queued carries over, so switching device loses next to nothing.
        self.output = None;
        output.stream.begin();
        self.output = Some(output);
        Ok(())
    }

    /// Makes sure audio has a working stream on the right device.
    fn refresh_output(&mut self) -> SinkResult<()> {
        let not_connected = |error: anyhow::Error| SinkError::NotConnected(error.to_string());
        if self.needs_new_output() {
            self.seen_choice = CHOICE_COUNT.load(Ordering::SeqCst);
            let chosen = CHOSEN_DEVICE.lock().unwrap().clone();
            match open_output(&chosen, true).and_then(|output| self.install(output)) {
                Ok(()) => {}
                // A choice that can't open shouldn't stop music that is playing.
                Err(error) => match &self.output {
                    Some(output) if !output.stream.is_broken() => {}
                    _ => {
                        self.output = None;
                        return Err(not_connected(error));
                    }
                },
            }
            self.last_missing_check = Instant::now();
        } else if self.output.as_ref().is_some_and(|output| output.on_fallback)
            && self.last_missing_check.elapsed() >= MISSING_DEVICE_RECHECK
        {
            self.last_missing_check = Instant::now();
            let chosen = CHOSEN_DEVICE.lock().unwrap().clone();
            if let Ok(output) = open_output(&chosen, false) {
                self.install(output).map_err(not_connected)?;
            }
        }
        Ok(())
    }

    /// Whether this packet was decoded before a skip took effect. A packet from
    /// after it can get here just before the news does, so it waits a moment.
    fn is_from_before_a_skip(&self) -> bool {
        if !SKIPPING.load(Ordering::SeqCst) {
            return false;
        }
        if clock_ms().saturating_sub(SKIP_AT.load(Ordering::SeqCst)) >= SKIP_TIMEOUT_MS {
            end_skip();
            return false;
        }
        thread::sleep(Duration::from_millis(2));
        SKIPPING.load(Ordering::SeqCst)
    }

    /// Holds the player back while enough audio is ready.
    fn wait_for_room(&self, limit: usize) {
        let started = Instant::now();
        loop {
            if self.needs_new_output() || SKIPPING.load(Ordering::SeqCst) {
                return;
            }
            let queued = QUEUED_SAMPLES.load(Ordering::SeqCst);
            if PAUSED.load(Ordering::SeqCst) {
                // The player can only take the pause command once this returns,
                // so audio may pile up for the moment that takes.
                if queued <= limit * 20 {
                    return;
                }
                if started.elapsed() > Duration::from_secs(2) {
                    // The command never came, so the player isn't paused after all.
                    set_paused(false);
                }
            } else if queued <= limit {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Sink for HighQualitySink {
    fn start(&mut self) -> SinkResult<()> {
        // The app clears its own pause before it tells the player to resume. Still
        // being paused means the player started from elsewhere, maybe on another
        // track, so what was held back is out of date.
        if PAUSED.swap(false, Ordering::SeqCst) {
            clear_queue();
        }
        self.refresh_output()
    }

    // librespot exits the whole app if this returns an error.
    fn stop(&mut self) -> SinkResult<()> {
        // After the app's own pause, what's queued stays for an instant resume.
        // Anything else, like the end of the last song, plays out.
        while QUEUED_SAMPLES.load(Ordering::SeqCst) > 0
            && !PAUSED.load(Ordering::SeqCst)
            && !SKIPPING.load(Ordering::SeqCst)
            && !self.needs_new_output()
        {
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _converter: &mut Converter) -> SinkResult<()> {
        self.refresh_output()?;
        let Some(rate) = self.output.as_ref().map(|output| output.stream.rate) else {
            return Err(SinkError::NotConnected("no audio device".into()));
        };
        let skips = SKIP_COUNT.load(Ordering::SeqCst);
        if skips != self.seen_skip {
            self.seen_skip = skips;
            self.resampler.reset();
        }
        if self.is_from_before_a_skip() {
            return Ok(());
        }
        let samples = packet.samples().map_err(|error| SinkError::OnWrite(error.to_string()))?;
        let resampled = self
            .resampler
            .process(samples)
            .map_err(|error| SinkError::OnWrite(error.to_string()))?;
        if !resampled.is_empty() {
            let mut queue = QUEUE.lock().unwrap();
            // Checked under the lock, so nothing old slips in behind a skip.
            if !SKIPPING.load(Ordering::SeqCst) {
                QUEUED_SAMPLES.fetch_add(resampled.len(), Ordering::SeqCst);
                queue.push_back(resampled);
            }
        }
        self.wait_for_room(rate as usize * CHANNELS * BUFFER_MS / 1000);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{f64::consts::TAU, sync::Arc};
    use rodio::{cpal::traits::HostTrait, DeviceTrait};

    fn tone(frequency: f64, rate: f64, frames: usize) -> Vec<f64> {
        (0..frames)
            .flat_map(|index| {
                let sample = 0.5 * (TAU * frequency * index as f64 / rate).sin();
                [sample, sample]
            })
            .collect()
    }

    /// Amplitude of one frequency, measured with a Hann window.
    fn level(mono: &[f64], frequency: f64, rate: f64) -> f64 {
        let n = mono.len() as f64;
        let (mut re, mut im, mut gain) = (0.0, 0.0, 0.0);
        for (index, sample) in mono.iter().enumerate() {
            let window = 0.5 - 0.5 * (TAU * index as f64 / n).cos();
            let phase = TAU * frequency * index as f64 / rate;
            re += sample * window * phase.cos();
            im += sample * window * phase.sin();
            gain += window;
        }
        2.0 * (re * re + im * im).sqrt() / gain
    }

    fn resample(input: &[f64], output_rate: u32) -> Vec<f64> {
        let mut resampler = StereoResampler::new(output_rate).unwrap();
        // Uneven packet sizes, as the decoder produces them.
        let mut output = Vec::new();
        for packet in input.chunks(1_628) {
            output.extend(resampler.process(packet).unwrap());
        }
        output.chunks_exact(2).skip(2_000).map(|frame| frame[0] as f64).collect()
    }

    /// What rodio was doing to the audio before this module existed.
    fn linear(input: &[f64], output_rate: f64) -> Vec<f64> {
        let mono: Vec<f64> = input.chunks_exact(2).map(|frame| frame[0]).collect();
        let step = SOURCE_RATE as f64 / output_rate;
        (0..((mono.len() - 1) as f64 / step) as usize)
            .map(|index| {
                let position = index as f64 * step;
                let base = position as usize;
                let fraction = position - base as f64;
                mono[base] * (1.0 - fraction) + mono[base + 1] * fraction
            })
            .collect()
    }

    #[test]
    fn finds_a_saved_device_even_if_windows_renumbers_it() {
        let names = [
            "2- Music (TC-HELICON GoXLR Mini)".to_string(),
            "4 - VG259 (AMD High Definition Audio Device)".to_string(),
            "Game (TC-HELICON GoXLR Mini)".to_string(),
        ];
        assert_eq!(find_device("Game (TC-HELICON GoXLR Mini)", &names), Some(2));
        assert_eq!(find_device("Music (TC-HELICON GoXLR Mini)", &names), Some(0));
        assert_eq!(find_device("3- Music (TC-HELICON GoXLR Mini)", &names), Some(0));
        assert_eq!(find_device("5 - VG259 (AMD High Definition Audio Device)", &names), Some(1));
        assert_eq!(find_device("Chat (TC-HELICON GoXLR Mini)", &names), None);
        // A number that is simply part of the name is left alone.
        assert_eq!(find_device("7.1 Speakers", &["Speakers".to_string()]), None);
    }

    /// The controls are global, so tests that use them take turns.
    static CONTROLS: Mutex<()> = Mutex::new(());

    fn fresh_controls() -> std::sync::MutexGuard<'static, ()> {
        let guard = CONTROLS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_queue();
        set_paused(false);
        end_skip();
        GAIN.store(1.0f32.to_bits(), Ordering::SeqCst);
        guard
    }

    /// Audio whose every sample says which frame it was: 1, 1, 2, 2, 3, 3...
    fn queue_numbered_frames(first: u32, frames: u32) {
        let chunk: Vec<f32> = (first..first + frames).flat_map(|n| [n as f32, n as f32]).collect();
        QUEUED_SAMPLES.fetch_add(chunk.len(), Ordering::SeqCst);
        QUEUE.lock().unwrap().push_back(chunk);
    }

    fn pull_frames(feed: &mut OutputFeed, frames: usize) -> Vec<f32> {
        (0..frames).map(|_| {
            let left = feed.next().unwrap();
            let right = feed.next().unwrap();
            assert_eq!(left, right, "channels went out of step");
            left
        }).collect()
    }

    const FADE_FRAMES: usize = 48 * FADE_MS as usize;

    #[test]
    fn pause_is_silent_within_the_fade_and_resume_loses_nothing() {
        let _controls = fresh_controls();
        let mut feed = OutputFeed::new(48_000);
        queue_numbered_frames(1, 10_000);
        let before = pull_frames(&mut feed, 100);
        assert_eq!(before[99], 100.0);

        set_paused(true);
        let fading = pull_frames(&mut feed, FADE_FRAMES + 2);
        assert_eq!(*fading.last().unwrap(), 0.0, "still audible after the fade");
        assert!(pull_frames(&mut feed, 5_000).iter().all(|sample| *sample == 0.0));

        // The fade consumed its frames and the rest waited.
        // Everything after the fade waited, so the numbering carries straight on.
        set_paused(false);
        let resumed = pull_frames(&mut feed, FADE_FRAMES + 10);
        let next_frame = (101 + FADE_FRAMES + 1 + FADE_FRAMES + 9) as f32;
        assert!((resumed.last().unwrap() - next_frame).abs() <= 3.0, "{:?}", resumed.last());
    }

    #[test]
    fn mute_silences_within_the_fade_but_the_music_keeps_moving() {
        let _controls = fresh_controls();
        let mut feed = OutputFeed::new(48_000);
        queue_numbered_frames(1, 10_000);
        pull_frames(&mut feed, 100);
        set_user_gain(0.0);
        let muted = pull_frames(&mut feed, FADE_FRAMES + 2);
        assert_eq!(*muted.last().unwrap(), 0.0);
        let queued_before = QUEUE.lock().unwrap().len();
        pull_frames(&mut feed, 1_000);
        set_user_gain(1.0);
        let back = pull_frames(&mut feed, FADE_FRAMES + 2);
        // 100 + the fade + 1,000 + the fade, with no frames held back.
        assert_eq!(*back.last().unwrap(), (100 + FADE_FRAMES + 2 + 1_000 + FADE_FRAMES + 2) as f32);
        assert_eq!(queued_before, 0, "the chunk should be in the feed");
    }

    #[test]
    fn a_volume_echo_from_the_player_cannot_undo_the_user() {
        let _controls = fresh_controls();
        set_user_gain(0.25);
        set_player_gain(0.75);
        assert_eq!(f32::from_bits(GAIN.load(Ordering::SeqCst)), 0.25);
        USER_VOLUME_AT.store(0, Ordering::SeqCst);
        set_player_gain(0.75);
        assert_eq!(f32::from_bits(GAIN.load(Ordering::SeqCst)), 0.75);
    }

    #[test]
    fn a_skip_fades_out_and_drops_everything_buffered() {
        let _controls = fresh_controls();
        let mut feed = OutputFeed::new(48_000);
        queue_numbered_frames(1, 1_000);
        queue_numbered_frames(1_001, 1_000);
        pull_frames(&mut feed, 10);
        begin_skip();
        assert_eq!(QUEUED_SAMPLES.load(Ordering::SeqCst), 0);
        let fading = pull_frames(&mut feed, FADE_FRAMES + 2);
        assert_eq!(*fading.last().unwrap(), 0.0);
        assert!(pull_frames(&mut feed, 3_000).iter().all(|sample| *sample == 0.0), "old audio came back");

        end_skip();
        queue_numbered_frames(5_000, 1_000);
        let new_audio = pull_frames(&mut feed, 500);
        // It fades in from the first frame of the new audio.
        assert_eq!(new_audio[FADE_FRAMES + 5], (5_000 + FADE_FRAMES + 5) as f32);
    }

    #[test]
    fn a_sink_keeps_queued_audio_for_the_apps_own_pause_only() {
        let _controls = fresh_controls();
        let mut sink = HighQualitySink {
            output: None,
            resampler: StereoResampler { filter: None, pending: [Vec::new(), Vec::new()] },
            resampler_rate: SOURCE_RATE,
            seen_choice: 0,
            seen_skip: 0,
            last_missing_check: Instant::now(),
        };
        queue_numbered_frames(1, 100);
        set_paused(true);
        // With no output this returns at once either way; the queue must survive.
        sink.stop().unwrap();
        assert_eq!(QUEUED_SAMPLES.load(Ordering::SeqCst), 200);
    }

    /// Opens every real output device in turn with silent audio. Run it with
    /// `cargo test real_devices -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs real audio devices"]
    fn switches_between_real_devices_without_stopping() {
        let _controls = fresh_controls();
        let names = device_names();
        assert!(!names.is_empty(), "no output devices");
        let hold: u64 = std::env::var("SPOTLIGHT_DEVICE_HOLD_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1_000);
        let mut sink = new_sink();
        let mut converter = Converter::new(None);
        // 4,096 frames is about the size of a decoder packet.
        let silence = || AudioPacket::Samples(vec![0.0; 4_096 * CHANNELS]);
        let mut choices = vec![String::new()];
        choices.extend(names);
        choices.push("Not a real device".to_string());
        sink.start().unwrap();
        for choice in &choices {
            choose_device(choice);
            let started = Instant::now();
            let mut played = 0.0;
            while started.elapsed() < Duration::from_millis(hold) {
                sink.write(silence(), &mut converter).unwrap();
                played += 4_096.0 / SOURCE_RATE as f64;
            }
            let seconds = started.elapsed().as_secs_f64();
            println!("{choice:?}: {seconds:.2} s took {played:.2} s of audio");
            // Audio has to keep moving through whichever device is open.
            assert!(played > seconds * 0.5, "{choice:?} stopped taking audio");
        }
        sink.stop().unwrap();
        choose_device("");
    }

    /// Plays an inaudible tone (-80 dB) on one device and flips the controls, while
    /// recording that device's real output through Windows loopback, to time when
    /// sound actually stops and starts. Pick a channel nothing else is playing on:
    /// `SPOTLIGHT_PROBE_DEVICE=... cargo test latency_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real audio device"]
    fn latency_probe_tone() {
        use rodio::cpal::traits::StreamTrait;

        let _controls = fresh_controls();
        let device_name = std::env::var("SPOTLIGHT_PROBE_DEVICE").expect("device name");
        let device = rodio::cpal::default_host()
            .output_devices()
            .unwrap()
            .find(|device| device.name().is_ok_and(|name| name == device_name))
            .expect("device");
        let config = device.default_output_config().unwrap();
        let (rate, channels) = (config.sample_rate().0 as f64, config.channels() as usize);

        // When the device's own output went between sound and silence.
        let changes: Arc<Mutex<Vec<(bool, Instant)>>> = Arc::default();
        let recorded = Arc::clone(&changes);
        let mut sound = false;
        // A tone crosses zero all the time, so silence has to last 2 ms to count.
        let quiet_needed = (rate * 0.002) as usize;
        let mut quiet_frames = 0usize;
        let mut quiet_since = None;
        let loopback = device
            .build_input_stream(
                &config.config(),
                move |data: &[f32], info: &rodio::cpal::InputCallbackInfo| {
                    let timestamp = info.timestamp();
                    let age = timestamp.callback.duration_since(&timestamp.capture).unwrap_or_default();
                    let first_frame_at = Instant::now().checked_sub(age).unwrap();
                    for (index, frame) in data.chunks_exact(channels).enumerate() {
                        let at = first_frame_at + Duration::from_secs_f64(index as f64 / rate);
                        if frame.iter().any(|sample| sample.abs() > 0.000_001) {
                            quiet_frames = 0;
                            quiet_since = None;
                            if !sound {
                                sound = true;
                                recorded.lock().unwrap().push((true, at));
                            }
                        } else {
                            quiet_frames += 1;
                            let since = *quiet_since.get_or_insert(at);
                            if sound && quiet_frames >= quiet_needed {
                                sound = false;
                                recorded.lock().unwrap().push((false, since));
                            }
                        }
                    }
                },
                |error| println!("loopback error: {error}"),
                None,
            )
            .unwrap();
        loopback.play().unwrap();

        choose_device(&device_name);
        let mut sink = new_sink();
        let mut converter = Converter::new(None);
        let mut phase = 0.0f64;
        let mut tone = move || {
            let samples: Vec<f64> = (0..1_024)
                .flat_map(|_| {
                    phase += TAU * 1_000.0 / SOURCE_RATE as f64;
                    let sample = 0.0001 * phase.sin();
                    [sample, sample]
                })
                .collect();
            AudioPacket::Samples(samples)
        };
        let mut play_for = |sink: &mut Box<dyn Sink>, ms: u64| {
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(ms) {
                sink.write(tone(), &mut converter).unwrap();
            }
        };
        let rounds: u32 = std::env::var("SPOTLIGHT_PROBE_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
        let mut flips: Vec<(&str, bool, Instant)> = Vec::new();
        sink.start().unwrap();
        play_for(&mut sink, 1_500);
        for _ in 0..rounds {
            flips.push(("pause", false, Instant::now()));
            set_paused(true);
            sink.stop().unwrap();
            thread::sleep(Duration::from_millis(250));
            flips.push(("resume", true, Instant::now()));
            set_paused(false);
            sink.start().unwrap();
            play_for(&mut sink, 250);
            flips.push(("mute", false, Instant::now()));
            set_user_gain(0.0);
            play_for(&mut sink, 250);
            flips.push(("unmute", true, Instant::now()));
            set_user_gain(1.0);
            play_for(&mut sink, 250);
        }
        thread::sleep(Duration::from_millis(100));
        drop(loopback);
        choose_device("");
        println!("underruns: {}", wasapi::UNDERRUNS.load(Ordering::Relaxed));

        let changes = changes.lock().unwrap();
        for kind in ["pause", "mute", "resume", "unmute"] {
            let mut delays: Vec<f64> = flips
                .iter()
                .filter(|(name, _, _)| *name == kind)
                .filter_map(|(_, wanted, at)| {
                    changes
                        .iter()
                        .find(|(sound, change)| sound == wanted && change >= at)
                        .map(|(_, change)| (*change - *at).as_secs_f64() * 1000.0)
                })
                .filter(|ms| *ms < 200.0)
                .collect();
            delays.sort_by(f64::total_cmp);
            match delays.as_slice() {
                [] => println!("{kind}: nothing seen"),
                some => println!(
                    "{kind:<7} n={:<3} best {:5.1}  median {:5.1}  worst {:5.1} ms",
                    some.len(),
                    some[0],
                    some[some.len() / 2],
                    some[some.len() - 1]
                ),
            }
        }
    }

    #[test]
    fn keeps_high_frequencies_that_linear_interpolation_loses() {
        let input = tone(16_000.0, 44_100.0, 44_100);
        let ours = level(&resample(&input, 48_000), 16_000.0, 48_000.0);
        let old = level(&linear(&input, 48_000.0), 16_000.0, 48_000.0);
        assert!((ours - 0.5).abs() < 0.005, "16 kHz level after sinc: {ours}");
        assert!(old < 0.40, "linear should audibly lose 16 kHz, got {old}");
    }

    #[test]
    fn adds_far_less_distortion_than_linear_interpolation() {
        let input = tone(16_000.0, 44_100.0, 44_100);
        // A 16 kHz tone sampled at 44.1 kHz has an image at 28.1 kHz. Linear
        // interpolation lets it through, and at 48 kHz it folds down to 19.9 kHz.
        let alias = 48_000.0 - (44_100.0 - 16_000.0);
        let ours = level(&resample(&input, 48_000), alias, 48_000.0);
        let old = level(&linear(&input, 48_000.0), alias, 48_000.0);
        assert!(ours < 0.0005, "alias after sinc: {ours}");
        assert!(old > ours * 50.0, "linear alias {old} vs sinc {ours}");
    }

    #[test]
    fn measured_results() {
        let high = tone(16_000.0, 44_100.0, 44_100);
        let db = |level: f64| 20.0 * (level / 0.5).log10();
        let alias = 48_000.0 - (44_100.0 - 16_000.0);
        let (sinc, old) = (resample(&high, 48_000), linear(&high, 48_000.0));
        println!(
            "16 kHz level: sinc {:+.2} dB, linear {:+.2} dB",
            db(level(&sinc, 16_000.0, 48_000.0)),
            db(level(&old, 16_000.0, 48_000.0))
        );
        println!(
            "false tone at 19.9 kHz: sinc {:.1} dB, linear {:.1} dB",
            db(level(&sinc, alias, 48_000.0).max(1e-12)),
            db(level(&old, alias, 48_000.0).max(1e-12))
        );

        let minute = tone(1_000.0, 44_100.0, 44_100 * 60);
        let started = std::time::Instant::now();
        let mut resampler = StereoResampler::new(48_000).unwrap();
        for packet in minute.chunks(1_628) {
            resampler.process(packet).unwrap();
        }
        let core_percent = started.elapsed().as_secs_f64() / 60.0 * 100.0;
        println!("resampling cost: {core_percent:.2}% of one core");
        assert!(core_percent < 15.0, "too expensive for real time: {core_percent:.2}%");
    }

    #[test]
    fn output_rate_matches_the_device() {
        let input = tone(1_000.0, 44_100.0, 44_100);
        let mut resampler = StereoResampler::new(48_000).unwrap();
        let frames = resampler.process(&input).unwrap().len() / 2;
        let expected = 48_000.0;
        assert!((frames as f64 - expected).abs() < 2_000.0, "{frames} frames");
    }

    #[test]
    fn device_at_source_rate_gets_untouched_audio() {
        let input = [0.25, -0.25, 0.5, -0.5];
        let mut resampler = StereoResampler::new(SOURCE_RATE).unwrap();
        assert_eq!(resampler.process(&input).unwrap(), vec![0.25f32, -0.25, 0.5, -0.5]);
    }
}
