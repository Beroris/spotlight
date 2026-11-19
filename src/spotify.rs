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
/// The longest one wait lasts before background checks look again. The whole of Spotify's
/// Retry-After is still kept to: asking sooner only makes it longer.
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

/// Where the player's state and its commands are.
const PLAYER_URL: &str = "https://api.spotify.com/v1/me/player";
/// How long one of them may take. Other requests get the client's longer limit.
const PLAYER_TIMEOUT: Duration = Duration::from_secs(6);

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

/// Where the pages after the first start, for a part of about `count` entries from `from` on
/// in a list `total` long, when the first page held `first_read` entries and there is more.
fn later_pages(from: usize, first_read: usize, count: usize, total: usize, more: bool) -> Vec<usize> {
    if !more || first_read == 0 {
        return Vec::new();
    }
    // A list Spotify gave no length for is read as far as was asked.
    let end = if total == 0 { from + count } else { (from + count).min(total) };
    (from + first_read..end).step_by(first_read).collect()
}

/// Runs `tasks` together and gives their results in the order the tasks came in.
async fn join_in_order<F: std::future::Future>(tasks: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut tasks: Vec<_> = tasks.into_iter().map(|task| (Box::pin(task), None)).collect();
    std::future::poll_fn(|context| {
        let mut waiting = false;
        for (task, result) in tasks.iter_mut() {
            if result.is_none() {
                match task.as_mut().poll(context) {
                    std::task::Poll::Ready(value) => *result = Some(value),
                    std::task::Poll::Pending => waiting = true,
                }
            }
        }
        if waiting { std::task::Poll::Pending } else { std::task::Poll::Ready(()) }
    })
    .await;
    tasks.into_iter().filter_map(|(_, result)| result).collect()
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
        let mut playback: PlaybackState = response.json().await?;
        // Spotify can go on saying a song plays after the device playing it has gone, and
        // then names no device. Nothing can be heard, so it counts as paused.
        if playback.device.is_none() {
            playback.is_playing = false;
        }
        Ok(Some(playback))
    }

    pub async fn queue(&self, token: &TokenStore) -> Result<QueueResponse> {
        let response = self.request(token, Method::GET, "https://api.spotify.com/v1/me/player/queue", None).await?;
        Ok(response.json().await?)
    }

    pub async fn devices(&self, token: &TokenStore) -> Result<Vec<Device>> {
        let response = self.request(token, Method::GET, "https://api.spotify.com/v1/me/player/devices", None).await?;
        Ok(response.json::<DevicesResponse>().await?.devices)
    }

    /// About `count` entries of the list at `list` from `from` on, in pages of `page_size`.
    ///
    /// The first page says how long the list is, and the pages after it are then asked for
    /// together. One after another, with a pause between, 500 songs took around two seconds
    /// to come and 500 liked songs nearer four.
    async fn part<P: Page + serde::de::DeserializeOwned>(&self, token: &TokenStore, list: &str, page_size: usize, from: usize, count: usize) -> Result<Part<P::Item>> {
        let page: P = self.request(token, Method::GET, &paged(list, page_size, from), None).await?.json().await?;
        let (mut items, first_read, mut next, total) = page.open();
        let mut read = first_read;
        let mut last_read = first_read;
        // Spotify may give fewer to a page than asked for. The next page starts where this
        // one ended either way.
        let starts = later_pages(from, first_read, count, total, next.is_some());
        let pages = starts.iter().map(|start| async move {
            let page: P = self.request(token, Method::GET, &paged(list, first_read, *start), None).await?.json().await?;
            Ok::<_, anyhow::Error>(page.open())
        });
        for page in join_in_order(pages).await {
            let (page_items, page_read, page_next, _) = page?;
            items.extend(page_items);
            read += page_read;
            last_read = page_read;
            next = page_next;
            // A page that came short ends the part there, so no songs are skipped over.
            if page_read < first_read {
                break;
            }
        }
        let more_from = next.filter(|_| last_read > 0).map(|_| from + read);
        Ok(Part { items, total: total.max(from + read), more_from })
    }

    /// The playlists in the library, `count` of them from `from` on.
    pub async fn playlists(&self, token: &TokenStore, from: usize, count: usize) -> Result<Part<PlaylistItem>> {
        self.part::<PlaylistPage>(token, "https://api.spotify.com/v1/me/playlists", 50, from, count).await
    }

    pub async fn liked_songs(&self, token: &TokenStore, from: usize, count: usize) -> Result<Part<FullTrack>> {
        self.part::<SavedTracksPage>(token, "https://api.spotify.com/v1/me/tracks", 50, from, count).await
    }

    pub async fn playlist_tracks(&self, token: &TokenStore, playlist_id: &str, from: usize, count: usize) -> Result<Part<FullTrack>> {
        let url = format!("https://api.spotify.com/v1/playlists/{}/items", urlencoding::encode(playlist_id));
        self.part::<PlaylistTracksPage>(token, &url, 100, from, count).await
    }

    pub async fn playlist_tracks_by_href(&self, token: &TokenStore, href: &str, from: usize, count: usize) -> Result<Part<FullTrack>> {
        self.part::<PlaylistTracksPage>(token, href, 100, from, count).await
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
            // A device that doesn't answer won't answer a transfer either. Trying only makes
            // the wait three times as long before the music can start somewhere else.
            Err(error) if error.downcast_ref::<reqwest::Error>().is_some_and(|error| error.is_timeout()) => Err(error),
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
        let query = urlencoding::encode(&query);
        let page = |page: usize| {
            let query = &query;
            async move {
                let url = format!("https://api.spotify.com/v1/search?q={query}&type=track&limit=10&offset={}", page * 10);
                let response = self.request(token, Method::GET, &url, None).await?;
                Ok::<_, anyhow::Error>(response.json::<SearchTracksResponse>().await?.tracks.items)
            }
        };
        // Asked for together: one after another, ten pages took about three seconds, which
        // was most of the wait for an artist's page. The first few go first, as an artist
        // with few songs has no more, and the rest only if those were all there.
        let mut all: Vec<FullTrack> = Vec::new();
        'pages: for wave in [0..3, 3..10] {
            for items in join_in_order(wave.map(page)).await {
                let items = items?;
                // Pages can come back short with more after them, so only an empty one ends it.
                if items.is_empty() {
                    break 'pages;
                }
                all.extend(items.into_iter().filter(|track| {
                    track.artists.iter().any(|candidate| candidate.name.eq_ignore_ascii_case(artist))
                }));
            }
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
        // Spotify holds a player command until the device answers. One that has closed or
        // gone to sleep never does, and the button pressed shouldn't hang on it for long.
        if url.starts_with(PLAYER_URL) {
            builder = builder.timeout(PLAYER_TIMEOUT);
        }
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
            let wait_ms = retry_after_secs.max(1).saturating_mul(1000);
            QUIET_UNTIL_MS.fetch_max(clock_ms().saturating_add(wait_ms), std::sync::atomic::Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::{join_in_order, later_pages};

    #[test]
    fn the_pages_after_the_first_cover_the_part_asked_for_and_no_more() {
        // 500 songs of a long playlist, 100 to a page.
        assert_eq!(later_pages(0, 100, 500, 1_240, true), [100, 200, 300, 400]);
        // The next part carries on where that one ended.
        assert_eq!(later_pages(500, 100, 500, 1_240, true), [600, 700, 800, 900]);
        // A list shorter than what was asked for ends where it ends.
        assert_eq!(later_pages(0, 50, 500, 120, true), [50, 100]);
        // Spotify gave fewer to a page than asked for: the pages follow what it gave.
        assert_eq!(later_pages(0, 50, 200, 1_000, true), [50, 100, 150]);
    }

    #[test]
    fn a_list_that_fits_one_page_asks_for_no_more() {
        assert!(later_pages(0, 37, 500, 37, false).is_empty());
        assert!(later_pages(0, 100, 100, 1_000, true).is_empty());
        // An empty page says nothing about how far apart pages are.
        assert!(later_pages(0, 0, 500, 1_000, true).is_empty());
    }

    #[test]
    fn a_list_of_unknown_length_is_read_as_far_as_asked() {
        assert_eq!(later_pages(0, 50, 150, 0, true), [50, 100]);
    }

    #[tokio::test]
    async fn pages_asked_for_together_come_back_in_the_order_asked() {
        // The first takes longest, the last comes at once.
        let pages = [30u64, 10, 0].map(|wait| async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            wait
        });
        let started = std::time::Instant::now();
        assert_eq!(join_in_order(pages).await, [30, 10, 0]);
        // Together, not one after another: all three in about the time of the longest.
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(join_in_order(Vec::<std::future::Ready<u8>>::new()).await.is_empty());
    }
}
