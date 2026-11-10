//! Spotify's Home page for this account: its mixes, stations and recommendations, and
//! the songs inside them.
//!
//! The public Web API refuses all of this to apps like this one, so it comes over the
//! connection that plays the music. These are Spotify's private services and could
//! change without notice. Everything here only reads, and anything unexpected becomes a
//! missing card or an error message.

use std::{path::Path, sync::Arc};

use anyhow::{anyhow, Result};
use librespot::{
    core::{Session, SpotifyUri},
    metadata::{Artist, Metadata, Track},
};
use reqwest::Method;
use serde_json::Value;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::models::{self, AlbumImage, FullTrack};

/// More songs than this in one list takes too long to look up, and nobody's mix is this long.
const MAX_SONGS: usize = 300;
/// Song details are fetched one at a time, so a few run together.
const LOOKUPS_AT_ONCE: usize = 8;

// Spotify sends 20-odd rows, and Home shows only these. They go by id because the titles
// change with the language and the owner's name.
const SHORTCUTS: &str = "0JQ5DAIiKWzVFULQfUm85Y";
const FAVORITE_ARTISTS: &str = "0JQ5DAnM3wGh0gz1MXnu3z";
/// After the shortcuts and artists, in page order.
const OTHER_ROWS: [&str; 3] = [
    "0JQ5DAnM3wGh0gz1MXnu89", // Your top mixes
    "0JQ5DAUnp4wcj0bCb3wh3S", // Made For you: Daily Mixes, Discover Weekly, Release Radar
    "0JQ5DAnM3wGh0gz1MXnu3R", // Recommended Stations
];
const ARTISTS_ROW: &str = "Your artists";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardKind {
    Playlist,
    Album,
    Artist,
    LikedSongs,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HomeCard {
    pub title: String,
    pub subtitle: String,
    pub uri: String,
    pub image_url: Option<String>,
    pub kind: CardKind,
}

impl HomeCard {
    /// What the list page calls this.
    pub fn kicker(&self) -> &'static str {
        match self.kind {
            CardKind::Artist => "Artist",
            CardKind::Album => "Album",
            _ if self.title.ends_with("Radio") => "Radio",
            _ if self.title.contains("Mix") => "Mix",
            _ => "Playlist",
        }
    }

    pub fn id(&self) -> &str {
        self.uri.rsplit(':').next().unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HomeSection {
    pub title: String,
    pub cards: Vec<HomeCard>,
    /// The quick shortcuts at the top, shown as small tiles.
    pub tiles: bool,
}

fn kind_of(uri: &str) -> Option<CardKind> {
    if uri.ends_with(":collection") {
        Some(CardKind::LikedSongs)
    } else if uri.starts_with("spotify:playlist:") {
        Some(CardKind::Playlist)
    } else if uri.starts_with("spotify:album:") {
        Some(CardKind::Album)
    } else if uri.starts_with("spotify:artist:") {
        Some(CardKind::Artist)
    } else {
        // Podcasts and the like, which Spotlight doesn't play.
        None
    }
}

/// Covers come at up to 640 px, far more than a card shows.
fn smaller_image_url(url: &str) -> String {
    const SIZES: [(&str, &str); 2] = [
        // Album: 640 px to 300 px.
        ("ab67616d0000b273", "ab67616d00001e02"),
        // Artist: 640 px to 320 px.
        ("ab6761610000e5eb", "ab67616100005174"),
    ];
    for (large, small) in SIZES {
        if url.contains(large) {
            return url.replacen(large, small, 1);
        }
    }
    url.to_string()
}

/// A picture's address. A playlist the owner made often has a mosaic of four album covers
/// instead, named by the covers' ids, which Spotify's image server draws from those ids.
fn image_url_from(uri: &str) -> Option<String> {
    if uri.starts_with("https://") {
        return Some(smaller_image_url(uri));
    }
    let covers: Vec<&str> = uri.strip_prefix("spotify:mosaic:")?.split(':').collect();
    let all_ids = covers.iter().all(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    (covers.len() == 4 && all_ids).then(|| format!("https://mosaic.scdn.co/300/{}", covers.concat()))
}

fn card_from(entry: &Value) -> Option<HomeCard> {
    let uri = entry.pointer("/target/uri")?.as_str()?.to_string();
    let kind = kind_of(&uri)?;
    let title = entry.pointer("/text/title")?.as_str()?.trim().to_string();
    if title.is_empty() {
        return None;
    }
    let subtitle = entry.pointer("/text/subtitle").and_then(Value::as_str).unwrap_or_default().to_string();
    // The AI DJ needs Spotify's own apps, and has no songs to list.
    if kind == CardKind::Playlist && title == "DJ" && subtitle.contains("DJ") {
        return None;
    }
    let image_url = entry
        .pointer("/images/main/uri")
        .and_then(Value::as_str)
        .and_then(image_url_from);
    Some(HomeCard { title, subtitle, uri, image_url, kind })
}

fn add_new_cards(cards: &mut Vec<HomeCard>, more: impl IntoIterator<Item = HomeCard>) {
    for card in more {
        // Liked Songs can come twice, under two different addresses.
        let both_liked = |known: &HomeCard| known.kind == CardKind::LikedSongs && card.kind == CardKind::LikedSongs;
        if !cards.iter().any(|known| known.uri == card.uri || both_liked(known)) {
            cards.push(card);
        }
    }
}

/// Keeps the rows Home shows. Artists share one row: those in the shortcuts, which are
/// the ones being played now, then Spotify's favourites.
fn choose_rows(mut found: Vec<(String, HomeSection)>) -> Vec<HomeSection> {
    let mut take = |id: &str| found.iter().position(|(section, _)| section.ends_with(id)).map(|at| found.remove(at).1);
    let shortcuts = take(SHORTCUTS);
    let favorites = take(FAVORITE_ARTISTS);
    let others: Vec<HomeSection> = OTHER_ROWS.iter().filter_map(|id| take(id)).collect();

    if shortcuts.is_none() && favorites.is_none() && others.is_empty() {
        // Spotify has changed its ids. Its first few rows are better than an empty page.
        let mut rows: Vec<HomeSection> = found.into_iter().take(2 + OTHER_ROWS.len()).map(|(_, section)| section).collect();
        if let Some(first) = rows.first_mut() {
            first.tiles = true;
        }
        return rows;
    }

    let mut artists = Vec::new();
    add_new_cards(&mut artists, shortcuts.iter().flat_map(|section| &section.cards).filter(|card| card.kind == CardKind::Artist).cloned());
    add_new_cards(&mut artists, favorites.into_iter().flat_map(|section| section.cards));

    let mut rows = Vec::new();
    rows.extend(shortcuts.map(|section| HomeSection { tiles: true, ..section }));
    if !artists.is_empty() {
        rows.push(HomeSection { title: ARTISTS_ROW.to_string(), cards: artists, tiles: false });
    }
    rows.extend(others);
    rows
}

/// Adds the artists this account follows to the end of the artists' row.
pub fn add_followed_artists(sections: &mut Vec<HomeSection>, followed: Vec<HomeCard>) {
    if followed.is_empty() {
        return;
    }
    let row = sections.iter().position(|section| section.title == ARTISTS_ROW).unwrap_or_else(|| {
        let after_shortcuts = usize::from(sections.first().is_some_and(|section| section.tiles));
        sections.insert(after_shortcuts, HomeSection { title: ARTISTS_ROW.to_string(), cards: Vec::new(), tiles: false });
        after_shortcuts
    });
    add_new_cards(&mut sections[row].cards, followed);
}

/// The rows of the Home page. Cards before any heading are left out: they repeat the shortcuts.
pub fn parse_home(json: &[u8]) -> Result<Vec<HomeSection>> {
    let page: Value = serde_json::from_slice(json)?;
    let body = page.get("body").and_then(Value::as_array).ok_or_else(|| anyhow!("Home page has no content"))?;

    let mut sections: Vec<(String, HomeSection)> = Vec::new();
    // A card belongs to the heading its id starts with.
    let mut current: Option<&str> = None;
    for entry in body {
        let component = entry.pointer("/component/id").and_then(Value::as_str).unwrap_or_default();
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        if component.ends_with("sectionHeader") {
            let title = entry.pointer("/text/title").and_then(Value::as_str).unwrap_or_default().trim();
            current = id.strip_suffix("-header");
            sections.push((current.unwrap_or_default().to_string(), HomeSection { title: title.to_string(), cards: Vec::new(), tiles: false }));
        } else if component.ends_with(":card") {
            let belongs = current.is_some_and(|section| id.split("_card").next() == Some(section));
            if !belongs {
                current = None;
                continue;
            }
            if let (Some((_, section)), Some(card)) = (sections.last_mut(), card_from(entry)) {
                add_new_cards(&mut section.cards, [card]);
            }
        }
    }
    sections.retain(|(_, section)| !section.title.is_empty() && !section.cards.is_empty());
    Ok(choose_rows(sections))
}

pub async fn fetch_home(session: &Session) -> Result<Vec<HomeSection>> {
    let page = session.spclient().request_as_json(&Method::GET, "/homeview/v1/home", None, None).await?;
    parse_home(&page)
}

fn is_song(uri: &str) -> bool {
    uri.starts_with("spotify:track:")
}

/// The songs in a playlist, mix or album, in order.
pub async fn context_song_uris(session: &Session, uri: &str) -> Result<Vec<String>> {
    let spclient = session.spclient();
    let context = spclient.get_context(uri).await?;
    let mut uris = Vec::new();
    let mut next_page = String::new();
    for page in &context.pages {
        uris.extend(page.tracks.iter().map(|track| track.uri().to_string()).filter(|uri| is_song(uri)));
        if next_page.is_empty() {
            next_page = page.next_page_url().to_string();
        }
    }
    while !next_page.is_empty() && uris.len() < MAX_SONGS {
        let page: Value = serde_json::from_slice(&spclient.get_next_page(&next_page).await?)?;
        let tracks = page.get("tracks").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        uris.extend(tracks.iter().filter_map(|track| track.get("uri")?.as_str()).filter(|uri| is_song(uri)).map(String::from));
        next_page = page.get("next_page_url").and_then(Value::as_str).unwrap_or_default().to_string();
    }
    uris.truncate(MAX_SONGS);
    Ok(uris)
}

/// The playlist Spotify makes as a song's radio: songs like it, starting with it.
pub async fn song_radio_uri(session: &Session, song_uri: &str) -> Result<String> {
    let answer = session.spclient().get_radio_for_track(&SpotifyUri::from_uri(song_uri)?).await?;
    radio_playlist_in(&serde_json::from_slice(&answer)?).ok_or_else(|| anyhow!("Spotify has no radio for this song"))
}

/// The first playlist link anywhere in an answer, which is how the radio's comes back.
fn radio_playlist_in(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if text.starts_with("spotify:playlist:") => Some(text.clone()),
        Value::Array(items) => items.iter().find_map(radio_playlist_in),
        Value::Object(fields) => fields.values().find_map(radio_playlist_in),
        _ => None,
    }
}

/// An artist's most played songs.
pub async fn artist_top_song_uris(session: &Session, artist_id: &str) -> Result<Vec<String>> {
    let artist = Artist::get(session, &SpotifyUri::from_uri(&format!("spotify:artist:{artist_id}"))?).await?;
    let country = session.country();
    let groups = &artist.top_tracks.0;
    let group = groups.iter().find(|group| group.country == country).or(groups.first());
    Ok(group
        .map(|group| group.tracks.0.iter().filter_map(|track| track.to_uri().ok()).collect())
        .unwrap_or_default())
}

fn id_of(uri: &SpotifyUri) -> Option<String> {
    Some(uri.to_uri().ok()?.rsplit(':').next()?.to_string())
}

fn full_track(uri: String, track: Track) -> FullTrack {
    let mut covers: Vec<_> = track.album.covers.0.iter().collect();
    // The way the Web API lists them: largest first.
    covers.sort_by_key(|cover| std::cmp::Reverse(cover.width));
    let date = &track.album.date;
    FullTrack {
        name: track.name,
        uri,
        duration_ms: track.duration,
        album: models::Album {
            id: id_of(&track.album.id),
            name: track.album.name,
            release_date: (date.year() > 1900).then(|| format!("{:04}-{:02}-{:02}", date.year(), u8::from(date.month()), date.day())),
            images: covers
                .into_iter()
                .filter_map(|cover| Some(AlbumImage { url: format!("https://i.scdn.co/image/{}", cover.id.to_base16().ok()?) }))
                .collect(),
        },
        artists: track.artists.0.iter().map(|artist| models::Artist { id: id_of(&artist.id), name: artist.name.clone() }).collect(),
    }
}

/// Full details for each song, in the order given. A song that can't be looked up is left out.
pub async fn songs_by_uri(session: &Session, uris: &[String]) -> Vec<FullTrack> {
    let slots = Arc::new(Semaphore::new(LOOKUPS_AT_ONCE));
    let mut lookups = JoinSet::new();
    for (index, uri) in uris.iter().cloned().enumerate() {
        let (session, slots) = (session.clone(), Arc::clone(&slots));
        lookups.spawn(async move {
            let _slot = slots.acquire_owned().await.ok()?;
            let track = Track::get(&session, &SpotifyUri::from_uri(&uri).ok()?).await.ok()?;
            Some((index, full_track(uri, track)))
        });
    }
    let mut found = Vec::with_capacity(uris.len());
    while let Some(lookup) = lookups.join_next().await {
        if let Ok(Some(song)) = lookup {
            found.push(song);
        }
    }
    found.sort_by_key(|(index, _)| *index);
    found.into_iter().map(|(_, song)| song).collect()
}

/// Reads a cover at a size that suits a card, as width, height and RGBA pixels.
pub fn thumbnail(path: &Path, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let cover = image::load_from_memory(&std::fs::read(path).ok()?).ok()?;
    let pixels = if cover.width() > size || cover.height() > size { cover.thumbnail(size, size) } else { cover }.into_rgba8();
    Some((pixels.width(), pixels.height(), pixels.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn header(section: &str, title: &str) -> Value {
        json!({ "id": format!("spotify:section:{section}-header"), "component": { "id": "glue:sectionHeader" }, "text": { "title": title } })
    }

    fn card(section: &str, number: u32, title: &str, subtitle: &str, uri: &str, image: &str) -> Value {
        json!({
            "id": format!("spotify:section:{section}_card{number}"),
            "component": { "id": "glue2:card" },
            "text": { "title": title, "subtitle": subtitle },
            "images": { "main": { "uri": image } },
            "target": { "uri": uri },
        })
    }

    fn page(body: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({ "id": "home", "body": body })).unwrap()
    }

    #[test]
    fn reads_sections_in_order_with_each_kind_of_card() {
        let json = page(vec![
            header("A", "Shortcuts"),
            card("A", 0, "Jessie Reyez Mix", "Cil and Mahalia", "spotify:playlist:mix", "https://pickasso.spotifycdn.com/image/x"),
            card("A", 1, "Camila Cabello", "", "spotify:artist:camila", "https://i.scdn.co/image/ab6761610000e5ebaa"),
            card("A", 2, "Deadbeat", "Tame Impala", "spotify:album:dead", "https://i.scdn.co/image/ab67616d0000b273bb"),
            card("A", 3, "Liked Songs", "", "spotify:user:someone:collection", "https://misc.scdn.co/liked"),
            header("B", "Your top mixes"),
            card("B", 0, "2000s Mix", "Timbaland", "spotify:playlist:two", "https://pickasso.spotifycdn.com/image/y"),
        ]);
        let sections = parse_home(&json).unwrap();

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "Shortcuts");
        assert!(sections[0].tiles, "the first section is the shortcut tiles");
        assert!(!sections[1].tiles);
        let kinds: Vec<CardKind> = sections[0].cards.iter().map(|card| card.kind).collect();
        assert_eq!(kinds, [CardKind::Playlist, CardKind::Artist, CardKind::Album, CardKind::LikedSongs]);
        assert_eq!(sections[0].cards[0].subtitle, "Cil and Mahalia");
    }

    #[test]
    fn asks_for_smaller_covers_where_spotify_has_them() {
        let json = page(vec![
            header("A", "Shortcuts"),
            card("A", 0, "Camila Cabello", "", "spotify:artist:camila", "https://i.scdn.co/image/ab6761610000e5ebaa"),
            card("A", 1, "Deadbeat", "", "spotify:album:dead", "https://i.scdn.co/image/ab67616d0000b273bb"),
            card("A", 2, "A Mix", "", "spotify:playlist:m", "https://pickasso.spotifycdn.com/image/x"),
            card("A", 3, "No picture", "", "spotify:playlist:n", "spotify:image:abc"),
            card("A", 4, "My Own", "", "spotify:playlist:o", "spotify:mosaic:aa11:bb22:cc33:dd44"),
            card("A", 5, "Odd mosaic", "", "spotify:playlist:p", "spotify:mosaic:aa11:bb22"),
            card("A", 6, "Not ids", "", "spotify:playlist:q", "spotify:mosaic:aa11:bb22:cc33:../x"),
        ]);
        let cards = &parse_home(&json).unwrap()[0].cards;
        assert_eq!(cards[0].image_url.as_deref(), Some("https://i.scdn.co/image/ab67616100005174aa"));
        assert_eq!(cards[1].image_url.as_deref(), Some("https://i.scdn.co/image/ab67616d00001e02bb"));
        assert_eq!(cards[2].image_url.as_deref(), Some("https://pickasso.spotifycdn.com/image/x"));
        assert_eq!(cards[3].image_url, None);
        assert_eq!(cards[4].image_url.as_deref(), Some("https://mosaic.scdn.co/300/aa11bb22cc33dd44"));
        assert_eq!(cards[5].image_url, None);
        assert_eq!(cards[6].image_url, None);
    }

    #[test]
    fn leaves_out_what_spotlight_cannot_play_and_repeats() {
        let json = page(vec![
            header("A", "Shortcuts"),
            card("A", 0, "A Mix", "", "spotify:playlist:m", "https://x/1"),
            card("A", 1, "A Mix again", "", "spotify:playlist:m", "https://x/1"),
            card("A", 2, "DJ", "Picked by your own AI DJ.", "spotify:playlist:dj", "https://x/2"),
            // Cards with no heading of their own repeat the shortcuts.
            card("Z", 0, "Lo-Fi", "", "spotify:playlist:lofi", "https://x/3"),
            header("S", "Shows to try"),
            card("S", 0, "Horror Stories", "", "spotify:show:horror", "https://x/4"),
            header("C", "Keep"),
            card("C", 0, "Song Radio", "", "spotify:playlist:r", "https://x/5"),
        ]);
        let sections = parse_home(&json).unwrap();
        let titles: Vec<&str> = sections.iter().map(|section| section.title.as_str()).collect();
        assert_eq!(titles, ["Shortcuts", "Keep"], "a section of only podcasts goes away");
        assert_eq!(sections[0].cards.len(), 1, "repeat, DJ and the stray card are gone");
    }

    #[test]
    fn liked_songs_shows_once_whichever_address_it_comes_under() {
        let json = page(vec![
            header("A", "Shortcuts"),
            card("A", 0, "Liked Songs", "", "spotify:collection", "https://misc.scdn.co/liked"),
            card("A", 1, "Maroon 5", "", "spotify:artist:maroon", "https://x/1"),
            card("A", 2, "Liked Songs", "", "spotify:user:someone:collection", "https://misc.scdn.co/liked"),
        ]);
        let sections = parse_home(&json).unwrap();
        let titles: Vec<&str> = sections[0].cards.iter().map(|card| card.title.as_str()).collect();
        assert_eq!(titles, ["Liked Songs", "Maroon 5"]);
    }

    #[test]
    fn shows_only_the_chosen_rows_with_artists_gathered_into_one() {
        let row = |id: &str, title: &str, cards: Vec<(&str, &str)>| -> Vec<Value> {
            let mut entries = vec![header(id, title)];
            for (number, (name, uri)) in cards.into_iter().enumerate() {
                entries.push(card(id, number as u32, name, "", uri, "https://x/1"));
            }
            entries
        };
        let mut body = Vec::new();
        body.extend(row(SHORTCUTS, "Shortcuts", vec![("A Mix", "spotify:playlist:a"), ("Jessie Reyez", "spotify:artist:jessie")]));
        body.extend(row("OTHER1", "Your playlists", vec![("Mine", "spotify:playlist:mine")]));
        body.extend(row(OTHER_ROWS[2], "Recommended Stations", vec![("Logic Radio", "spotify:playlist:logic")]));
        body.extend(row("OTHER2", "Party", vec![("Party", "spotify:playlist:party")]));
        body.extend(row(OTHER_ROWS[0], "Your top mixes", vec![("2000s Mix", "spotify:playlist:two")]));
        body.extend(row(FAVORITE_ARTISTS, "Your favorite artists", vec![("Rihanna", "spotify:artist:rihanna"), ("Jessie Reyez", "spotify:artist:jessie")]));

        let mut sections = parse_home(&page(body)).unwrap();
        let titles = |sections: &[HomeSection]| sections.iter().map(|section| section.title.clone()).collect::<Vec<_>>();
        assert_eq!(titles(&sections), ["Shortcuts", "Your artists", "Your top mixes", "Recommended Stations"]);
        assert!(sections[0].tiles && !sections[1].tiles);
        let names = |section: &HomeSection| section.cards.iter().map(|card| card.title.clone()).collect::<Vec<_>>();
        assert_eq!(names(&sections[1]), ["Jessie Reyez", "Rihanna"], "in the shortcuts first, and no one twice");

        let followed = |name: &str, id: &str| HomeCard { title: name.into(), subtitle: String::new(), uri: format!("spotify:artist:{id}"), image_url: None, kind: CardKind::Artist };
        add_followed_artists(&mut sections, vec![followed("Rihanna", "rihanna"), followed("Masego", "masego")]);
        assert_eq!(names(&sections[1]), ["Jessie Reyez", "Rihanna", "Masego"]);
        assert_eq!(sections.len(), 4);
    }

    #[test]
    fn followed_artists_get_a_row_when_spotify_sends_none() {
        let followed = HomeCard { title: "Masego".into(), subtitle: String::new(), uri: "spotify:artist:masego".into(), image_url: None, kind: CardKind::Artist };
        let shortcuts = HomeSection { title: "Shortcuts".into(), cards: Vec::new(), tiles: true };
        let mixes = HomeSection { title: "Your top mixes".into(), cards: Vec::new(), tiles: false };
        let mut sections = vec![shortcuts, mixes];
        add_followed_artists(&mut sections, vec![followed]);
        assert_eq!(sections[1].title, ARTISTS_ROW);
        assert_eq!(sections[1].cards.len(), 1);
    }

    #[test]
    fn unknown_ids_still_give_a_short_page() {
        let mut body = Vec::new();
        for number in 0..9 {
            let id = format!("NEW{number}");
            body.push(header(&id, &format!("Row {number}")));
            body.push(card(&id, 0, "A Mix", "", &format!("spotify:playlist:{number}"), "https://x/1"));
        }
        let sections = parse_home(&page(body)).unwrap();
        assert_eq!(sections.len(), 5);
        assert!(sections[0].tiles);
    }

    #[test]
    fn a_page_in_another_shape_is_an_error_not_a_crash() {
        assert!(parse_home(b"not json").is_err());
        assert!(parse_home(b"{\"something\":\"else\"}").is_err());
        assert_eq!(parse_home(b"{\"body\":[{\"id\":1},{\"component\":{\"id\":\"glue2:card\"}}]}").unwrap(), []);
    }

    #[test]
    fn finds_the_radio_playlist_wherever_it_is() {
        let answer = serde_json::json!({ "total": 1, "mediaItems": [{ "uri": "spotify:playlist:37i9dQZF1E8abc" }] });
        assert_eq!(radio_playlist_in(&answer).as_deref(), Some("spotify:playlist:37i9dQZF1E8abc"));
        assert_eq!(radio_playlist_in(&serde_json::json!({ "mediaItems": [{ "uri": "spotify:track:x" }] })), None);
    }

    #[test]
    fn cards_say_what_they_are() {
        let make = |title: &str, kind| HomeCard { title: title.into(), subtitle: String::new(), uri: "spotify:x:abc".into(), image_url: None, kind };
        assert_eq!(make("Jessie Reyez Mix", CardKind::Playlist).kicker(), "Mix");
        assert_eq!(make("Daily Mix 1", CardKind::Playlist).kicker(), "Mix");
        assert_eq!(make("Logic Radio", CardKind::Playlist).kicker(), "Radio");
        assert_eq!(make("Discover Weekly", CardKind::Playlist).kicker(), "Playlist");
        assert_eq!(make("Deadbeat", CardKind::Album).kicker(), "Album");
        assert_eq!(make("Maroon 5", CardKind::Artist).kicker(), "Artist");
        assert_eq!(make("Maroon 5", CardKind::Artist).id(), "abc");
    }

    /// Reads the real Home page and a real mix, and never plays anything:
    /// `SPOTLIGHT_PROBE_TOKEN=... cargo test real_home -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a Spotify login"]
    async fn real_home_and_mix_can_be_read() {
        use librespot::core::{authentication::Credentials, SessionConfig};

        let token = std::env::var("SPOTLIGHT_PROBE_TOKEN").expect("token");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(Credentials::with_access_token(token), false).await.unwrap();

        let started = std::time::Instant::now();
        let sections = fetch_home(&session).await.unwrap();
        println!("home: {} sections in {:?}", sections.len(), started.elapsed());
        for section in &sections {
            println!("  {} ({})", section.title, section.cards.len());
        }

        let mix = sections.iter().flat_map(|section| &section.cards).find(|card| card.kicker() == "Mix").expect("a mix");
        let started = std::time::Instant::now();
        let uris = context_song_uris(&session, &mix.uri).await.unwrap();
        let songs = songs_by_uri(&session, &uris).await;
        println!("{}: {} of {} songs in {:?}", mix.title, songs.len(), uris.len(), started.elapsed());
        assert!(!songs.is_empty());
        let first = &songs[0];
        println!("  first: {} by {} on {} ({} ms, {} covers, released {:?})", first.name, first.artists.first().map(|artist| artist.name.as_str()).unwrap_or("?"), first.album.name, first.duration_ms, first.album.images.len(), first.album.release_date);
        assert!(first.duration_ms > 0 && !first.album.images.is_empty() && !first.artists.is_empty());

        let artist = sections.iter().flat_map(|section| &section.cards).find(|card| card.kind == CardKind::Artist).expect("an artist");
        let started = std::time::Instant::now();
        let top = artist_top_song_uris(&session, artist.id()).await.unwrap();
        let songs = songs_by_uri(&session, &top).await;
        println!("{}: {} top songs in {:?}: {}", artist.title, songs.len(), started.elapsed(), songs.iter().take(3).map(|song| song.name.as_str()).collect::<Vec<_>>().join(", "));
        assert!(!songs.is_empty());
    }
}
