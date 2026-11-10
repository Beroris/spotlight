//! Spotlight: a song fading into the next one as it ends. See SPOTLIGHT-CHANGES.md.

use std::{
    collections::VecDeque,
    f64::consts::FRAC_PI_2,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use librespot_core::SpotifyUri;
use librespot_metadata::audio::{AudioItem, UniqueFields};

use crate::{
    NUM_CHANNELS, SAMPLE_RATE,
    decoder::{AudioDecoder, AudioPacket, AudioPacketPosition, DecoderError, DecoderResult},
};

/// How much silence at the end of a song a fade can look past. Many songs end with a few
/// seconds of it, and a fade over silence only fades the next song in after a gap.
pub(crate) const SILENT_END_MS: u32 = 8_000;
/// Reading ahead starts this much earlier again, so it is done before the fade is due.
const READ_AHEAD_MARGIN_MS: u32 = 2_000;
/// Reading ahead takes at most this long at a time, so the music before it never waits.
pub(crate) const READ_AHEAD_BUDGET: Duration = Duration::from_millis(3);
/// Quieter than this counts as silence: about -36 dB, below which the last of a reverb or a
/// fade-out can't be heard over the next song coming in.
const SILENCE: f64 = 0.016;

/// How long before a song's end to start reading ahead to find where its sound stops.
pub(crate) fn read_ahead_from_end_ms(crossfade_ms: u32) -> u32 {
    crossfade_ms + SILENT_END_MS + READ_AHEAD_MARGIN_MS
}

static CROSSFADE_MS: AtomicU32 = AtomicU32::new(0);

/// How long a song fades into the next one as it ends. Zero, the default, turns it off.
/// Takes effect from the next song change.
pub fn set_crossfade(duration: Duration) {
    CROSSFADE_MS.store(
        duration.as_millis().min(u32::MAX as u128) as u32,
        Ordering::Relaxed,
    );
}

pub(crate) fn crossfade_ms() -> u32 {
    CROSSFADE_MS.load(Ordering::Relaxed)
}

/// Whether `current` should fade into `next` as it ends. Songs shorter than two fades play
/// whole. An album played in order runs straight on, as the gaps between its songs, or the lack
/// of them, are part of it.
pub(crate) fn can_fade(
    crossfade_ms: u32,
    current: &AudioItem,
    current_duration_ms: u32,
    next: &AudioItem,
    next_duration_ms: u32,
) -> bool {
    let shortest = crossfade_ms.saturating_mul(2);
    crossfade_ms > 0
        && current_duration_ms >= shortest
        && next_duration_ms >= shortest
        && !continues_album(current, next)
}

/// The end of a song read ahead of where it is playing, to find where its sound stops. What is
/// read waits here and plays in turn, so nothing is decoded twice.
pub(crate) struct Tail {
    /// The song it was read from, so it is never played under another.
    pub(crate) play_request_id: u64,
    packets: VecDeque<(AudioPacketPosition, Vec<f32>)>,
    /// Where the last sound read so far ends.
    sound_until_ms: u32,
    /// How much audio has been read ahead, which is capped in case the song runs on.
    read_ms: u32,
    max_read_ms: u32,
    state: TailState,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum TailState {
    Reading,
    /// The whole song has been read.
    Ended,
    /// The song ran on past where it should have ended, so where its sound stops isn't known.
    GaveUp,
}

impl Tail {
    /// Starts reading ahead from `position_ms`, `remaining_ms` before the song's end.
    pub(crate) fn new(play_request_id: u64, position_ms: u32, remaining_ms: u32) -> Self {
        Self {
            play_request_id,
            packets: VecDeque::new(),
            sound_until_ms: position_ms,
            read_ms: 0,
            // The length a song is listed at can be a little short.
            max_read_ms: remaining_ms.saturating_add(5_000),
            state: TailState::Reading,
        }
    }

    /// Reads on until the song ends or `budget` runs out.
    pub(crate) fn read_ahead(&mut self, decoder: &mut dyn AudioDecoder, budget: Duration) {
        let until = Instant::now() + budget;
        while self.state == TailState::Reading && Instant::now() < until {
            if self.read_ms > self.max_read_ms {
                self.state = TailState::GaveUp;
                break;
            }
            match decoder.next_packet() {
                Ok(Some((position, AudioPacket::Samples(samples)))) => {
                    self.note(&position, &samples);
                    self.packets.push_back((
                        position,
                        samples.into_iter().map(|sample| sample as f32).collect(),
                    ));
                }
                // Passthrough isn't faded, so it is never read ahead.
                Ok(Some((_, AudioPacket::Raw(_)))) => {}
                // A song that can't be read to the end ends where reading stopped.
                Ok(None) | Err(_) => self.state = TailState::Ended,
            }
        }
    }

    /// Keeps track of where the sound read so far stops, and how much has been read.
    fn note(&mut self, position: &AudioPacketPosition, samples: &[f64]) {
        let frames = samples.len() / NUM_CHANNELS as usize;
        let length_ms = (frames as u64 * 1000 / SAMPLE_RATE as u64) as u32;
        if samples.iter().any(|sample| sample.abs() > SILENCE) {
            self.sound_until_ms = position.position_ms.saturating_add(length_ms);
        }
        self.read_ms = self.read_ms.saturating_add(length_ms);
    }

    /// The next packet to play: the next read ahead, or if playing has caught up with reading,
    /// the decoder's, still noted.
    pub(crate) fn next_packet(
        &mut self,
        decoder: &mut dyn AudioDecoder,
    ) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
        if let Some((position, samples)) = self.packets.pop_front() {
            return Ok(Some((
                position,
                AudioPacket::Samples(samples.into_iter().map(f64::from).collect()),
            )));
        }
        match self.state {
            TailState::Ended => Ok(None),
            TailState::GaveUp => decoder.next_packet(),
            TailState::Reading => {
                let next = decoder.next_packet();
                match &next {
                    Ok(Some((position, AudioPacket::Samples(samples)))) => {
                        self.note(position, samples)
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => self.state = TailState::Ended,
                }
                next
            }
        }
    }

    /// Where the song's sound stops, once it has been read to the end.
    pub(crate) fn sound_ends_ms(&self) -> Option<u32> {
        (self.state == TailState::Ended).then_some(self.sound_until_ms)
    }

    /// Where the first packet read ahead and not yet played starts, if there is one.
    pub(crate) fn first_unplayed_ms(&self) -> Option<u32> {
        self.packets.front().map(|(position, _)| position.position_ms)
    }

    /// What is left to play: what was read ahead, then the rest from `decoder`.
    pub(crate) fn into_rest(self, decoder: Box<dyn AudioDecoder + Send>) -> TailRest {
        TailRest {
            packets: self.packets,
            decoder: (self.state != TailState::Ended).then_some(decoder),
        }
    }
}

/// The rest of a song that ended early, read ahead or not, for it to fade out from.
pub(crate) struct TailRest {
    packets: VecDeque<(AudioPacketPosition, Vec<f32>)>,
    decoder: Option<Box<dyn AudioDecoder + Send>>,
}

impl AudioDecoder for TailRest {
    fn seek(&mut self, _position_ms: u32) -> Result<u32, DecoderError> {
        // Only ever played on to the end.
        Ok(0)
    }

    fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
        if let Some((position, samples)) = self.packets.pop_front() {
            return Ok(Some((
                position,
                AudioPacket::Samples(samples.into_iter().map(f64::from).collect()),
            )));
        }
        match self.decoder.as_mut() {
            Some(decoder) => decoder.next_packet(),
            None => Ok(None),
        }
    }
}

/// Whether `next` comes straight after `current` on the same album.
fn continues_album(current: &AudioItem, next: &AudioItem) -> bool {
    let (
        UniqueFields::Track {
            album,
            album_artists,
            number,
            disc_number,
            ..
        },
        UniqueFields::Track {
            album: next_album,
            album_artists: next_album_artists,
            number: next_number,
            disc_number: next_disc_number,
            ..
        },
    ) = (&current.unique_fields, &next.unique_fields)
    else {
        return false;
    };
    let follows = (next_disc_number == disc_number && *next_number == number + 1)
        || (*next_disc_number == disc_number + 1 && *next_number == 1);
    album == next_album && album_artists == next_album_artists && follows
}

/// A song that ended early so the next one can fade in over its last seconds.
pub(crate) struct FadingSong {
    decoder: Box<dyn AudioDecoder + Send>,
    pub(crate) normalisation_factor: f64,
    /// The song and request it played under, for telling Spirc if it is stopped before the next.
    pub(crate) track_id: SpotifyUri,
    pub(crate) play_request_id: u64,
    /// Set when it ended: the crossfade length, or what was left of it if that was less.
    fade_ms: u32,
    /// Decoded and not heard yet, from `read` on.
    samples: Vec<f64>,
    read: usize,
    finished: bool,
    /// The next song has been asked for. A song asked for after that takes over at once.
    pub(crate) claimed: bool,
    /// Paused before the next song started.
    pub(crate) paused: bool,
    fade: Option<Fade>,
}

impl FadingSong {
    pub(crate) fn new(
        decoder: Box<dyn AudioDecoder + Send>,
        normalisation_factor: f64,
        track_id: SpotifyUri,
        play_request_id: u64,
        fade_ms: u32,
    ) -> Self {
        Self {
            decoder,
            normalisation_factor,
            track_id,
            play_request_id,
            fade_ms,
            samples: Vec::new(),
            read: 0,
            finished: false,
            claimed: false,
            paused: false,
            fade: None,
        }
    }

    /// Before the next song starts: the next stretch of this one at full level. Empty once it has ended.
    pub(crate) fn next_alone(&mut self) -> Vec<f64> {
        if self.read < self.samples.len() {
            let rest = self.samples.split_off(self.read);
            self.samples.clear();
            self.read = 0;
            return rest;
        }
        while !self.finished {
            match self.decoder.next_packet() {
                Ok(Some((_, AudioPacket::Samples(data)))) if !data.is_empty() => return data,
                // Passthrough audio can't be mixed, so it isn't faded.
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => self.finished = true,
            }
        }
        Vec::new()
    }

    /// The next `count` samples, with silence after the song ends.
    pub(crate) fn take(&mut self, count: usize) -> Vec<f64> {
        while !self.finished && self.samples.len() - self.read < count {
            match self.decoder.next_packet() {
                Ok(Some((_, AudioPacket::Samples(data)))) => {
                    self.samples.drain(..self.read);
                    self.read = 0;
                    self.samples.extend_from_slice(&data);
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => self.finished = true,
            }
        }
        let end = (self.read + count).min(self.samples.len());
        let mut out = self.samples[self.read..end].to_vec();
        self.read = end;
        out.resize(count, 0.0);
        out
    }

    /// Mixes `old`, the next stretch of this song, into `new`, the start of the next song: this
    /// one fading out as that one fades in. Returns true once the fade is over.
    pub(crate) fn mix(&mut self, new: &mut [f64], old: &[f64]) -> bool {
        let fade_ms = self.fade_ms;
        let fade = self.fade.get_or_insert_with(|| Fade::new(fade_ms));
        fade.mix(new, old);
        fade.is_done()
    }
}

/// Two loud songs at once can add up past full scale, which the output would clip hard. In the
/// middle of a fade, peaks above `knee` bend smoothly towards full scale instead.
fn soft_clip(sample: f64, knee: f64) -> f64 {
    let level = sample.abs();
    if level <= knee || knee >= 1.0 {
        return sample;
    }
    let room = 1.0 - knee;
    sample.signum() * (knee + room * ((level - knee) / room).tanh())
}

/// Equal-power gains, so two different songs together sound about as loud as either alone.
struct Fade {
    /// In frames, a sample for each channel.
    length: usize,
    done: usize,
}

impl Fade {
    fn new(duration_ms: u32) -> Self {
        let length = (duration_ms as u64 * SAMPLE_RATE as u64 / 1000).max(1) as usize;
        Self { length, done: 0 }
    }

    /// Both interleaved, the same length.
    fn mix(&mut self, new: &mut [f64], old: &[f64]) {
        let channels = NUM_CHANNELS as usize;
        for (new_frame, old_frame) in new.chunks_mut(channels).zip(old.chunks(channels)) {
            // Exact at the end: cos(π/2) in floating point is a hair above zero.
            let (fade_in, fade_out) = if self.done >= self.length {
                (1.0, 0.0)
            } else {
                (self.done as f64 / self.length as f64 * FRAC_PI_2).sin_cos()
            };
            // How much the two overlap: nothing at either end, most half way, where the knee
            // is lowest. At the ends the clip does nothing, so nothing jumps as it starts or stops.
            let knee = 1.0 - 0.1 * (2.0 * fade_in * fade_out);
            for (new_sample, old_sample) in new_frame.iter_mut().zip(old_frame) {
                *new_sample = soft_clip(*new_sample * fade_in + *old_sample * fade_out, knee);
            }
            self.done += 1;
        }
    }

    fn is_done(&self) -> bool {
        self.done >= self.length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::{AudioPacketPosition, DecoderResult};
    use librespot_metadata::audio::AudioFiles;

    /// Hands out the given packets, then ends.
    struct Packets(std::collections::VecDeque<Vec<f64>>);

    impl AudioDecoder for Packets {
        fn seek(&mut self, _position_ms: u32) -> Result<u32, crate::decoder::DecoderError> {
            Ok(0)
        }

        fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
            Ok(self.0.pop_front().map(|data| {
                (
                    AudioPacketPosition {
                        position_ms: 0,
                        skipped: false,
                    },
                    AudioPacket::Samples(data),
                )
            }))
        }
    }

    fn track() -> SpotifyUri {
        SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC").unwrap()
    }

    /// A 1 ms fade is 44 frames at 44.1 kHz.
    fn song(packets: Vec<Vec<f64>>) -> FadingSong {
        FadingSong::new(Box::new(Packets(packets.into())), 1.0, track(), 7, 1)
    }

    fn item(album: &str, disc_number: u32, number: u32) -> AudioItem {
        AudioItem {
            track_id: track(),
            uri: String::new(),
            files: AudioFiles::default(),
            name: String::new(),
            covers: Vec::new(),
            language: Vec::new(),
            duration_ms: 200_000,
            is_explicit: false,
            availability: Ok(()),
            alternatives: None,
            unique_fields: UniqueFields::Track {
                artists: Default::default(),
                album: album.to_string(),
                album_artists: vec!["Artist".to_string()],
                popularity: 0,
                number,
                disc_number,
            },
        }
    }

    #[test]
    fn takes_samples_across_packets_and_pads_with_silence_after_the_end() {
        let mut song = song(vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0]]);
        assert_eq!(song.take(4), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(song.take(3), [5.0, 0.0, 0.0]);
        assert_eq!(song.take(2), [0.0, 0.0]);
    }

    #[test]
    fn plays_alone_from_where_it_was_then_ends() {
        let mut song = song(vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0]]);
        assert_eq!(song.take(2), [1.0, 2.0]);
        assert_eq!(song.next_alone(), [3.0, 4.0]);
        assert_eq!(song.next_alone(), [5.0, 6.0]);
        assert!(song.next_alone().is_empty());
    }

    #[test]
    fn fades_with_equal_power_and_ends_on_the_new_song_alone() {
        let mut fade = Fade::new(1);
        assert_eq!(fade.length, 44);
        // Quiet enough that the soft clip leaves it alone.
        let mut new = vec![0.5; 2 * 44];
        fade.mix(&mut new, &vec![0.5; 2 * 44]);
        // Starts on the old song alone.
        assert!((new[0] - 0.5).abs() < 1e-12);
        // Half way, each is at 1/sqrt(2): their powers add up to one.
        let half = new[2 * 22];
        assert!(
            (half - 0.5 * std::f64::consts::SQRT_2).abs() < 1e-9,
            "half way {half}"
        );
        assert!(fade.is_done());
        // After the fade, the new song plays at full level and the old one is gone.
        let mut after = vec![0.5; 4];
        fade.mix(&mut after, &[1.0; 4]);
        assert_eq!(after, [0.5; 4]);
    }

    #[test]
    fn keeps_both_channels_of_a_frame_at_one_gain() {
        let mut fade = Fade::new(1);
        let mut new = vec![1.0, -1.0, 1.0, -1.0];
        fade.mix(&mut new, &[0.0; 4]);
        assert_eq!(new[0], -new[1]);
        assert_eq!(new[2], -new[3]);
    }

    #[test]
    fn loud_songs_together_bend_below_full_scale_and_quiet_ones_pass_untouched() {
        let mut fade = Fade::new(1);
        let mut loud = vec![1.0; 2 * 44];
        fade.mix(&mut loud, &vec![1.0; 2 * 44]);
        // Without the bend, half way would reach 1.41.
        assert!(loud.iter().all(|sample| *sample <= 1.0), "{loud:?}");
        assert!(loud[2 * 22] > 0.95);

        assert_eq!(soft_clip(0.8, 0.9), 0.8);
        assert_eq!(soft_clip(-0.8, 0.9), -0.8);
        assert!(soft_clip(-3.0, 0.9) >= -1.0);
        // Nothing bends at either end of the fade.
        assert_eq!(soft_clip(0.99, 1.0), 0.99);
    }

    #[test]
    fn the_fade_length_is_fixed_when_the_song_ends_early() {
        // Changing the setting afterwards doesn't change a fade under way.
        set_crossfade(Duration::from_secs(12));
        let mut song = song(vec![vec![0.25; 2 * 50]]);
        let mut new = vec![0.5; 2 * 44];
        let old = song.take(new.len());
        assert!(song.mix(&mut new, &old), "the 1 ms fade it was given isn't over");
        set_crossfade(Duration::ZERO);
    }

    #[test]
    fn the_fade_runs_its_full_length_when_the_old_song_runs_out_first() {
        let mut song = song(vec![vec![0.25; 10]]);
        let mut new = vec![1.0; 2 * 30];
        let old = song.take(new.len());
        assert!(!song.mix(&mut new, &old), "a 44 frame fade ended after 30");
        // The new song keeps rising after the old one has ended.
        assert!(new[2 * 29] > new[2 * 10]);
        assert!(new[2 * 29] < 1.0);
    }

    #[test]
    fn never_fades_through_an_album_in_order() {
        let first = item("Album", 1, 4);
        let other = item("Other album", 1, 5);
        assert!(can_fade(6_000, &first, 200_000, &other, 180_000));
        assert!(!can_fade(0, &first, 200_000, &other, 180_000), "turned off");
        assert!(
            !can_fade(6_000, &first, 11_000, &other, 180_000),
            "this song is too short"
        );
        assert!(
            !can_fade(6_000, &first, 200_000, &other, 11_000),
            "the next song is too short"
        );
        assert!(
            !can_fade(6_000, &first, 200_000, &item("Album", 1, 5), 180_000),
            "next on the album"
        );
        assert!(
            !can_fade(6_000, &item("Album", 1, 12), 200_000, &item("Album", 2, 1), 180_000),
            "next disc"
        );
        assert!(
            can_fade(6_000, &first, 200_000, &item("Album", 1, 9), 180_000),
            "same album, out of order"
        );
    }

    /// Packets of 100 ms each, at their places in the song from `start_ms`, at the given levels.
    struct Timed {
        levels: std::collections::VecDeque<f64>,
        at_ms: u32,
    }

    const PACKET_FRAMES: usize = SAMPLE_RATE as usize / 10;

    impl AudioDecoder for Timed {
        fn seek(&mut self, position_ms: u32) -> Result<u32, DecoderError> {
            self.at_ms = position_ms;
            Ok(position_ms)
        }

        fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
            let Some(level) = self.levels.pop_front() else {
                return Ok(None);
            };
            let position = AudioPacketPosition {
                position_ms: self.at_ms,
                skipped: false,
            };
            self.at_ms += 100;
            Ok(Some((
                position,
                AudioPacket::Samples(vec![level; PACKET_FRAMES * NUM_CHANNELS as usize]),
            )))
        }
    }

    fn timed(start_ms: u32, levels: &[f64]) -> Timed {
        Timed {
            levels: levels.iter().copied().collect(),
            at_ms: start_ms,
        }
    }

    fn level(packet: DecoderResult<Option<(AudioPacketPosition, AudioPacket)>>) -> Option<(u32, f64)> {
        match packet.unwrap() {
            Some((position, AudioPacket::Samples(samples))) => Some((position.position_ms, samples[0])),
            _ => None,
        }
    }

    #[test]
    fn finds_where_the_sound_stops_before_a_silent_end() {
        // Sound for 300 ms, then 500 ms of near silence to the end.
        let mut decoder = timed(10_000, &[0.5, 0.375, 0.25, 0.001, 0.0, 0.0, 0.0, 0.0]);
        let mut tail = Tail::new(3, 10_000, 800);
        assert_eq!(tail.sound_ends_ms(), None, "nothing read yet");
        tail.read_ahead(&mut decoder, Duration::from_secs(5));
        assert_eq!(tail.sound_ends_ms(), Some(10_300));
        // What was read plays in order, and then the song ends.
        assert_eq!(tail.first_unplayed_ms(), Some(10_000));
        assert_eq!(level(tail.next_packet(&mut decoder)), Some((10_000, 0.5)));
        assert_eq!(level(tail.next_packet(&mut decoder)), Some((10_100, 0.375)));
        for _ in 0..6 {
            assert!(level(tail.next_packet(&mut decoder)).is_some());
        }
        assert_eq!(level(tail.next_packet(&mut decoder)), None);
    }

    #[test]
    fn a_song_that_never_goes_quiet_has_its_sound_to_the_end() {
        let mut decoder = timed(0, &[0.5; 5]);
        let mut tail = Tail::new(3, 0, 500);
        tail.read_ahead(&mut decoder, Duration::from_secs(5));
        assert_eq!(tail.sound_ends_ms(), Some(500));
    }

    #[test]
    fn playing_that_catches_up_with_reading_still_counts_the_sound() {
        let mut decoder = timed(0, &[0.5, 0.5, 0.0]);
        let mut tail = Tail::new(3, 0, 300);
        // No time to read ahead at all: each packet comes straight from the decoder.
        tail.read_ahead(&mut decoder, Duration::ZERO);
        assert_eq!(level(tail.next_packet(&mut decoder)), Some((0, 0.5)));
        assert_eq!(level(tail.next_packet(&mut decoder)), Some((100, 0.5)));
        assert_eq!(level(tail.next_packet(&mut decoder)), Some((200, 0.0)));
        assert_eq!(tail.sound_ends_ms(), None, "the end isn't known until the song ends");
        assert_eq!(level(tail.next_packet(&mut decoder)), None);
        assert_eq!(tail.sound_ends_ms(), Some(200));
    }

    #[test]
    fn a_song_running_past_its_listed_length_is_played_on_without_a_known_end() {
        // Listed as 200 ms from here, but 6 s long.
        let mut decoder = timed(0, &[0.5; 60]);
        let mut tail = Tail::new(3, 0, 200);
        tail.read_ahead(&mut decoder, Duration::from_secs(5));
        assert_eq!(tail.sound_ends_ms(), None);
        // Everything still plays, in order: what was read, then the rest from the decoder.
        let mut at = Vec::new();
        while let Some((position_ms, _)) = level(tail.next_packet(&mut decoder)) {
            at.push(position_ms);
        }
        assert_eq!(at, (0..60).map(|i| i * 100).collect::<Vec<_>>());
    }

    #[test]
    fn what_is_left_plays_out_for_the_fade_then_the_rest_of_the_song() {
        // 6 s from here, more than is read ahead, so some is still in the decoder.
        let mut decoder = timed(0, &[0.5; 60]);
        let mut tail = Tail::new(3, 0, 0);
        tail.read_ahead(&mut decoder, Duration::from_secs(5));
        assert_eq!(level(tail.next_packet(&mut decoder)), Some((0, 0.5)));
        let mut rest = tail.into_rest(Box::new(decoder));
        let mut at = Vec::new();
        while let Some((position_ms, _)) = level(rest.next_packet()) {
            at.push(position_ms);
        }
        assert_eq!(at, (1..60).map(|i| i * 100).collect::<Vec<_>>());
    }
}
