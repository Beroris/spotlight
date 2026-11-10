//! Where the music was left: the song, how far in, what comes next, and shuffle and repeat.
//! Spotify forgets it once Spotlight's player closes, so Spotlight keeps it and shows it paused
//! the next time it opens, ready to carry on.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::models::FullTrack;

/// Enough of the list for hours of music, and still a small file.
const REST_KEPT: usize = 1_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LeftOff {
    pub track: FullTrack,
    pub position_ms: i32,
    /// What Next up showed, in its order.
    #[serde(default)]
    pub upcoming: Vec<FullTrack>,
    /// The rest of the list it was playing from, after those, so the music doesn't stop once
    /// Next up runs out.
    #[serde(default)]
    pub rest: Vec<String>,
    /// Where the list came from, such as a playlist.
    #[serde(default)]
    pub list_uri: String,
    #[serde(default)]
    pub shuffle: bool,
    /// "off", "context" or "track", as Spotify names them.
    #[serde(default)]
    pub repeat: String,
}

fn path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join("left-off.json"))
}

/// What was written last, so an unchanged state isn't written again. Also keeps two writes
/// from crossing.
static WRITTEN: Mutex<Vec<u8>> = Mutex::new(Vec::new());

pub fn load() -> Option<LeftOff> {
    let bytes = fs::read(path().ok()?).ok()?;
    let left_off = serde_json::from_slice::<LeftOff>(&bytes).ok()?;
    *WRITTEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = bytes;
    Some(left_off).filter(|left_off| !left_off.track.uri.is_empty())
}

pub fn save(left_off: &LeftOff) -> Result<()> {
    let bytes = serde_json::to_vec(left_off)?;
    let mut written = WRITTEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *written == bytes {
        return Ok(());
    }
    let path = path()?;
    let partial = path.with_extension("json.partial");
    fs::write(&partial, &bytes)?;
    fs::rename(&partial, &path)?;
    *written = bytes;
    Ok(())
}

/// After signing out: the next account shouldn't open on this one's music.
pub fn forget() {
    let mut written = WRITTEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Ok(path) = path() {
        let _ = fs::remove_file(path);
    }
    written.clear();
}

pub fn save_in_background(left_off: LeftOff) {
    std::thread::spawn(move || {
        let _ = save(&left_off);
    });
}

/// The songs of the list after the ones Next up shows. In shuffle that's the list's other
/// songs in any order, which the player shuffles anyway. Nothing when the song isn't from
/// that list, as the list is then something played earlier.
pub fn rest_of_list(list: &[FullTrack], current: &str, upcoming: &[FullTrack], shuffled: bool) -> Vec<String> {
    let shown = |uri: &str| uri == current || upcoming.iter().any(|track| track.uri == uri);
    let Some(current_at) = list.iter().position(|track| track.uri == current) else {
        return Vec::new();
    };
    let after = if shuffled {
        0
    } else {
        // Songs added to the queue aren't in the list, so it carries on after the last one
        // that is.
        let last_shown = upcoming.iter().rev().find_map(|song| list.iter().position(|track| track.uri == song.uri));
        last_shown.unwrap_or(current_at) + 1
    };
    list.iter()
        .skip(after)
        .filter(|track| !shown(&track.uri))
        .take(REST_KEPT)
        .map(|track| track.uri.clone())
        .collect()
}

/// Everything to play, in order: the song, Next up, then the rest of the list.
pub fn play_order(left_off: &LeftOff, upcoming: &[FullTrack]) -> Vec<String> {
    let mut uris = Vec::with_capacity(1 + upcoming.len() + left_off.rest.len());
    uris.push(left_off.track.uri.clone());
    uris.extend(upcoming.iter().map(|track| track.uri.clone()));
    uris.extend(left_off.rest.iter().filter(|uri| !upcoming.iter().any(|track| &track.uri == *uri)).cloned());
    uris
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Album;

    fn song(uri: &str) -> FullTrack {
        FullTrack {
            name: uri.into(),
            uri: uri.into(),
            duration_ms: 200_000,
            album: Album { id: None, name: String::new(), release_date: None, images: Vec::new() },
            artists: Vec::new(),
        }
    }

    fn songs(uris: &[&str]) -> Vec<FullTrack> {
        uris.iter().map(|uri| song(uri)).collect()
    }

    #[test]
    fn the_list_carries_on_after_the_last_song_shown() {
        let list = songs(&["a", "b", "c", "d", "e", "f"]);
        assert_eq!(rest_of_list(&list, "b", &songs(&["c", "d"]), false), ["e", "f"]);
    }

    #[test]
    fn songs_added_to_the_queue_dont_lose_the_place_in_the_list() {
        let list = songs(&["a", "b", "c", "d", "e"]);
        assert_eq!(rest_of_list(&list, "b", &songs(&["c", "queued"]), false), ["d", "e"]);
        assert_eq!(rest_of_list(&list, "b", &songs(&["queued"]), false), ["c", "d", "e"]);
    }

    #[test]
    fn in_shuffle_the_rest_is_every_song_not_shown() {
        let list = songs(&["a", "b", "c", "d", "e"]);
        assert_eq!(rest_of_list(&list, "c", &songs(&["e", "a"]), true), ["b", "d"]);
    }

    #[test]
    fn a_song_from_somewhere_else_has_no_rest() {
        let list = songs(&["a", "b", "c"]);
        assert!(rest_of_list(&list, "elsewhere", &songs(&["b"]), false).is_empty());
    }

    #[test]
    fn the_order_is_the_song_then_next_up_then_the_rest() {
        let left_off = LeftOff { track: song("a"), rest: vec!["d".into(), "e".into()], ..Default::default() };
        assert_eq!(play_order(&left_off, &songs(&["c", "b"])), ["a", "c", "b", "d", "e"]);
        // A song dragged up from the rest isn't played twice.
        assert_eq!(play_order(&left_off, &songs(&["e", "c"])), ["a", "e", "c", "d"]);
    }

    #[test]
    fn it_reads_back_what_it_wrote() {
        let left_off = LeftOff {
            track: song("a"),
            position_ms: 61_000,
            upcoming: songs(&["b"]),
            rest: vec!["c".into()],
            list_uri: "spotify:playlist:x".into(),
            shuffle: true,
            repeat: "context".into(),
        };
        let bytes = serde_json::to_vec(&left_off).unwrap();
        assert_eq!(serde_json::from_slice::<LeftOff>(&bytes).unwrap(), left_off);
    }
}
