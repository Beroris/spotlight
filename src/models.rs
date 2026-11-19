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
    /// Who else on the account may set this player's volume: "Any" device or app, only
    /// "Spotlight" on another device, or "None". Anything else is read as "Any".
    #[serde(default = "default_volume_from")]
    pub volume_from: String,
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
    /// Wait in the tray, rather than open, when Windows starts Spotlight at sign-in.
    #[serde(default = "default_true")]
    pub tray_at_sign_in: bool,
    #[serde(default = "default_true")]
    pub low_resource_mode: bool,
    #[serde(default)]
    pub reduce_animations: bool,
    #[serde(default = "default_true")]
    pub disable_background_effects: bool,
    /// Tint the header and Now Playing panel with the current album's color.
    #[serde(default = "default_true")]
    pub album_colors: bool,
    /// The player bar's shape: "Bar" for one rounded bar, "Islands" for three.
    #[serde(default = "default_bar_shape")]
    pub bar_shape: String,
    /// What the player bar is made of: "Glass", "Frosted" or "Smoked".
    #[serde(default = "default_bar_look")]
    pub bar_look: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub developer_mode: bool,
    /// Look for a new version on GitHub when Spotlight opens and once a day.
    #[serde(default = "default_true")]
    pub check_for_updates: bool,
    pub token: Option<TokenStore>,
    pub local_playback_token: Option<TokenStore>,
    /// The Spotify account signed in, by its id, so what Spotlight keeps for each account
    /// finds its own. Learned after signing in.
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub library_snapshot: LibrarySnapshot,
    #[serde(default)]
    pub startup_snapshot: StartupSnapshot,
    /// The shuffle and repeat last used, overall and with each playlist.
    #[serde(default)]
    pub play_modes: PlayModes,
}

/// Shuffle and repeat as they were last used.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Modes {
    pub shuffle: bool,
    /// "off", "context" or "track", as Spotify names them.
    pub repeat: String,
}

/// The shuffle and repeat last used with each playlist, and with any list at all. A list
/// that hasn't been played with Spotlight yet starts the way the last one was left.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct PlayModes {
    #[serde(default)]
    pub last: Option<Modes>,
    /// Newest first, as (list address, modes).
    #[serde(default)]
    pub lists: Vec<(String, Modes)>,
}

impl PlayModes {
    /// How many lists are remembered. The one left alone longest goes first.
    const MOST_LISTS: usize = 300;

    /// What `list_uri` should start with, if anything has been used yet.
    pub fn for_list(&self, list_uri: &str) -> Option<&Modes> {
        self.lists
            .iter()
            .find(|(uri, _)| uri == list_uri)
            .map(|(_, modes)| modes)
            .or(self.last.as_ref())
    }

    /// Notes `modes` as the last used, and as `list_uri`'s when there is one. True when that
    /// changed anything.
    pub fn remember(&mut self, list_uri: Option<&str>, modes: Modes) -> bool {
        let mut changed = self.last.as_ref() != Some(&modes);
        self.last = Some(modes.clone());
        if let Some(list_uri) = list_uri.filter(|uri| !uri.is_empty()) {
            let at = self.lists.iter().position(|(uri, _)| uri == list_uri);
            changed |= at.is_none_or(|at| at != 0 || self.lists[at].1 != modes);
            if let Some(at) = at {
                self.lists.remove(at);
            }
            self.lists.insert(0, (list_uri.to_string(), modes));
            self.lists.truncate(Self::MOST_LISTS);
        }
        changed
    }
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

fn default_bar_shape() -> String {
    "Bar".to_string()
}

fn default_bar_look() -> String {
    "Glass".to_string()
}

fn default_theme_mode() -> String {
    "Dark".to_string()
}

fn default_accent_color() -> String {
    "Green".to_string()
}

fn default_volume_from() -> String {
    "Any".to_string()
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
    #[serde(default)]
    pub total: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaylistItem {
    pub name: String,
    pub uri: String,
    #[serde(default)]
    pub images: Vec<AlbumImage>,
    #[serde(alias = "tracks", alias = "items")]
    pub tracks: PlaylistTracksRef,
    #[serde(default)]
    pub snapshot_id: Option<String>,
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
    #[serde(default)]
    pub total: Option<u32>,
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
    #[serde(default)]
    pub total: Option<u32>,
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
    /// The first of Liked Songs, to show at once on opening. More, if asked for, aren't kept.
    pub liked_songs: Vec<CachedLikedSong>,
    /// How many songs Liked Songs has, as Spotify last said. Zero if it hasn't.
    #[serde(default)]
    pub liked_songs_total: usize,
    /// How many playlists the library has in all, as Spotify last said, and how many of them
    /// were asked for. Zero for either is what Spotlight loads at first.
    #[serde(default)]
    pub playlists_total: usize,
    #[serde(default)]
    pub playlists_wanted: usize,
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
    /// Spotify's version of the playlist, which changes with every edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(shuffle: bool, repeat: &str) -> Modes {
        Modes { shuffle, repeat: repeat.into() }
    }

    #[test]
    fn a_list_starts_the_way_it_was_last_left() {
        let mut play_modes = PlayModes::default();
        assert_eq!(play_modes.for_list("spotify:playlist:a"), None);
        play_modes.remember(Some("spotify:playlist:a"), modes(true, "context"));
        play_modes.remember(Some("spotify:playlist:b"), modes(false, "off"));
        assert_eq!(play_modes.for_list("spotify:playlist:a"), Some(&modes(true, "context")));
        assert_eq!(play_modes.for_list("spotify:playlist:b"), Some(&modes(false, "off")));
    }

    #[test]
    fn a_new_list_starts_the_way_the_last_was_left() {
        let mut play_modes = PlayModes::default();
        play_modes.remember(Some("spotify:playlist:a"), modes(true, "track"));
        assert_eq!(play_modes.for_list("spotify:playlist:new"), Some(&modes(true, "track")));
        play_modes.remember(None, modes(false, "context"));
        assert_eq!(play_modes.for_list("spotify:playlist:new"), Some(&modes(false, "context")));
        assert_eq!(play_modes.for_list("spotify:playlist:a"), Some(&modes(true, "track")));
    }

    #[test]
    fn remembering_the_same_again_changes_nothing() {
        let mut play_modes = PlayModes::default();
        assert!(play_modes.remember(Some("a"), modes(true, "off")));
        assert!(!play_modes.remember(Some("a"), modes(true, "off")));
        assert!(play_modes.remember(Some("b"), modes(true, "off")));
        assert!(play_modes.remember(Some("a"), modes(true, "off")));
        assert_eq!(play_modes.lists.len(), 2);
    }

    #[test]
    fn only_so_many_lists_are_kept() {
        let mut play_modes = PlayModes::default();
        for n in 0..PlayModes::MOST_LISTS + 5 {
            play_modes.remember(Some(&format!("list {n}")), modes(false, "off"));
        }
        assert_eq!(play_modes.lists.len(), PlayModes::MOST_LISTS);
        assert_eq!(play_modes.lists[0].0, format!("list {}", PlayModes::MOST_LISTS + 4));
    }
}
