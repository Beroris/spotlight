use serde::{Deserialize, Serialize};

pub const LIKED_SONGS_PLAYLIST_URI: &str = "spotlight:liked-songs";

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub client_id: String,
    pub redirect_uri: String,
    pub device_name: String,
    #[serde(default = "default_artwork_enabled")]
    pub artwork_enabled: bool,
    #[serde(default = "default_show_artist_column")]
    pub show_artist_column: bool,
    #[serde(default = "default_show_time_column")]
    pub show_time_column: bool,
    #[serde(default)]
    pub compact_sidebars: bool,
    /// The library's and the right panel's widths as dragged, in logical pixels. Zero is
    /// the usual width.
    #[serde(default)]
    pub library_width: u32,
    #[serde(default)]
    pub panel_width: u32,
    /// Whether the mini player stays above other windows.
    #[serde(default = "default_true")]
    pub mini_on_top: bool,
    /// Where the mini player was last left: its window's top left corner on the screen, in
    /// physical pixels.
    #[serde(default)]
    pub mini_position: Option<(i32, i32)>,
    #[serde(default = "default_theme_mode")]
    pub theme_mode: String,
    #[serde(default = "default_accent_color")]
    pub accent_color: String,
    #[serde(default = "default_audio_quality")]
    pub audio_quality: String,
    /// The Windows output device to play through. Empty means the Windows default.
    #[serde(default)]
    pub output_device: String,
    #[serde(default = "default_true")]
    pub normalize_volume: bool,
    #[serde(default = "default_true")]
    pub autoplay_similar: bool,
    /// Seconds a song fades into the next as it ends. Zero is off.
    #[serde(default)]
    pub crossfade_seconds: u32,
    #[serde(default = "default_true")]
    pub remember_volume: bool,
    /// Resume the last session on this PC when Spotlight opens. Off by default
    /// so launching the app never takes over playback from another device.
    #[serde(default)]
    pub resume_on_startup: bool,
    #[serde(default = "default_volume_percent")]
    pub last_volume_percent: i32,
    #[serde(default = "default_true")]
    pub global_hotkeys: bool,
    #[serde(default = "default_true")]
    pub media_keys_enabled: bool,
    #[serde(default = "default_play_pause_hotkey")]
    pub play_pause_hotkey: String,
    #[serde(default = "default_next_hotkey")]
    pub next_hotkey: String,
    #[serde(default = "default_previous_hotkey")]
    pub previous_hotkey: String,
    #[serde(default = "default_shuffle_hotkey")]
    pub shuffle_hotkey: String,
    #[serde(default = "default_repeat_hotkey")]
    pub repeat_hotkey: String,
    #[serde(default)]
    pub minimize_to_tray: bool,
    #[serde(default)]
    pub close_to_tray: bool,
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default = "default_true")]
    pub low_resource_mode: bool,
    #[serde(default)]
    pub reduce_animations: bool,
    #[serde(default = "default_true")]
    pub disable_background_effects: bool,
    /// Tint the header and Now Playing panel with the current album's color.
    #[serde(default = "default_true")]
    pub album_colors: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub developer_mode: bool,
    /// Look for a new version on GitHub when Spotlight opens and once a day.
    #[serde(default = "default_true")]
    pub check_for_updates: bool,
    pub token: Option<TokenStore>,
    pub local_playback_token: Option<TokenStore>,
    #[serde(default)]
    pub library_snapshot: LibrarySnapshot,
    #[serde(default)]
    pub startup_snapshot: StartupSnapshot,
}

fn default_artwork_enabled() -> bool {
    true
}

fn default_show_artist_column() -> bool {
    true
}

fn default_show_time_column() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_play_pause_hotkey() -> String { "Ctrl+Alt+P".into() }
fn default_next_hotkey() -> String { "Ctrl+Alt+N".into() }
fn default_previous_hotkey() -> String { "Ctrl+Alt+B".into() }
fn default_shuffle_hotkey() -> String { "Ctrl+Alt+S".into() }
fn default_repeat_hotkey() -> String { "Ctrl+Alt+R".into() }

#[derive(Clone, Debug, Deserialize)]
pub struct QueueResponse {
    pub currently_playing: Option<FullTrack>,
    #[serde(default)]
    pub queue: Vec<FullTrack>,
}

fn default_theme_mode() -> String {
    "Dark".to_string()
}

fn default_accent_color() -> String {
    "Green".to_string()
}

fn default_audio_quality() -> String {
    "Normal".to_string()
}

fn default_log_level() -> String {
    "Info".to_string()
}

fn default_volume_percent() -> i32 {
    100
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenStore {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_epoch: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaylistPage {
    pub items: Vec<PlaylistItem>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaylistItem {
    pub name: String,
    pub uri: String,
    #[serde(default)]
    pub images: Vec<AlbumImage>,
    #[serde(alias = "tracks", alias = "items")]
    pub tracks: PlaylistTracksRef,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaylistTracksRef {
    pub href: String,
    #[serde(default)]
    pub total: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaylistTracksPage {
    pub items: Vec<PlaylistTrackItem>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaylistTrackItem {
    #[serde(alias = "item")]
    pub track: Option<FullTrack>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SavedTracksPage {
    pub items: Vec<SavedTrackItem>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SavedTrackItem {
    pub track: FullTrack,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RecentlyPlayedPage {
    pub items: Vec<RecentlyPlayedItem>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RecentlyPlayedItem {
    pub track: FullTrack,
    #[serde(default)]
    pub played_at: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SearchTracksResponse {
    pub tracks: SearchTracksPage,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TracksResponse {
    pub tracks: Vec<Option<FullTrack>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SearchTracksPage {
    pub items: Vec<FullTrack>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FullTrack {
    pub name: String,
    pub uri: String,
    pub duration_ms: i32,
    pub album: Album,
    pub artists: Vec<Artist>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Album {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub images: Vec<AlbumImage>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AlbumImage {
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Artist {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub progress_ms: Option<i32>,
    pub item: Option<FullTrack>,
    #[serde(default)]
    pub shuffle_state: bool,
    #[serde(default)]
    pub repeat_state: String,
    #[serde(default)]
    pub context: Option<PlaybackContext>,
    #[serde(default)]
    pub device: Option<Device>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaybackContext {
    pub uri: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DevicesResponse {
    pub devices: Vec<Device>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Device {
    pub id: Option<String>,
    pub is_active: bool,
    pub name: String,
    #[serde(rename = "type", default)]
    pub device_type: String,
    #[serde(default)]
    pub volume_percent: Option<i32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LibrarySnapshot {
    pub playlists: Vec<CachedPlaylist>,
    pub liked_songs: Vec<CachedLikedSong>,
    #[serde(default)]
    pub fetched_at_epoch: u64,
    #[serde(default)]
    pub next_allowed_refresh_at_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedPlaylist {
    pub name: String,
    pub uri: String,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub track_count: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedLikedSong {
    pub name: String,
    pub artists: Vec<String>,
    pub uri: String,
    #[serde(default)]
    pub duration_ms: i32,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub image_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct StartupSnapshot {
    pub last_track_uri: Option<String>,
    pub last_playlist_uri: Option<String>,
    pub last_played_at_epoch: u64,
}
