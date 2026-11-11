use anyhow::{anyhow, Result};
use reqwest::{header::CONTENT_LENGTH, Client, Method, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;

use crate::models::{Device, DevicesResponse, FullTrack, PlaybackState, PlaylistItem, PlaylistPage, PlaylistTrackItem, PlaylistTracksPage, QueueResponse, SavedTracksPage, SearchTracksResponse, TokenStore};

#[derive(Debug, Clone)]
pub struct RateLimitError {
    pub retry_after_secs: u64,
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "spotify request rate-limited; retry after {}s", self.retry_after_secs)
    }
}

impl std::error::Error for RateLimitError {}

/// When Spotify last said to wait until, in ms since Spotlight started. Not by the wall clock,
/// which setting back could stretch the wait by. Its limit counts every
/// request the app makes, so background checks hold off for all of them.
static QUIET_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The longest Spotlight waits on Spotify's word, so a huge Retry-After can't stop it for hours.
const LONGEST_RATE_LIMIT_WAIT: Duration = Duration::from_secs(600);

fn clock_ms() -> u64 {
    static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    STARTED.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// How long background checks should still wait after Spotify last asked to slow down.
pub fn rate_limit_wait() -> Option<Duration> {
    let left = QUIET_UNTIL_MS.load(std::sync::atomic::Ordering::Relaxed).saturating_sub(clock_ms());
    (left > 0).then(|| Duration::from_millis(left).min(LONGEST_RATE_LIMIT_WAIT))
}

/// How many songs a long list shows at first, and how many more each time more are asked for.
pub const SONGS_PER_PART: usize = 500;
/// The same for the playlists in the library.
pub const PLAYLISTS_PER_PART: usize = 200;

/// Part of a long list on Spotify, from where it was asked for.
pub struct Part<T> {
    pub items: Vec<T>,
    /// How long the whole list is, as Spotify counts it.
    pub total: usize,
    /// Where the next part starts in Spotify's list, while there's more.
    pub more_from: Option<usize>,
}

/// A page of a list as Spotify sends it.
trait Page {
    type Item;
    /// What it holds, how many entries that was before any that can't be shown were left
    /// out, where the next page is, and how long the whole list is.
    fn open(self) -> (Vec<Self::Item>, usize, Option<String>, usize);
}

impl Page for PlaylistPage {
    type Item = PlaylistItem;
    fn open(self) -> (Vec<PlaylistItem>, usize, Option<String>, usize) {
        let read = self.items.len();
        (self.items, read, self.next, self.total.unwrap_or(0) as usize)
    }
}

impl Page for SavedTracksPage {
    type Item = FullTrack;
    fn open(self) -> (Vec<FullTrack>, usize, Option<String>, usize) {
        let read = self.items.len();
        (self.items.into_iter().map(|item| item.track).collect(), read, self.next, self.total.unwrap_or(0) as usize)
    }
}

impl Page for PlaylistTracksPage {
    type Item = FullTrack;
    fn open(self) -> (Vec<FullTrack>, usize, Option<String>, usize) {
        let read = self.items.len();
        // Local files and songs taken off Spotify come without one.
        let tracks = self.items.into_iter().filter_map(|item: PlaylistTrackItem| item.track).collect();
        (tracks, read, self.next, self.total.unwrap_or(0) as usize)
    }
}

/// `url` with `limit` and `offset` added.
fn paged(url: &str, limit: usize, offset: usize) -> String {
    let joiner = if url.contains('?') { '&' } else { '?' };
    format!("{url}{joiner}limit={limit}&offset={offset}")
}

#[derive(Clone)]
pub struct SpotifyApi {
    http: Client,
}

impl SpotifyApi {
    pub fn new() -> Self {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(12))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("valid Spotify HTTP client configuration");
        Self { http }
    }

    /// The connection pool, for other requests that would otherwise set up their own.
    pub fn http(&self) -> &Client {
        &self.http
    }

    pub async fn current_playback(&self, token: &TokenStore) -> Result<Option<PlaybackState>> {
        let response = self.request(token, Method::GET, "https://api.spotify.com/v1/me/player", None).await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(response.json().await?))
    }

    pub async fn queue(&self, token: &TokenStore) -> Result<QueueResponse> {
        let response = self.request(token, Method::GET, "https://api.spotify.com/v1/me/player/queue", None).await?;
        Ok(response.json().await?)
    }

    pub async fn devices(&self, token: &TokenStore) -> Result<Vec<Device>> {
        let response = self.request(token, Method::GET, "https://api.spotify.com/v1/me/player/devices", None).await?;
        Ok(response.json::<DevicesResponse>().await?.devices)
    }

    /// About `count` entries of a list from `from` on, a page at a time. `first` is the first
    /// page's address.
    async fn part<P: Page + serde::de::DeserializeOwned>(&self, token: &TokenStore, first: String, from: usize, count: usize) -> Result<Part<P::Item>> {
        let mut url = first;
        let mut items = Vec::new();
        let mut read = 0;
        loop {
            let page: P = self.request(token, Method::GET, &url, None).await?.json().await?;
            let (page_items, page_read, next, total) = page.open();
            items.extend(page_items);
            read += page_read;
            match next {
                Some(next) if read < count && page_read > 0 => {
                    url = next;
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                next => {
                    let more_from = next.filter(|_| page_read > 0).map(|_| from + read);
                    return Ok(Part { items, total: total.max(from + read), more_from });
                }
            }
        }
    }

    /// The playlists in the library, `count` of them from `from` on.
    pub async fn playlists(&self, token: &TokenStore, from: usize, count: usize) -> Result<Part<PlaylistItem>> {
        self.part::<PlaylistPage>(token, paged("https://api.spotify.com/v1/me/playlists", 50, from), from, count).await
    }

    pub async fn liked_songs(&self, token: &TokenStore, from: usize, count: usize) -> Result<Part<FullTrack>> {
        self.part::<SavedTracksPage>(token, paged("https://api.spotify.com/v1/me/tracks", 50, from), from, count).await
    }

    pub async fn playlist_tracks(&self, token: &TokenStore, playlist_id: &str, from: usize, count: usize) -> Result<Part<FullTrack>> {
        let url = format!("https://api.spotify.com/v1/playlists/{}/items", urlencoding::encode(playlist_id));
        self.part::<PlaylistTracksPage>(token, paged(&url, 100, from), from, count).await
    }

    pub async fn playlist_tracks_by_href(&self, token: &TokenStore, href: &str, from: usize, count: usize) -> Result<Part<FullTrack>> {
        self.part::<PlaylistTracksPage>(token, paged(href, 100, from), from, count).await
    }

    pub async fn playlist_image_url(&self, token: &TokenStore, playlist_id: &str) -> Result<Option<String>> {
        let response = self
            .request(
                token,
                Method::GET,
                &format!(
                    "https://api.spotify.com/v1/playlists/{}/images",
                    urlencoding::encode(playlist_id)
                ),
                None,
            )
            .await?;
        let images: Vec<crate::models::AlbumImage> = response.json().await?;
        Ok(images.into_iter().next().map(|image| image.url))
    }

    pub async fn transfer(&self, token: &TokenStore, device_id: &str, play: bool) -> Result<()> {
        self.request(
            token,
            Method::PUT,
            "https://api.spotify.com/v1/me/player",
            Some(json!({ "device_ids": [device_id], "play": play })),
        ).await?;
        Ok(())
    }

    /// Start music on a device. Spotify moves playback there by itself, so we
    /// do not transfer first: a transfer loads the previous session onto the
    /// device, and Spotify can keep reporting that session after the new music
    /// starts. Transfer is only the fallback for a device that refuses.
    async fn start_on_device(&self, token: &TokenStore, device_id: &str, body: Value) -> Result<()> {
        let url = format!("https://api.spotify.com/v1/me/player/play?device_id={}", urlencoding::encode(device_id));
        match self.request(token, Method::PUT, &url, Some(body.clone())).await {
            Ok(_) => Ok(()),
            Err(error) if error.downcast_ref::<RateLimitError>().is_some() => Err(error),
            Err(_) => {
                self.transfer(token, device_id, false).await?;
                tokio::time::sleep(Duration::from_millis(300)).await;
                self.request(token, Method::PUT, &url, Some(body)).await?;
                Ok(())
            }
        }
    }

    pub async fn play_playlist(&self, token: &TokenStore, device_id: &str, context_uri: &str) -> Result<()> {
        self.start_on_device(token, device_id, json!({ "context_uri": context_uri })).await
    }

    pub async fn play_context_from_track(
        &self,
        token: &TokenStore,
        device_id: &str,
        context_uri: &str,
        track_uri: &str,
    ) -> Result<()> {
        self.start_on_device(
            token,
            device_id,
            json!({
                "context_uri": context_uri,
                "offset": { "uri": track_uri },
            }),
        ).await
    }

    pub async fn play_track(&self, token: &TokenStore, device_id: &str, track_uri: &str) -> Result<()> {
        self.start_on_device(token, device_id, json!({ "uris": [track_uri] })).await
    }

    /// Start partway into the first track. A separate seek right after play
    /// can arrive before the track has loaded and get lost.
    pub async fn play_tracks_at(&self, token: &TokenStore, device_id: &str, track_uris: &[String], position_ms: i32) -> Result<()> {
        self.start_on_device(token, device_id, json!({ "uris": track_uris, "position_ms": position_ms.max(0) })).await
    }

    pub async fn play_tracks(&self, token: &TokenStore, device_id: &str, track_uris: &[String]) -> Result<()> {
        self.start_on_device(token, device_id, json!({ "uris": track_uris })).await
    }

    pub async fn add_to_queue(&self, token: &TokenStore, device_id: Option<&str>, track_uri: &str) -> Result<()> {
        let mut url = format!(
            "https://api.spotify.com/v1/me/player/queue?uri={}",
            urlencoding::encode(track_uri)
        );
        if let Some(device_id) = device_id {
            url.push_str("&device_id=");
            url.push_str(&urlencoding::encode(device_id));
        }
        self.request(token, Method::POST, &url, None).await?;
        Ok(())
    }

    pub async fn library_contains(&self, token: &TokenStore, uri: &str) -> Result<bool> {
        let url = format!("https://api.spotify.com/v1/me/library/contains?uris={}", urlencoding::encode(uri));
        let response = self.request(token, Method::GET, &url, None).await?;
        let saved: Vec<bool> = response.json().await?;
        Ok(saved.first().copied().unwrap_or(false))
    }

    pub async fn set_in_library(&self, token: &TokenStore, uri: &str, saved: bool) -> Result<()> {
        let url = format!("https://api.spotify.com/v1/me/library?uris={}", urlencoding::encode(uri));
        let method = if saved { Method::PUT } else { Method::DELETE };
        self.request(token, method, &url, None).await?;
        Ok(())
    }

    pub async fn add_tracks_to_playlist(&self, token: &TokenStore, playlist_id: &str, track_uris: &[String]) -> Result<()> {
        self.request(
            token,
            Method::POST,
            &format!(
                "https://api.spotify.com/v1/playlists/{}/items",
                urlencoding::encode(playlist_id)
            ),
            Some(json!({ "uris": track_uris })),
        ).await?;
        Ok(())
    }

    /// The signed-in account's id.
    pub async fn account_id(&self, token: &TokenStore) -> Result<String> {
        let me: Value = self.request(token, Method::GET, "https://api.spotify.com/v1/me", None).await?.json().await?;
        me.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string).ok_or_else(|| anyhow!("Spotify didn't say which account this is"))
    }

    /// A new private playlist in the account, empty.
    pub async fn create_playlist(&self, token: &TokenStore, name: &str) -> Result<PlaylistItem> {
        let body = json!({ "name": name, "public": false });
        let response = match self.request(token, Method::POST, "https://api.spotify.com/v1/me/playlists", Some(body.clone())).await {
            Ok(response) => response,
            // Where it was before Spotify moved it: under the account's id.
            Err(first) => {
                let Ok(user) = self.account_id(token).await else { return Err(first) };
                let url = format!("https://api.spotify.com/v1/users/{}/playlists", urlencoding::encode(&user));
                self.request(token, Method::POST, &url, Some(body)).await?
            }
        };
        Ok(response.json().await?)
    }

    /// The last 50 songs Spotify has counted on this account, newest first, each with when
    /// it was played in milliseconds since 1970.
    pub async fn recently_played(&self, token: &TokenStore) -> Result<Vec<(i64, FullTrack)>> {
        let response = self.request(token, Method::GET, "https://api.spotify.com/v1/me/player/recently-played?limit=50", None).await?;
        let page: crate::models::RecentlyPlayedPage = response.json().await?;
        // Spotify sends them newest first, so one it gives no readable time keeps its place
        // by taking the time of the one before.
        let mut last_at = i64::MAX;
        Ok(page
            .items
            .into_iter()
            .map(|item| {
                last_at = crate::history::parse_time_ms(&item.played_at).unwrap_or(last_at);
                (last_at, item.track)
            })
            .collect())
    }

    pub async fn track(&self, token: &TokenStore, track_id: &str) -> Result<FullTrack> {
        let response = self
            .request(
                token,
                Method::GET,
                &format!("https://api.spotify.com/v1/tracks/{}", urlencoding::encode(track_id)),
                None,
            )
            .await?;
        Ok(response.json().await?)
    }

    pub async fn tracks(&self, token: &TokenStore, track_ids: &[String]) -> Result<Vec<FullTrack>> {
        if track_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = track_ids
            .iter()
            .take(50)
            .map(|id| urlencoding::encode(id))
            .collect::<Vec<_>>()
            .join(",");
        let response = self
            .request(
                token,
                Method::GET,
                &format!("https://api.spotify.com/v1/tracks?ids={ids}"),
                None,
            )
            .await?;
        Ok(response
            .json::<crate::models::TracksResponse>()
            .await?
            .tracks
            .into_iter()
            .flatten()
            .collect())
    }

    pub async fn search_tracks(&self, token: &TokenStore, query: &str, limit: u8) -> Result<Vec<FullTrack>> {
        let limit = limit.clamp(1, 10);
        let response = self
            .request(
                token,
                Method::GET,
                &format!(
                    "https://api.spotify.com/v1/search?q={}&type=track&limit={}",
                    urlencoding::encode(query),
                    limit
                ),
                None,
            )
            .await?;
        Ok(response.json::<SearchTracksResponse>().await?.tracks.items)
    }

    pub async fn artist_image_url(&self, token: &TokenStore, artist_id: &str) -> Result<Option<String>> {
        #[derive(serde::Deserialize)]
        struct ArtistImages {
            #[serde(default)]
            images: Vec<crate::models::AlbumImage>,
        }
        let url = format!("https://api.spotify.com/v1/artists/{}", urlencoding::encode(artist_id));
        let response = self.request(token, Method::GET, &url, None).await?;
        Ok(response.json::<ArtistImages>().await?.images.into_iter().next().map(|image| image.url))
    }

    /// The artists this account follows, as name, id and a card-sized picture.
    pub async fn followed_artists(&self, token: &TokenStore) -> Result<Vec<(String, String, Option<String>)>> {
        #[derive(serde::Deserialize)]
        struct Picture {
            url: String,
            #[serde(default)]
            width: Option<u32>,
        }
        #[derive(serde::Deserialize)]
        struct Followed {
            id: String,
            name: String,
            #[serde(default)]
            images: Vec<Picture>,
        }
        #[derive(serde::Deserialize)]
        struct Page {
            items: Vec<Followed>,
        }
        #[derive(serde::Deserialize)]
        struct Response {
            artists: Page,
        }
        let response = self
            .request(token, Method::GET, "https://api.spotify.com/v1/me/following?type=artist&limit=50", None)
            .await?;
        Ok(response
            .json::<Response>()
            .await?
            .artists
            .items
            .into_iter()
            .map(|artist| {
                // Largest first, so the last one at least this big is the best fit.
                let picture = artist.images.iter().rev().find(|image| image.width.unwrap_or(0) >= 200).or(artist.images.first());
                (artist.name, artist.id, picture.map(|image| image.url.clone()))
            })
            .collect())
    }

    /// An artist's songs. Spotify no longer offers top tracks to apps like this
    /// one and caps search pages at 10, so this walks a few pages of search.
    pub async fn artist_tracks(&self, token: &TokenStore, artist: &str) -> Result<Vec<FullTrack>> {
        let query = format!("artist:\"{artist}\"");
        let mut all: Vec<FullTrack> = Vec::new();
        for page in 0..10 {
            let response = self
                .request(
                    token,
                    Method::GET,
                    &format!(
                        "https://api.spotify.com/v1/search?q={}&type=track&limit=10&offset={}",
                        urlencoding::encode(&query),
                        page * 10
                    ),
                    None,
                )
                .await?;
            let items = response.json::<SearchTracksResponse>().await?.tracks.items;
            // Pages can come back short with more after them, so only an empty one ends it.
            if items.is_empty() {
                break;
            }
            all.extend(items.into_iter().filter(|track| {
                track.artists.iter().any(|candidate| candidate.name.eq_ignore_ascii_case(artist))
            }));
        }
        Ok(all)
    }

    /// Every song on an album. Album listings leave the album off each song, so
    /// it's filled in from the one passed here.
    pub async fn album_tracks(&self, token: &TokenStore, album: &crate::models::Album) -> Result<Vec<FullTrack>> {
        #[derive(serde::Deserialize)]
        struct AlbumTrack {
            name: String,
            uri: String,
            duration_ms: i32,
            artists: Vec<crate::models::Artist>,
        }
        #[derive(serde::Deserialize)]
        struct AlbumTracksPage {
            items: Vec<AlbumTrack>,
            next: Option<String>,
        }
        let id = album.id.as_deref().ok_or_else(|| anyhow!("that song has no album"))?;
        let mut url = format!("https://api.spotify.com/v1/albums/{}/tracks?limit=50", urlencoding::encode(id));
        let mut all = Vec::new();
        loop {
            let response = self.request(token, Method::GET, &url, None).await?;
            let page: AlbumTracksPage = response.json().await?;
            all.extend(page.items.into_iter().map(|track| FullTrack {
                name: track.name,
                uri: track.uri,
                duration_ms: track.duration_ms,
                album: album.clone(),
                artists: track.artists,
            }));
            match page.next {
                Some(next) => url = next,
                None => return Ok(all),
            }
        }
    }

    pub async fn pause(&self, token: &TokenStore, device_id: Option<&str>) -> Result<()> {
        let url = endpoint_with_device("https://api.spotify.com/v1/me/player/pause", device_id);
        self.request(token, Method::PUT, &url, None).await?;
        Ok(())
    }

    pub async fn resume(&self, token: &TokenStore, device_id: Option<&str>) -> Result<()> {
        let url = endpoint_with_device("https://api.spotify.com/v1/me/player/play", device_id);
        self.request(token, Method::PUT, &url, None).await?;
        Ok(())
    }

    pub async fn next(&self, token: &TokenStore, device_id: Option<&str>) -> Result<()> {
        let url = endpoint_with_device("https://api.spotify.com/v1/me/player/next", device_id);
        self.request(token, Method::POST, &url, None).await?;
        Ok(())
    }

    pub async fn previous(&self, token: &TokenStore, device_id: Option<&str>) -> Result<()> {
        let url = endpoint_with_device("https://api.spotify.com/v1/me/player/previous", device_id);
        self.request(token, Method::POST, &url, None).await?;
        Ok(())
    }

    pub async fn seek(&self, token: &TokenStore, device_id: Option<&str>, position_ms: i32) -> Result<()> {
        let mut url = format!("https://api.spotify.com/v1/me/player/seek?position_ms={position_ms}");
        if let Some(device_id) = device_id {
            url.push_str("&device_id=");
            url.push_str(&urlencoding::encode(device_id));
        }
        self.request(token, Method::PUT, &url, None).await?;
        Ok(())
    }

    pub async fn volume(&self, token: &TokenStore, device_id: Option<&str>, volume_percent: i32) -> Result<()> {
        let mut url = format!("https://api.spotify.com/v1/me/player/volume?volume_percent={volume_percent}");
        if let Some(device_id) = device_id {
            url.push_str("&device_id=");
            url.push_str(&urlencoding::encode(device_id));
        }
        self.request(token, Method::PUT, &url, None).await?;
        Ok(())
    }

    pub async fn shuffle(&self, token: &TokenStore, device_id: Option<&str>, enabled: bool) -> Result<()> {
        let mut url = format!(
            "https://api.spotify.com/v1/me/player/shuffle?state={}",
            if enabled { "true" } else { "false" }
        );
        if let Some(device_id) = device_id {
            url.push_str("&device_id=");
            url.push_str(&urlencoding::encode(device_id));
        }
        self.request(token, Method::PUT, &url, None).await?;
        Ok(())
    }

    pub async fn repeat(&self, token: &TokenStore, device_id: Option<&str>, state: &str) -> Result<()> {
        let mut url = format!(
            "https://api.spotify.com/v1/me/player/repeat?state={}",
            urlencoding::encode(state)
        );
        if let Some(device_id) = device_id {
            url.push_str("&device_id=");
            url.push_str(&urlencoding::encode(device_id));
        }
        self.request(token, Method::PUT, &url, None).await?;
        Ok(())
    }

    async fn request(&self, token: &TokenStore, method: Method, url: &str, body: Option<Value>) -> Result<reqwest::Response> {
        let requires_empty_body = body.is_none() && (method == Method::POST || method == Method::PUT);
        let mut builder = self.http.request(method, url).bearer_auth(&token.access_token);
        if let Some(body) = body {
            builder = builder.json(&body);
        } else if requires_empty_body {
            builder = builder.header(CONTENT_LENGTH, "0").body(Vec::<u8>::new());
        }

        let response = builder.send().await?;
        if response.status().is_success() || response.status() == StatusCode::NO_CONTENT {
            return Ok(response);
        }

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(5);
            let wait = Duration::from_secs(retry_after_secs.max(1)).min(LONGEST_RATE_LIMIT_WAIT);
            QUIET_UNTIL_MS.fetch_max(clock_ms() + wait.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
            // Return immediately. Sleeping here used to hold the caller's
            // playback-refresh permit for the full Retry-After duration.
            return Err(RateLimitError { retry_after_secs }.into());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or(body);
        if status == StatusCode::FORBIDDEN && message.to_ascii_lowercase().contains("scope") {
            return Err(anyhow!(
                "Spotify denied this action ({message}). Reconnect Spotify in Settings → Account to grant the new permissions."
            ));
        }
        if status == StatusCode::NOT_FOUND && message.to_ascii_lowercase().contains("device") {
            return Err(anyhow!("no active device: {message}"));
        }
        Err(anyhow!("spotify request failed ({status}): {message}"))
    }
}

fn endpoint_with_device(base: &str, device_id: Option<&str>) -> String {
    match device_id {
        Some(device_id) => format!("{base}?device_id={}", urlencoding::encode(device_id)),
        None => base.to_string(),
    }
}
