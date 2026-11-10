//! A song's lyrics, from the same service Spotify's own apps use, over the connection that
//! plays the music. It isn't a public service and could change without notice: anything
//! unexpected becomes "no lyrics" or an error message, never a crash.

use anyhow::{anyhow, Result};
use librespot::core::{Session, SpotifyId};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Method,
};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// When the line starts, in the song. Zero for every line when not synced.
    pub start_ms: u32,
    pub words: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lyrics {
    /// Lines come with their times, so the one being sung can be shown.
    pub synced: bool,
    pub lines: Vec<Line>,
    /// Who the lyrics come from, to credit them, such as "Musixmatch".
    pub provider: String,
}

impl Lyrics {
    /// The line being sung at `position_ms`, if any has started yet.
    pub fn line_at(&self, position_ms: u32) -> Option<usize> {
        if !self.synced {
            return None;
        }
        self.lines.partition_point(|line| line.start_ms <= position_ms).checked_sub(1)
    }

    /// When the line after the one at `position_ms` starts.
    pub fn next_start_after(&self, position_ms: u32) -> Option<u32> {
        if !self.synced {
            return None;
        }
        let next = self.lines.partition_point(|line| line.start_ms <= position_ms);
        self.lines.get(next).map(|line| line.start_ms)
    }
}

/// Reads Spotify's answer. `None` when it has lines but no words worth showing.
pub fn parse(json: &[u8]) -> Result<Option<Lyrics>> {
    let value: Value = serde_json::from_slice(json)?;
    let lyrics = value.get("lyrics").ok_or_else(|| anyhow!("no lyrics in the answer"))?;
    let synced = lyrics.get("syncType").and_then(Value::as_str) == Some("LINE_SYNCED");
    let number = |field: Option<&Value>| -> u32 {
        match field {
            Some(Value::String(text)) => text.parse().unwrap_or(0),
            Some(Value::Number(number)) => number.as_u64().unwrap_or(0).min(u64::from(u32::MAX)) as u32,
            _ => 0,
        }
    };
    let mut lines: Vec<Line> = lyrics
        .get("lines")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|line| {
            let words = line.get("words").and_then(Value::as_str).unwrap_or_default().trim();
            Line {
                start_ms: if synced { number(line.get("startTimeMs")) } else { 0 },
                // A gap in the singing comes as an empty line or a note.
                words: if words.is_empty() { "♪".to_string() } else { words.to_string() },
            }
        })
        .collect();
    if synced {
        // In order, so the line being sung can be found by halving.
        lines.sort_by_key(|line| line.start_ms);
    }
    // Two gaps in a row say nothing more than one.
    lines.dedup_by(|line, before| line.words == "♪" && before.words == "♪");
    if lines.iter().all(|line| line.words == "♪") {
        return Ok(None);
    }
    let provider = lyrics
        .get("providerDisplayName")
        .or_else(|| lyrics.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(Some(Lyrics { synced, lines, provider }))
}

/// The lyrics for a song, or `None` if Spotify has none for it.
pub async fn fetch(session: &Session, song_uri: &str) -> Result<Option<Lyrics>> {
    let id = song_uri.strip_prefix("spotify:track:").ok_or_else(|| anyhow!("not a song"))?;
    let track = SpotifyId::from_base62(id)?;
    let spclient = session.spclient();
    let answer = match spclient.get_lyrics(&track).await {
        Ok(answer) => answer,
        Err(error) if is_not_found(&error.to_string()) => return Ok(None),
        // Asked the way the web player asks, which some accounts need.
        Err(_) => {
            let mut headers = HeaderMap::new();
            headers.insert("app-platform", HeaderValue::from_static("WebPlayer"));
            let endpoint = format!("/color-lyrics/v2/track/{id}?format=json&vocalRemoval=false&market=from_token");
            match spclient.request_as_json(&Method::GET, &endpoint, Some(headers), None).await {
                Ok(answer) => answer,
                Err(error) if is_not_found(&error.to_string()) => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
    };
    if answer.is_empty() {
        return Ok(None);
    }
    parse(&answer)
}

fn is_not_found(error: &str) -> bool {
    error.contains("404") || error.to_ascii_lowercase().contains("not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced() -> Lyrics {
        parse(r#"{"lyrics":{"syncType":"LINE_SYNCED","provider":"MusixMatch","providerDisplayName":"Musixmatch","lines":[
            {"startTimeMs":"12000","words":"Second line","endTimeMs":"0"},
            {"startTimeMs":"1500","words":"First line","endTimeMs":"0"},
            {"startTimeMs":"20000","words":"","endTimeMs":"0"},
            {"startTimeMs":"21000","words":"♪","endTimeMs":"0"},
            {"startTimeMs":"30000","words":"  Last line ","endTimeMs":"0"}]}}"#.as_bytes())
        .unwrap()
        .unwrap()
    }

    #[test]
    fn reads_synced_lines_in_order_with_gaps_shown_once() {
        let lyrics = synced();
        assert!(lyrics.synced);
        assert_eq!(lyrics.provider, "Musixmatch");
        let words: Vec<_> = lyrics.lines.iter().map(|line| line.words.as_str()).collect();
        assert_eq!(words, ["First line", "Second line", "♪", "Last line"]);
        assert_eq!(lyrics.lines[0].start_ms, 1500);
    }

    #[test]
    fn finds_the_line_being_sung_and_when_the_next_starts() {
        let lyrics = synced();
        assert_eq!(lyrics.line_at(0), None, "before the singing starts");
        assert_eq!(lyrics.next_start_after(0), Some(1500));
        assert_eq!(lyrics.line_at(1500), Some(0));
        assert_eq!(lyrics.line_at(11_999), Some(0));
        assert_eq!(lyrics.next_start_after(11_999), Some(12_000));
        assert_eq!(lyrics.line_at(12_000), Some(1));
        assert_eq!(lyrics.line_at(400_000), Some(3));
        assert_eq!(lyrics.next_start_after(400_000), None, "after the last line");
    }

    #[test]
    fn unsynced_lyrics_have_no_line_being_sung() {
        let lyrics = parse(br#"{"lyrics":{"syncType":"UNSYNCED","lines":[{"startTimeMs":"0","words":"Just words"}]}}"#)
            .unwrap()
            .unwrap();
        assert!(!lyrics.synced);
        assert_eq!(lyrics.line_at(5000), None);
        assert_eq!(lyrics.next_start_after(5000), None);
    }

    #[test]
    fn nothing_but_gaps_is_no_lyrics() {
        assert_eq!(parse(br#"{"lyrics":{"syncType":"LINE_SYNCED","lines":[{"startTimeMs":"0","words":""}]}}"#).unwrap(), None);
        assert!(parse(br#"{"error":"nope"}"#).is_err());
    }

    /// Reads one song's lyrics from Spotify, and never plays anything:
    /// `SPOTLIGHT_PROBE_TOKEN=... SPOTLIGHT_PROBE_SONG=spotify:track:... cargo test real_lyrics -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a Spotify login"]
    async fn real_lyrics_can_be_read() {
        use librespot::core::{authentication::Credentials, SessionConfig};

        let token = std::env::var("SPOTLIGHT_PROBE_TOKEN").expect("token");
        let song = std::env::var("SPOTLIGHT_PROBE_SONG").expect("song");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(Credentials::with_access_token(token), false).await.unwrap();
        let lyrics = fetch(&session, &song).await.unwrap();
        match lyrics {
            Some(lyrics) => println!(
                "{} lines, synced: {}, from {}; first: {:?}",
                lyrics.lines.len(),
                lyrics.synced,
                lyrics.provider,
                lyrics.lines.first()
            ),
            None => println!("no lyrics"),
        }
    }
}
