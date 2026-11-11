//! The songs played on this PC. Spotify's list of recently played songs leaves out what
//! Spotlight's own player plays, so Spotlight keeps a short list of its own and puts the two
//! together for Recently played. Each account has its own list, so signing in as someone else
//! never shows what the last one played.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::models::FullTrack;

/// As many as Spotify gives back, which is as many as the page shows.
pub const KEPT: usize = 50;

/// How long a song plays before it counts, as Spotify counts it.
const COUNTS_AFTER_MS: u32 = 30_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Play {
    /// When it counted, in milliseconds since 1970 (UTC).
    pub at_ms: i64,
    pub track: FullTrack,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    /// Newest first.
    plays: Vec<Play>,
}

/// Held while a list is read and written back, so two changes can't cross.
static FILES: Mutex<()> = Mutex::new(());

/// The list's file name. Without an account it's the list from before lists were kept by
/// account, which also holds plays from while the account wasn't known yet. The next account
/// signed in takes those over.
fn file_name(account: Option<&str>) -> String {
    match account {
        None => "history.json".to_string(),
        Some(account) => {
            let safe: String = account
                .chars()
                .map(|letter| if letter.is_ascii_alphanumeric() || matches!(letter, '-' | '_' | '.') { letter } else { '_' })
                .collect();
            format!("history-{safe}.json")
        }
    }
}

fn path(account: Option<&str>) -> Result<PathBuf> {
    Ok(config::config_dir()?.join(file_name(account)))
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as i64)
}

/// A missing or damaged file is an empty list.
fn read(path: &Path) -> Vec<Play> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<File>(&bytes).ok())
        .map(|file| file.plays)
        .unwrap_or_default()
}

fn write(path: &Path, plays: Vec<Play>) -> Result<()> {
    let partial = path.with_extension("json.partial");
    fs::write(&partial, serde_json::to_vec(&File { plays })?)?;
    fs::rename(&partial, path)?;
    Ok(())
}

/// Two lists as one, newest first.
fn combined(mut plays: Vec<Play>, more: Vec<Play>) -> Vec<Play> {
    plays.extend(more);
    plays.sort_by_key(|play| std::cmp::Reverse(play.at_ms));
    plays.truncate(KEPT);
    plays
}

/// Moves the plays not yet in an account's list into `account`'s.
fn claim(account: &str) -> Result<()> {
    let unclaimed = path(None)?;
    if !unclaimed.exists() {
        return Ok(());
    }
    let own = path(Some(account))?;
    write(&own, combined(read(&own), read(&unclaimed)))?;
    fs::remove_file(unclaimed)?;
    Ok(())
}

/// Gives `account` the plays recorded before it was known.
pub fn adopt(account: &str) {
    let _files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = claim(account);
}

/// The songs `account` played here, newest first.
pub fn load(account: Option<&str>) -> Vec<Play> {
    let _files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(account) = account {
        let _ = claim(account);
    }
    path(account).map(|path| read(&path)).unwrap_or_default()
}

/// Adds a song to the front of the list of the account `whose` names.
pub fn record(whose: impl FnOnce() -> Option<Option<String>>, play: Play) -> Result<()> {
    let _files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
    // Asked only now, with the files held, so the play can't go into the unclaimed list just
    // after its account claimed it. `None` leaves it out.
    let Some(account) = whose() else { return Ok(()) };
    if let Some(account) = account.as_deref() {
        let _ = claim(account);
    }
    let path = path(account.as_deref())?;
    write(&path, combined(vec![play], read(&path)))
}

/// The song the player has, and whether it has counted yet. Everything that starts or stops
/// the clock bumps `generation`, so a wait from before does nothing.
#[derive(Default)]
pub struct Watch {
    song: Option<FullTrack>,
    counted: bool,
    generation: u64,
}

impl Watch {
    /// A new song, or the same one started again.
    pub fn song_changed(&mut self, song: FullTrack) {
        self.song = Some(song);
        self.counted = false;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn stopped(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// The song is playing from `position_ms`. Gives how long to wait before it counts, and
    /// the generation to hand back to `counts`, unless it has counted already.
    pub fn playing(&mut self, uri: &str, position_ms: u32) -> Option<(Duration, u64)> {
        self.generation = self.generation.wrapping_add(1);
        let song = self.song.as_ref().filter(|song| song.uri == uri && !self.counted)?;
        // A song shorter than the usual wait counts just before its end.
        let needed = COUNTS_AFTER_MS.min((song.duration_ms.max(0) as u32).saturating_sub(1_000));
        Some((Duration::from_millis(needed.saturating_sub(position_ms) as u64), self.generation))
    }

    /// The song, if it is still playing as it was when the wait began. It counts only once.
    pub fn counts(&mut self, generation: u64) -> Option<FullTrack> {
        if generation != self.generation || self.counted {
            return None;
        }
        self.counted = true;
        self.song.clone()
    }
}

/// Spotify's recently played songs and the ones played here, newest first, with a song
/// played twice in a row listed once.
pub fn merge(from_spotify: Vec<(i64, FullTrack)>, played_here: Vec<Play>) -> Vec<FullTrack> {
    let mut all: Vec<(i64, FullTrack)> = from_spotify
        .into_iter()
        // Should Spotify list a song played here too, it's the same play: a song counts here
        // after 30 seconds, and Spotify lists it when it ends.
        .filter(|(at, song)| {
            !played_here.iter().any(|play| {
                play.track.uri == song.uri && (at - play.at_ms).abs() <= song.duration_ms.max(0) as i64 + 120_000
            })
        })
        .collect();
    all.extend(played_here.into_iter().map(|play| (play.at_ms, play.track)));
    all.sort_by(|a, b| b.0.cmp(&a.0));
    let mut songs: Vec<FullTrack> = Vec::with_capacity(all.len().min(KEPT));
    for (_, song) in all {
        if songs.last().is_some_and(|last| last.uri == song.uri) {
            continue;
        }
        songs.push(song);
        if songs.len() == KEPT {
            break;
        }
    }
    songs
}

/// Milliseconds since 1970 from a time as Spotify writes it, such as
/// `2026-11-08T22:01:28.123Z`.
pub fn parse_time_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |from: usize, to: usize| -> Option<i64> { text.get(from..to)?.parse().ok() };
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        let padded = format!("{:0<3}", &fraction[..digits.min(3)]);
        millis = padded.parse::<i64>().ok()?;
        rest = &fraction[digits..];
    }
    let offset_minutes = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest.get(1..3)?.parse::<i64>().ok()? * 60 + rest.get(4..6)?.parse::<i64>().ok()?)
        }
        _ => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970 for a date in the Gregorian calendar (Howard Hinnant's method).
    let year_from_march = if month <= 2 { year - 1 } else { year };
    let era = year_from_march.div_euclid(400);
    let year_of_era = year_from_march - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Album;

    fn song(uri: &str, duration_ms: i32) -> FullTrack {
        FullTrack {
            name: uri.into(),
            uri: uri.into(),
            duration_ms,
            album: Album { id: None, name: String::new(), release_date: None, images: Vec::new() },
            artists: Vec::new(),
        }
    }

    fn uris(songs: &[FullTrack]) -> Vec<&str> {
        songs.iter().map(|song| song.uri.as_str()).collect()
    }

    #[test]
    fn each_account_has_a_file_of_its_own() {
        assert_eq!(file_name(None), "history.json");
        assert_eq!(file_name(Some("31abcxyz")), "history-31abcxyz.json");
        // Old accounts have names, which mustn't reach outside the folder.
        assert_eq!(file_name(Some("../a b\\c")), "history-.._a_b_c.json");
    }

    #[test]
    fn unclaimed_plays_join_the_account_by_time() {
        let own = vec![Play { at_ms: 3, track: song("c", 1) }, Play { at_ms: 1, track: song("a", 1) }];
        let unclaimed = vec![Play { at_ms: 2, track: song("b", 1) }];
        let plays = combined(own, unclaimed);
        assert_eq!(plays.iter().map(|play| play.track.uri.as_str()).collect::<Vec<_>>(), ["c", "b", "a"]);
        let many = (0..KEPT as i64 + 5).map(|at_ms| Play { at_ms, track: song("x", 1) }).collect();
        assert_eq!(combined(many, Vec::new()).len(), KEPT);
    }

    #[test]
    fn times_read_as_spotify_writes_them() {
        assert_eq!(parse_time_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time_ms("2026-11-08T22:01:28.123Z"), Some(1_794_175_288_123));
        assert_eq!(parse_time_ms("2026-11-08T22:01:28Z"), Some(1_794_175_288_000));
        assert_eq!(parse_time_ms("2026-11-08T22:01:28.5Z"), Some(1_794_175_288_500));
        assert_eq!(parse_time_ms("2026-11-08T23:01:28+01:00"), Some(1_794_175_288_000));
        assert_eq!(parse_time_ms("2024-02-29T12:00:00Z"), Some(1_709_208_000_000));
        assert_eq!(parse_time_ms("yesterday"), None);
    }

    #[test]
    fn songs_played_here_go_in_among_spotifys_by_time() {
        let from_spotify = vec![(5_000_000, song("a", 200_000)), (1_000_000, song("b", 200_000))];
        let here = vec![Play { at_ms: 9_000_000, track: song("c", 200_000) }, Play { at_ms: 3_000_000, track: song("d", 200_000) }];
        assert_eq!(uris(&merge(from_spotify, here)), ["c", "a", "d", "b"]);
    }

    #[test]
    fn a_play_spotify_also_lists_shows_once() {
        // Counted here 30 s in; Spotify lists it when it ends, three minutes later.
        let from_spotify = vec![(1_210_000, song("a", 210_000)), (900_000, song("b", 200_000))];
        let here = vec![Play { at_ms: 1_030_000, track: song("a", 210_000) }];
        assert_eq!(uris(&merge(from_spotify, here)), ["a", "b"]);
    }

    #[test]
    fn the_same_song_hours_apart_shows_twice() {
        let from_spotify = vec![(10_000_000, song("a", 200_000)), (5_000_000, song("b", 200_000))];
        let here = vec![Play { at_ms: 1_000_000, track: song("a", 200_000) }];
        assert_eq!(uris(&merge(from_spotify, here)), ["a", "b", "a"]);
    }

    #[test]
    fn a_song_played_twice_in_a_row_shows_once() {
        let here = vec![Play { at_ms: 2_000_000, track: song("a", 200_000) }, Play { at_ms: 1_000_000, track: song("a", 200_000) }];
        assert_eq!(uris(&merge(Vec::new(), here)), ["a"]);
    }

    #[test]
    fn a_song_counts_after_thirty_seconds_once() {
        let mut watch = Watch::default();
        watch.song_changed(song("a", 200_000));
        let (wait, generation) = watch.playing("a", 10_000).unwrap();
        assert_eq!(wait, Duration::from_secs(20));
        assert!(watch.counts(generation).is_some());
        assert!(watch.counts(generation).is_none());
        assert!(watch.playing("a", 40_000).is_none());
    }

    #[test]
    fn a_pause_or_a_new_song_cancels_the_wait() {
        let mut watch = Watch::default();
        watch.song_changed(song("a", 200_000));
        let (_, generation) = watch.playing("a", 0).unwrap();
        watch.stopped();
        assert!(watch.counts(generation).is_none());
        let (wait, generation) = watch.playing("a", 25_000).unwrap();
        assert_eq!(wait, Duration::from_secs(5));
        watch.song_changed(song("b", 200_000));
        assert!(watch.counts(generation).is_none());
        assert!(watch.playing("a", 0).is_none());
    }

    #[test]
    fn a_short_song_counts_just_before_its_end() {
        let mut watch = Watch::default();
        watch.song_changed(song("a", 12_000));
        let (wait, _) = watch.playing("a", 0).unwrap();
        assert_eq!(wait, Duration::from_secs(11));
    }
}
