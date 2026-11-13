#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod art_tilt;
mod audio_output;
mod auth;
mod config;
mod crash_restart;
mod history;
mod home;
mod left_off;
mod lyrics;
mod media_controls;
mod media_keys;
mod memory_trim;
mod models;
mod playback;
mod spotify;
mod startup_entry;
mod taskbar;
mod tray;
mod updates;
mod wasapi;
mod window_chrome;

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, Mutex, OnceLock}, time::{Duration, Instant}};

use anyhow::{anyhow, Context, Result};
use media_keys::{MediaCommand, MediaKeyGuard};
use models::{AppConfig, CachedLikedSong, CachedPlaylist, FullTrack, LIKED_SONGS_PLAYLIST_URI, PlaybackState, TokenStore};
use single_instance::SingleInstance;
use slint::{CloseRequestResponse, Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::{runtime::Runtime, sync::mpsc};

slint::include_modules!();

const SEARCH_RESULTS_URI: &str = "spotlight:search-results";
/// The listening history page, which the sidebar marks as open by these.
const HISTORY_TITLE: &str = "Recently played";
const HISTORY_KICKER: &str = "History";
const MAX_EAGER_PLAYLIST_ARTWORK_ROWS: usize = 500;
const PLAYLIST_ARTWORK_WARM_LIMIT: usize = 100;
const QUEUE_ARTWORK_WARM_LIMIT: usize = 40;
const MAX_LOCAL_QUEUE_TRACKS: usize = 50;
/// Sending the queue to the player restarts the song's stream, so several edits in a row go as one.
/// How long after an edit a queue from Spotify can be from before Spotify heard about it.
const QUEUE_EDIT_SETTLE: Duration = Duration::from_secs(8);
const OPTIMISTIC_TRACK_TTL: Duration = Duration::from_secs(6);
const MISSING_PLAYBACK_GRACE: Duration = Duration::from_secs(3);
// Row thumbnails are 64px, so a large cache is only a few MB.
const MAX_DECODED_ALBUM_ART_IMAGES: usize = 256;
// The largest a playlist cover is drawn, the page header's 120px, in logical px.
const PLAYLIST_COVER_PX: f32 = 128.0;

thread_local! {
    static SPOTLIGHT_LOGO_IMAGE: RefCell<Option<slint::Image>> = RefCell::new(None);
    static STATIC_ICON_IMAGES: RefCell<HashMap<String, slint::Image>> = RefCell::new(HashMap::new());
    static ALBUM_ART_IMAGES: RefCell<HashMap<PathBuf, slint::Image>> = RefCell::new(HashMap::new());
    static PLAYLIST_COVERS: RefCell<HashMap<PathBuf, slint::Image>> = RefCell::new(HashMap::new());
}

static ALBUM_ART_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static RUNTIME_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
static ALBUM_ART_CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();
/// A newer release, once a check has found one.
static UPDATE_FOUND: Mutex<Option<updates::Release>> = Mutex::new(None);
/// Whether the daily check runs. Follows the setting.
static UPDATE_AUTO_CHECK: AtomicBool = AtomicBool::new(true);
/// A check or download is under way, so another waits.
static UPDATE_BUSY: AtomicBool = AtomicBool::new(false);

struct AppState {
    config: AppConfig,
    token: Option<TokenStore>,
    local_playback_token: Option<TokenStore>,
    /// Device that playback commands target. Follows whatever device Spotify
    /// reports as active; falls back to the user's pick, then this PC.
    device_id: Option<String>,
    local_device_id: Option<String>,
    /// Device Spotify currently reports as active (may be another app).
    active_device_id: Option<String>,
    preferred_device_id: Option<String>,
    available_device_ids: Vec<String>,
    available_device_names: Vec<String>,
    playlist_uris: Vec<String>,
    playlist_track_hrefs: Vec<String>,
    playlist_track_counts: Vec<Option<u32>>,
    track_uris: Vec<String>,
    playlist_track_cache: HashMap<String, Vec<FullTrack>>,
    /// The version of each library playlist its cached songs came from, so a playlist
    /// changed elsewhere is fetched again rather than shown as it was.
    playlist_cache_versions: HashMap<String, String>,
    /// When the songs of a Home card that isn't in the library were fetched. Mixes change
    /// under the same link, so they're fetched again once Home is.
    home_songs_at: HashMap<String, Instant>,
    selected_playlist_uri: String,
    active_queue_playlist_uri: String,
    current_track_uri: Option<String>,
    latest_local_track_uri: Option<String>,
    /// Whether the song this PC is playing has played long enough for Recently played.
    play_watch: history::Watch,
    /// The song in the player bar, whichever device is playing it.
    now_playing: Option<FullTrack>,
    /// Where the music was when Spotlight last closed, shown paused until it plays or something
    /// else does.
    parked: Option<left_off::LeftOff>,
    /// When play was last pressed on that song, while it's starting.
    picking_up_since: Option<Instant>,
    /// Shuffle and repeat as the player bar last showed them.
    left_off_modes: (bool, String),
    missing_playback_since: Option<Instant>,
    playback_clock: PlaybackClock,
    optimistic_playback_until: Option<Instant>,
    optimistic_is_playing: Option<bool>,
    optimistic_track_uri: Option<String>,
    optimistic_track_until: Option<Instant>,
    optimistic_shuffle_until: Option<Instant>,
    optimistic_shuffle_enabled: Option<bool>,
    optimistic_repeat_until: Option<Instant>,
    optimistic_repeat_mode: Option<String>,
    fast_playback_refresh_until: Option<Instant>,
    playback_engine: Option<playback::PlaybackEngine>,
    playback_engine_generation: u64,
    local_playback_start_lock: Arc<tokio::sync::Mutex<()>>,
    track_start_generation: u64,
    track_start_lock: Arc<tokio::sync::Mutex<()>>,
    /// The session whose library refresh is under way. One left over from an account that
    /// signed out doesn't hold up the next.
    library_refresh_in_flight: Option<u64>,
    /// How much of each long list of songs is loaded, by the list's link. Liked Songs too.
    loaded: HashMap<String, Loaded>,
    /// Numbers each `loaded` entry, so a part fetched for an older copy of a list isn't added
    /// to a newer one.
    load_stamp: u64,
    /// Goes up whenever the library's playlists change, so a refresh or a "Load more" that
    /// began before another change doesn't undo it.
    playlists_revision: u64,
    /// When the signed-in account was last asked for, while it isn't known yet.
    account_asked_at: Option<Instant>,
    next_allowed_library_refresh_at_epoch: u64,
    last_device_display: Vec<String>,
    last_queue_display: Vec<String>,
    last_queue_summary: String,
    queue_override_tracks: Vec<FullTrack>,
    queue_override_active: bool,
    /// The rows on screen, which is what a drag or a remove refers to.
    queue_painted_uris: Vec<String>,
    queue_edited_at: Option<Instant>,
    spotify_queue_tracks: Option<Vec<FullTrack>>,
    playback_refresh_lock: Arc<tokio::sync::Mutex<()>>,
    spotify_queue_current_uri: Option<String>,
    last_spotify_queue_refresh: Option<Instant>,
    device_switch_generation: u64,
    current_album_art_url: Option<String>,
    current_album_art_path: Option<PathBuf>,
    last_album_art_ui_path: Option<PathBuf>,
    previous_volume_percent: i32,
    muted: bool,
    last_volume_change: Option<Instant>,
    /// This PC's own volume. Other devices keep theirs, so controlling one never changes it.
    local_volume_percent: i32,
    /// Whether this PC's player is playing right now, from its own events. Spotify's
    /// report of the active device lags behind, and can still name the one before.
    local_playing: bool,
    track_view_generation: u64,
    /// Goes up at each sign-out, so what was asked for the account that left can tell when it
    /// comes back.
    session: u64,
}

/// How much of a long list of songs is loaded. Spotify sends a list a part at a time, and
/// Spotlight loads the rest of a long one only when asked.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Loaded {
    /// How long the whole list is, as Spotify counts it.
    total: usize,
    /// How far into Spotify's list was asked for.
    wanted: usize,
    /// Where the next part starts in Spotify's list, while there's more.
    more_from: Option<usize>,
    /// Which loading this was, so two copies of a list that look alike can be told apart.
    stamp: u64,
}

/// Notes how much of a list is now loaded, numbered afresh.
fn mark_loaded(guard: &mut AppState, list_uri: &str, mut loaded: Loaded) {
    guard.load_stamp = guard.load_stamp.wrapping_add(1);
    loaded.stamp = guard.load_stamp;
    guard.loaded.insert(list_uri.to_string(), loaded);
}

/// How far into a list of songs to load: as far as was asked for, and at least the first part.
fn songs_wanted(guard: &AppState, list_uri: &str) -> usize {
    guard.loaded.get(list_uri).map_or(0, |loaded| loaded.wanted).max(spotify::SONGS_PER_PART)
}

/// How many songs Liked Songs has, though only the first are kept between runs.
fn liked_count(snapshot: &models::LibrarySnapshot) -> usize {
    snapshot.liked_songs_total.max(snapshot.liked_songs.len())
}

fn cached_playlist(item: &models::PlaylistItem) -> CachedPlaylist {
    CachedPlaylist {
        name: item.name.clone(),
        uri: item.uri.clone(),
        image_url: item.images.first().map(|image| image.url.clone()),
        track_count: item.tracks.total,
        snapshot_id: item.snapshot_id.clone(),
    }
}

impl AppState {
    /// True when this PC's librespot player is the active Spotify device, so
    /// commands can go straight to it. librespot silently ignores commands
    /// while it is not active, so anything else must use the Web API.
    /// True when the volume slider acts on this PC: it's playing here, or nothing is
    /// playing anywhere, so the next play starts here.
    fn local_volume_is_target(&self) -> bool {
        self.local_playing || self.local_is_active() || self.active_device_id.is_none()
    }

    fn local_is_active(&self) -> bool {
        self.local_device_id.is_some()
            && self.active_device_id == self.local_device_id
            && self.playback_engine.as_ref().is_some_and(|engine| !engine.is_finished())
    }

    /// True when the selected target is this PC (active or idle).
    fn local_is_selected(&self) -> bool {
        self.device_id.is_some() && self.device_id == self.local_device_id
    }
}

#[derive(Clone, Debug)]
struct PlaybackClock {
    progress_ms: i32,
    duration_ms: i32,
    is_playing: bool,
    last_sync: Instant,
}

#[derive(Clone, Copy)]
enum LocalPlayerAction {
    PlayPause,
    Next,
    /// Next, playing even if it was paused. Sent together so nothing can come between.
    NextAndPlay,
    Previous,
}

#[derive(Clone, Copy)]
enum NowPlayingUpdate {
    Optimistic,
    Enrichment,
    Authoritative,
}

impl LocalPlayerAction {
    fn run(self, engine: &playback::PlaybackEngine) -> Result<()> {
        match self {
            Self::PlayPause => engine.play_pause(),
            Self::Next => engine.next(),
            Self::NextAndPlay => engine.next().and_then(|()| engine.play()),
            Self::Previous => engine.previous(),
        }
    }
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self {
            progress_ms: 0,
            duration_ms: 0,
            is_playing: false,
            last_sync: Instant::now(),
        }
    }
}

impl PlaybackClock {
    fn displayed_progress_ms(&self) -> i32 {
        if !self.is_playing {
            return self.progress_ms.clamp(0, self.duration_ms.max(0));
        }
        let elapsed = self.last_sync.elapsed().as_millis() as i32;
        (self.progress_ms + elapsed).clamp(0, self.duration_ms.max(0))
    }

    fn sync(&mut self, progress_ms: i32, duration_ms: i32, is_playing: bool) {
        self.progress_ms = progress_ms.max(0);
        self.duration_ms = duration_ms.max(0);
        self.is_playing = is_playing;
        self.last_sync = Instant::now();
    }
}

/// Writes what went wrong to `crash.log` in the settings folder when Spotlight panics, then
/// opens it again. Most panics happen inside Windows' callbacks, which can't unwind, so they end
/// the app with nothing on screen to say why.
fn keep_crash_log() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        append_crash_log(&format!("thread {}:\n{info}", thread.name().unwrap_or("unnamed")));
        // Release builds abort after any panic, so this copy is going whatever happens next.
        if cfg!(panic = "abort") {
            crash_restart::restart();
        }
        previous(info);
    }));
}

/// Adds an entry to `crash.log`. Only the last few entries are kept.
pub(crate) fn append_crash_log(entry: &str) {
    let Ok(dir) = config::config_dir() else { return };
    let path = dir.join("crash.log");
    let mut log = std::fs::read_to_string(&path).unwrap_or_default();
    if log.len() > 24 * 1024 {
        let mut cut = log.len() - 12 * 1024;
        // Song titles in other languages take several bytes a letter.
        while !log.is_char_boundary(cut) {
            cut += 1;
        }
        let start = log[cut..].find("\n\n").map_or(cut, |at| cut + at + 2);
        log.drain(..start.min(log.len()));
    }
    log.push_str(&format!(
        "{} UTC, Spotlight {}, {entry}\n\n",
        utc_timestamp(std::time::SystemTime::now()),
        updates::CURRENT_VERSION,
    ));
    let _ = std::fs::write(&path, log);
}

/// "2026-11-09 19:08:08", in UTC.
fn utc_timestamp(time: std::time::SystemTime) -> String {
    let seconds = time.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Days since 1970 to a calendar date (Howard Hinnant's method).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rest / 3_600, rest / 60 % 60, rest % 60)
}

fn main() -> Result<()> {
    keep_crash_log();
    if std::env::args().skip(1).any(|arg| arg == "--quit") {
        // The installer asks a running Spotlight to quit before it replaces or removes it.
        window_chrome::quit_running();
        return Ok(());
    }
    let args = std::env::args().collect::<Vec<_>>();
    // After a crash, the new copy waits here for the old one to be gone.
    let after_crash = crash_restart::start(&args);
    // After a portable update, the new version waits here for the old one to quit.
    updates::finish_update(&args);
    // The portable copy leaves Windows as it found it.
    if !config::is_portable() {
        startup_entry::register();
    }
    let at_sign_in = std::env::args().skip(1).any(|arg| arg == startup_entry::AT_SIGN_IN_ARG);
    let instance = SingleInstance::new("SpotlightDesktopSingleton").context("single-instance init failed")?;
    if !instance.is_single() {
        // Opening it again brings back the copy that's running, even from the tray. Windows
        // starting it at sign-in hasn't been asked to show anything.
        if !at_sign_in {
            window_chrome::show_running();
        }
        return Ok(());
    }
    memory_trim::start();

    let runtime = Arc::new(tokio::runtime::Builder::new_multi_thread().worker_threads(3).enable_all().build()?);
    let _ = RUNTIME_HANDLE.set(runtime.handle().clone());
    let config = config::load_config()?;
    let next_allowed_library_refresh_at_epoch = config.library_snapshot.next_allowed_refresh_at_epoch;
    let cached_playlist_uris = config
        .library_snapshot
        .playlists
        .iter()
        .map(|item| item.uri.clone())
        .collect::<Vec<_>>();
    // Not saved between runs. Blank until the library is fetched, as the list's own link works too.
    let cached_playlist_track_hrefs = vec![String::new(); cached_playlist_uris.len()];
    let cached_playlist_track_counts = config
        .library_snapshot
        .playlists
        .iter()
        .map(|item| item.track_count)
        .collect::<Vec<_>>();
    let local_volume_percent = if config.remember_volume { config.last_volume_percent.clamp(0, 100) } else { 100 };
    let cached_liked_uris = config
        .library_snapshot
        .liked_songs
        .iter()
        .map(|item| item.uri.clone())
        .collect::<Vec<_>>();
    let cached_liked_tracks = liked_tracks_from_snapshot(&config);
    // Only the first of Liked Songs is kept between runs, and the rest loads when asked for.
    let loaded_liked = {
        let snapshot = &config.library_snapshot;
        (snapshot.liked_songs_total > snapshot.liked_songs.len()).then_some(Loaded {
            total: snapshot.liked_songs_total,
            wanted: spotify::SONGS_PER_PART,
            more_from: Some(snapshot.liked_songs.len()),
            stamp: 0,
        })
    };
    let state = Arc::new(Mutex::new(AppState {
        token: config.token.clone(),
        local_playback_token: config.local_playback_token.clone(),
        config,
        device_id: None,
        local_device_id: None,
        active_device_id: None,
        preferred_device_id: None,
        available_device_ids: Vec::new(),
        available_device_names: Vec::new(),
        playlist_uris: cached_playlist_uris,
        playlist_track_hrefs: cached_playlist_track_hrefs,
        playlist_track_counts: cached_playlist_track_counts,
        track_uris: cached_liked_uris.clone(),
        playlist_track_cache: HashMap::from([(LIKED_SONGS_PLAYLIST_URI.to_string(), cached_liked_tracks)]),
        playlist_cache_versions: HashMap::new(),
        home_songs_at: HashMap::new(),
        selected_playlist_uri: LIKED_SONGS_PLAYLIST_URI.to_string(),
        active_queue_playlist_uri: LIKED_SONGS_PLAYLIST_URI.to_string(),
        current_track_uri: None,
        latest_local_track_uri: None,
        play_watch: history::Watch::default(),
        now_playing: None,
        parked: None,
        picking_up_since: None,
        left_off_modes: (false, "off".to_string()),
        missing_playback_since: None,
        playback_clock: PlaybackClock::default(),
        optimistic_playback_until: None,
        optimistic_is_playing: None,
        optimistic_track_uri: None,
        optimistic_track_until: None,
        optimistic_shuffle_until: None,
        optimistic_shuffle_enabled: None,
        optimistic_repeat_until: None,
        optimistic_repeat_mode: None,
        fast_playback_refresh_until: None,
        playback_engine: None,
        playback_engine_generation: 0,
        local_playback_start_lock: Arc::new(tokio::sync::Mutex::new(())),
        track_start_generation: 0,
        track_start_lock: Arc::new(tokio::sync::Mutex::new(())),
        library_refresh_in_flight: None,
        next_allowed_library_refresh_at_epoch,
        last_device_display: Vec::new(),
        last_queue_display: Vec::new(),
        last_queue_summary: "Queue unavailable until playback starts".to_string(),
        queue_override_tracks: Vec::new(),
        queue_override_active: false,
        queue_painted_uris: Vec::new(),
        queue_edited_at: None,
        spotify_queue_tracks: None,
        playback_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        spotify_queue_current_uri: None,
        last_spotify_queue_refresh: None,
        device_switch_generation: 0,
        current_album_art_url: None,
        current_album_art_path: None,
        last_album_art_ui_path: None,
        previous_volume_percent: 100,
        muted: false,
        last_volume_change: None,
        local_volume_percent,
        local_playing: false,
        track_view_generation: 0,
        session: 0,
        loaded: loaded_liked.into_iter().map(|loaded| (LIKED_SONGS_PLAYLIST_URI.to_string(), loaded)).collect(),
        load_stamp: 0,
        playlists_revision: 0,
        account_asked_at: None,
    }));

    let ui = AppWindow::new()?;
    #[cfg(target_os = "windows")]
    apply_runtime_window_icon();
    {
        let guard = state.lock().unwrap();
        ui.set_client_id(guard.config.client_id.clone().into());
        ui.set_redirect_uri(guard.config.redirect_uri.clone().into());
        ui.set_artwork_enabled(guard.config.artwork_enabled);
        ui.set_show_artist_column(guard.config.show_artist_column);
        ui.set_show_time_column(guard.config.show_time_column);
        ui.set_compact_sidebars(guard.config.compact_sidebars);
        // The same limits a drag has.
        if guard.config.library_width > 0 {
            ui.set_library_width(guard.config.library_width.clamp(240, 420) as f32);
        }
        ui.set_mini_on_top(guard.config.mini_on_top);
        if guard.config.panel_width > 0 {
            ui.set_panel_width(guard.config.panel_width.clamp(300, 520) as f32);
        }
        ui.set_theme_mode(guard.config.theme_mode.clone().into());
        ui.set_accent_color(guard.config.accent_color.clone().into());
        ui.set_audio_quality(guard.config.audio_quality.clone().into());
        ui.set_output_device(guard.config.output_device.clone().into());
        audio_output::choose_device(&guard.config.output_device);
        ui.set_normalize_volume(guard.config.normalize_volume);
        ui.set_autoplay_similar(guard.config.autoplay_similar);
        ui.set_crossfade_seconds(guard.config.crossfade_seconds.min(MAX_CROSSFADE_SECONDS) as i32);
        set_crossfade(guard.config.crossfade_seconds);
        ui.set_remember_volume(guard.config.remember_volume);
        ui.set_global_hotkeys(guard.config.global_hotkeys);
        ui.set_media_keys_enabled(guard.config.media_keys_enabled);
        ui.set_play_pause_hotkey(guard.config.play_pause_hotkey.clone().into());
        ui.set_next_hotkey(guard.config.next_hotkey.clone().into());
        ui.set_previous_hotkey(guard.config.previous_hotkey.clone().into());
        ui.set_shuffle_hotkey(guard.config.shuffle_hotkey.clone().into());
        ui.set_repeat_hotkey(guard.config.repeat_hotkey.clone().into());
        ui.set_minimize_to_tray(guard.config.minimize_to_tray);
        ui.set_close_to_tray(guard.config.close_to_tray);
        ui.set_start_minimized(guard.config.start_minimized);
        ui.set_tray_at_sign_in(guard.config.tray_at_sign_in);
        ui.set_low_resource_mode(guard.config.low_resource_mode);
        ui.set_reduce_animations(guard.config.reduce_animations);
        ui.set_disable_background_effects(guard.config.disable_background_effects);
        ui.set_album_colors(guard.config.album_colors);
        ui.set_log_level(guard.config.log_level.clone().into());
        ui.set_developer_mode(guard.config.developer_mode);
        ui.set_check_for_updates(guard.config.check_for_updates);
        ui.set_volume_percent(if guard.config.remember_volume { guard.config.last_volume_percent.clamp(0, 100) } else { 100 });
        ui.set_spotlight_logo(load_spotlight_logo());
        ui.set_album_art(slint::Image::default());
        ui.set_play_icon(load_icon("play.svg"));
        ui.set_pause_icon(load_icon("pause.svg"));
        ui.set_previous_icon(load_icon("previous.svg"));
        ui.set_next_icon(load_icon("next.svg"));
        ui.set_search_icon(load_icon("search.svg"));
        ui.set_refresh_icon(load_icon("refresh.svg"));
        ui.set_spinner_icon(load_icon("spinner.svg"));
        ui.set_settings_icon(load_icon("settings.svg"));
        ui.set_playlist_icon(load_icon("playlist.svg"));
        ui.set_device_icon(load_icon("device.svg"));
        ui.set_queue_icon(load_icon("queue.svg"));
        ui.set_shuffle_icon(load_icon("shuffle.svg"));
        ui.set_repeat_icon(load_icon("repeat.svg"));
        ui.set_volume_icon(load_icon("volume.svg"));
        ui.set_volume_low_icon(load_icon("volume-low.svg"));
        ui.set_mute_icon(load_icon("volume-mute.svg"));
        ui.set_repeat_one_icon(load_icon("repeat-one.svg"));
        ui.set_fullscreen_icon(load_icon("fullscreen.svg"));
        ui.set_plus_icon(load_icon("plus.svg"));
        ui.set_lyrics_icon(load_icon("lyrics.svg"));
        ui.set_mini_player_icon(load_icon("mini-player.svg"));
        ui.set_history_art(load_icon("recently-played.svg"));
        apply_theme_to_ui(&ui);
        ui.set_signed_in(guard.token.is_some());
        // Nobody signed in yet: the setup guide opens, at the start or, with a Client ID
        // already given, where that's used.
        if !ui.get_signed_in() {
            ui.set_onboarding_step(if guard.config.client_id.trim().is_empty() { 1 } else { 3 });
        }
        ui.set_player_ready(guard.local_playback_token.is_some());
        ui.set_resume_on_startup(guard.config.resume_on_startup);
        ui.set_local_device_name(guard.config.device_name.clone().into());
        if guard.token.is_some() {
            ui.set_connection_status("Restoring saved session...".into());
        }
        if guard.local_playback_token.is_some() {
            ui.set_player_status("Starting local player...".into());
        }
    }

    apply_cached_library_to_ui(&ui, &state);

    let spotify_api = spotify::SpotifyApi::new();
    let (media_tx, mut media_rx) = mpsc::unbounded_channel::<MediaCommand>();
    media_controls::set_commands(media_tx.clone());
    let hotkeys = {
        let guard = state.lock().unwrap();
        let bindings = hotkey_bindings(&guard.config);
        media_controls::set_enabled(guard.config.media_keys_enabled);
        if guard.config.global_hotkeys {
            match MediaKeyGuard::install(media_tx.clone(), &bindings) {
                Ok(installed) => {
                    ui.set_hotkey_warning(installed.warnings.join("\n").into());
                    Some(installed)
                }
                Err(error) => {
                    ui.set_hotkey_warning(format!("Shortcuts unavailable: {error}").into());
                    None
                }
            }
        } else {
            None
        }
    };
    let hotkeys_guard = Arc::new(Mutex::new(hotkeys));
    if state.lock().unwrap().config.close_to_tray || state.lock().unwrap().config.minimize_to_tray {
        tray::ensure_started();
    }

    {
        let ui_weak = ui.as_weak();
        tray::set_restore_handler(move || {
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.show();
                    ui.window().set_minimized(false);
                }
            });
        });
    }

    {
        let ui_weak = ui.as_weak();
        let state_for_close = Arc::clone(&state);
        ui.window().on_close_requested(move || {
            let ui_wants_tray = ui_weak
                .upgrade()
                .map(|ui| ui.get_close_to_tray() || ui.get_minimize_to_tray());
            let config_wants_tray = {
                let guard = state_for_close.lock().unwrap();
                guard.config.close_to_tray || guard.config.minimize_to_tray
            };
            let hide_to_tray = ui_wants_tray
                .map(|enabled| enabled || config_wants_tray)
                .unwrap_or(config_wants_tray);
            if hide_to_tray {
                tray::ensure_started();
                tray::hide_spotlight_window();
                CloseRequestResponse::KeepWindowShown
            } else {
                config::exit_after_saving();
            }
        });
    }

    bind_callbacks(
        &ui,
        Arc::clone(&runtime),
        Arc::clone(&state),
        spotify_api.clone(),
        media_tx,
        Arc::clone(&hotkeys_guard),
    );
    bind_updates(&ui, &runtime, spotify_api.http().clone(), state.lock().unwrap().config.check_for_updates);

    {
        let ui_weak = ui.as_weak();
        runtime.spawn(async move {
            // Route keys through the same callbacks as the on-screen buttons so
            // they control whichever device is actually playing.
            while let Some(command) = media_rx.recv().await {
                let ui_weak_command = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_command.upgrade() {
                        match command {
                            MediaCommand::PlayPause => ui.invoke_play_pause(),
                            MediaCommand::Next => ui.invoke_next_track(),
                            MediaCommand::Previous => ui.invoke_previous_track(),
                            MediaCommand::Shuffle => ui.invoke_toggle_shuffle(),
                            MediaCommand::Repeat => ui.invoke_cycle_repeat(),
                            MediaCommand::SeekTo(ms) if !ui.get_current_track_uri().is_empty() => {
                                let ms = ms.min(ui.get_progress_max_ms());
                                ui.set_progress_ms(ms);
                                ui.set_elapsed(format_time(ms).into());
                                ui.invoke_seek_to(ms);
                            }
                            MediaCommand::SeekTo(_) => {}
                        }
                    }
                });
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        runtime.spawn(async move {
            // Just after opening, music may be going on another device, and at sign-in the
            // network can be late, so look often for the first minute.
            const STARTUP_WATCH: Duration = Duration::from_secs(60);
            const STARTUP_INTERVAL: Duration = Duration::from_secs(2);
            let opened_at = Instant::now();
            let mut last_poll = Instant::now();
            // Checks that failed in a row, and what the title bar says about it.
            let mut misses = 0u32;
            let mut trouble_shown = "";
            loop {
                let (interval, playback_only) = {
                    let guard = state.lock().unwrap();
                    let foreground = spotlight_window_is_foreground();
                    let low_resource = guard.config.low_resource_mode;
                    if guard.token.is_none() {
                        (Duration::from_secs(30), false)
                    } else if guard
                        .fast_playback_refresh_until
                        .is_some_and(|until| Instant::now() < until)
                    {
                        (Duration::from_millis(if foreground { 500 } else if low_resource { 1_000 } else { 750 }), true)
                    } else if guard.playback_clock.is_playing && guard.local_playing && guard.local_is_active() {
                        // This PC is the one playing. Its player's own events keep the bar
                        // right, the next song included, and it hears at once when another
                        // device takes over. Spotify is asked now and then for the rest, such as
                        // songs queued from a phone.
                        let secs = match (foreground, low_resource) {
                            (true, false) => 5,
                            (true, true) => 8,
                            (false, false) => 15,
                            (false, true) => 30,
                        };
                        (Duration::from_secs(secs), true)
                    } else if guard.playback_clock.is_playing {
                        let remaining_ms = guard
                            .playback_clock
                            .duration_ms
                            .saturating_sub(guard.playback_clock.displayed_progress_ms());
                        if guard.playback_clock.duration_ms > 0 && remaining_ms <= 6_000 {
                            (Duration::from_millis(if low_resource { 600 } else { 400 }), true)
                        } else if foreground {
                            (Duration::from_millis(if low_resource { 1_500 } else { 1_000 }), true)
                        } else {
                            (Duration::from_secs(if low_resource { 10 } else { 3 }), true)
                        }
                    } else if foreground {
                        (Duration::from_secs(if low_resource { 5 } else { 3 }), true)
                    } else {
                        (Duration::from_secs(if low_resource { 30 } else { 8 }), true)
                    }
                };
                let interval = if playback_only && opened_at.elapsed() < STARTUP_WATCH {
                    interval.min(STARTUP_INTERVAL)
                } else {
                    interval
                };
                // The wait is worked out again every second, so bringing the window to the
                // front catches up at once rather than sitting out a long background wait.
                let remaining = interval.saturating_sub(last_poll.elapsed());
                if !remaining.is_zero() {
                    tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
                    continue;
                }
                let signed_in = state.lock().unwrap().token.is_some();
                // Asking again before Spotify said to, for this or any other request, only
                // makes it wait longer.
                if let Some(wait) = spotify::rate_limit_wait() {
                    if signed_in {
                        show_sync_trouble(&ui_weak, &mut trouble_shown, RATE_LIMITED);
                    }
                    tokio::time::sleep(wait).await;
                    continue;
                }
                last_poll = Instant::now();
                if !signed_in {
                    misses = 0;
                    show_sync_trouble(&ui_weak, &mut trouble_shown, "");
                    continue;
                }
                let result = if playback_only {
                    refresh_playback_only(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await
                } else {
                    refresh_everything(ui_weak.clone(), Arc::clone(&state), spotify_api.clone(), false, false, false)
                        .await
                        .map(|_| ())
                };
                // Signed out while it was out: what it found is about the account that left.
                if state.lock().unwrap().token.is_some() {
                    show_sync_trouble(&ui_weak, &mut trouble_shown, sync_trouble(&result, &mut misses));
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        runtime.spawn(async move {
            loop {
                let foreground = spotlight_window_is_foreground();
                let low_resource = state.lock().unwrap().config.low_resource_mode;
                tokio::time::sleep(if foreground {
                    Duration::from_millis(if low_resource { 500 } else { 250 })
                } else {
                    Duration::from_secs(1)
                }).await;
                let (progress_ms, duration_ms, is_playing, playing_here) = {
                    let guard = state.lock().unwrap();
                    (
                        guard.playback_clock.displayed_progress_ms(),
                        guard.playback_clock.duration_ms,
                        guard.playback_clock.is_playing,
                        guard.local_playing && guard.local_is_active(),
                    )
                };
                if !is_playing || (duration_ms <= 0 && progress_ms <= 0) || (low_resource && !foreground) {
                    continue;
                }
                // This PC's player says itself when the next song starts, so Spotify isn't asked.
                let should_refresh_transition =
                    !playing_here && is_playing && duration_ms > 0 && duration_ms.saturating_sub(progress_ms) <= 250;
                let _ = slint::invoke_from_event_loop({
                    let ui_weak = ui_weak.clone();
                    let state = Arc::clone(&state);
                    move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            if should_refresh_transition {
                                begin_fast_playback_refresh(&state, Duration::from_secs(4));
                            } else {
                                show_progress(&ui, progress_ms);
                            }
                        }
                    }
                });
            }
        });
    }

    {
        let (volume_tx, mut volume_rx) = mpsc::unbounded_channel::<i32>();
        let (seek_tx, mut seek_rx) = mpsc::unbounded_channel::<i32>();
        bind_debounced_slider_callbacks(&ui, Arc::clone(&state), volume_tx, seek_tx);

        let ui_weak = ui.as_weak();
        let runtime = Arc::clone(&runtime);
        let volume_state = Arc::clone(&state);
        let volume_api = spotify_api.clone();
        runtime.spawn(async move {
            while let Some(mut value) = volume_rx.recv().await {
                while let Ok(next) = volume_rx.try_recv() {
                    value = next;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                while let Ok(next) = volume_rx.try_recv() {
                    value = next;
                }
                let value = value.clamp(0, 100);
                {
                    let mut guard = volume_state.lock().unwrap();
                    guard.last_volume_change = Some(Instant::now());
                    guard.muted = value == 0;
                    let local = guard.local_volume_is_target();
                    if local {
                        guard.local_volume_percent = value;
                    }
                    if value > 0 {
                        guard.previous_volume_percent = value;
                        // Only this PC's volume is remembered for the next start.
                        if local && guard.config.remember_volume {
                            guard.config.last_volume_percent = value;
                            config::save_config_background(&guard.config);
                        }
                    }
                };
                let result = apply_volume_change(Arc::clone(&volume_state), volume_api.clone(), value).await;
                let _ = slint::invoke_from_event_loop({
                    let ui_weak = ui_weak.clone();
                    move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_muted(value == 0);
                            if value > 0 {
                                ui.set_previous_volume_percent(value);
                            }
                            if let Err(error) = result {
                                append_log(&ui, &format!("Volume change failed: {error}"));
                            }
                        }
                    }
                });
            }
        });
        let ui_weak = ui.as_weak();
        let seek_state = Arc::clone(&state);
        let seek_api = spotify_api.clone();
        runtime.spawn(async move {
            while let Some(mut value) = seek_rx.recv().await {
                // Only sent when the pointer is let go, so nothing to wait for.
                while let Ok(next) = seek_rx.try_recv() {
                    value = next;
                }
                let mut value = value.max(0);
                let (duration_ms, is_playing) = {
                    let mut guard = seek_state.lock().unwrap();
                    let duration_ms = guard.playback_clock.duration_ms.max(0);
                    value = value.clamp(0, duration_ms.max(1));
                    let is_playing = guard.playback_clock.is_playing;
                    guard.playback_clock.sync(value, duration_ms, is_playing);
                    guard.fast_playback_refresh_until = Some(Instant::now() + Duration::from_secs(2));
                    (duration_ms, is_playing)
                };
                let result = apply_seek_change(Arc::clone(&seek_state), seek_api.clone(), value).await;
                if result.is_ok() {
                    for _ in 0..1 {
                        tokio::time::sleep(Duration::from_millis(350)).await;
                        let _ = refresh_playback_only(
                            ui_weak.clone(),
                            Arc::clone(&seek_state),
                            seek_api.clone(),
                        )
                        .await;
                    }
                }
                let _ = slint::invoke_from_event_loop({
                    let ui_weak = ui_weak.clone();
                    move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            match result {
                                Ok(()) => {
                                    ui.set_elapsed(format_time(value).into());
                                    ui.set_duration(format_time(duration_ms).into());
                                    ui.set_is_playing(is_playing);
                                }
                                Err(error) => append_log(&ui, &format!("Seek failed: {error}")),
                            }
                        }
                    }
                });
            }
        });
    }

    // Where the music was when Spotlight last closed, paused, until it plays or something else
    // does.
    if state.lock().unwrap().token.is_some() {
        if let Some(left_off) = left_off::load() {
            let art_url = left_off.track.album.images.first().map(|image| image.url.clone());
            show_left_off(&ui, &state, left_off);
            // A cover that wasn't saved comes a moment later.
            if let Some(url) = art_url.filter(|url| cached_album_art_path(url).is_none()) {
                let ui_weak = ui.as_weak();
                let state = Arc::clone(&state);
                runtime.spawn(async move {
                    let Ok(path) = load_album_art(&url).await else { return };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        let still_shown = state
                            .lock()
                            .unwrap()
                            .parked
                            .as_ref()
                            .is_some_and(|parked| parked.track.album.images.first().is_some_and(|image| image.url == url));
                        if still_shown {
                            set_album_art_if_changed(&ui, &state, Some(path));
                        }
                    });
                });
            }
        }
    }

    if state.lock().unwrap().token.is_some() {
        let ui_weak = ui.as_weak();
        let runtime_for_pick_up = Arc::clone(&runtime);
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        runtime.spawn(async move {
            let load_library_on_startup = should_refresh_library_at_startup(&state);
            let local_start = ensure_local_playback(Arc::clone(&state), ui_weak.clone());
            let session_restore = refresh_everything(
                ui_weak.clone(),
                Arc::clone(&state),
                spotify_api.clone(),
                load_library_on_startup,
                true,
                false,
            );
            // The full restore can page through the whole library before it asks what's playing,
            // so ask straight away too: music already going on a phone shows up at once.
            let playing_now = refresh_playback_only(ui_weak.clone(), Arc::clone(&state), spotify_api.clone());
            let (local_result, restore_result, _) = tokio::join!(local_start, session_restore, playing_now);

            if let Err(error) = local_result {
                let ui_weak_error = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_error.upgrade() {
                        ui.set_player_status("Local player failed".into());
                        append_log(&ui, &format!("Startup local playback restore failed: {error}"));
                    }
                });
            }

            if let Err(error) = restore_result {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_connection_status("Restore failed".into());
                        append_log(&ui, &format!("Startup session restore failed: {error}"));
                    }
                });
                return;
            }

            let resume = state.lock().unwrap().config.resume_on_startup;
            // Music that was playing on this PC when Spotlight crashed plays on after the
            // restart. Opened by hand, it waits for play unless the setting says otherwise.
            let carry_on = after_crash.is_some_and(|left| left.was_playing);
            // With nothing playing anywhere, resuming carries on from where the music was left.
            let pick_up = (resume || carry_on) && state.lock().unwrap().parked.is_some();
            let _ = refresh_until_local_device_ready(ui_weak.clone(), Arc::clone(&state), spotify_api.clone(), resume && !pick_up).await;
            if !pick_up {
                return;
            }
            if state.lock().unwrap().parked.is_some() {
                let _ = slint::invoke_from_event_loop(move || {
                    if state.lock().unwrap().parked.is_some() {
                        pick_up_left_off(&ui_weak, &runtime_for_pick_up, &state, PickUp::WhereItWas);
                    }
                });
            } else if resume {
                // Something started elsewhere meanwhile, which resuming moves here as before.
                // Carrying on after a crash leaves it where it is.
                let _ = refresh_everything(ui_weak, state, spotify_api, false, false, true).await;
            }
        });
    }

    // A restart after a crash comes back the way the window was. Otherwise the settings say:
    // started by Windows at sign-in, or set to start minimized with the tray in use, it waits
    // in the tray.
    let (to_tray, minimized) = match after_crash {
        Some(left) => (left.was_in_tray, left.was_minimized),
        None => {
            let guard = state.lock().unwrap();
            let tray_in_use = guard.config.close_to_tray || guard.config.minimize_to_tray;
            let start_minimized = guard.config.start_minimized;
            ((at_sign_in && guard.config.tray_at_sign_in) || (start_minimized && tray_in_use), start_minimized && !tray_in_use)
        }
    };
    if to_tray {
        // The window is made but not shown: the tray brings it out. Hiding it once the event
        // loop has shown it would flash it on the screen first.
        tray::ensure_started();
        i_slint_backend_winit::keep_first_window_hidden();
    }

    {
        ui.on_caption_layout(window_chrome::set_caption_layout);
        ui.on_minimize_window(window_chrome::minimize);
        ui.on_close_window(window_chrome::close);

        let ui_weak = ui.as_weak();
        ui.on_stage_tilt_redraw(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().request_redraw();
            }
        });

        // The bar stood still while hidden. It catches up from the clock before the first frame.
        let ui_weak = ui.as_weak();
        let state = Arc::clone(&state);
        ui.on_window_shown(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (progress_ms, duration_ms) = {
                let guard = state.lock().unwrap();
                (guard.playback_clock.displayed_progress_ms(), guard.playback_clock.duration_ms)
            };
            if duration_ms > 0 || progress_ms > 0 {
                show_progress(&ui, progress_ms);
            }
        });

        // Slint creates the native window some time after the event loop
        // starts, so keep trying briefly until it exists. `ui.run()` shows the window, so going
        // to the taskbar waits for it too. Going to the tray kept it hidden already, and hiding
        // it again changes nothing. The window stays made while it's in the tray, so the media
        // keys and Windows' media controls still reach it.
        let ui_weak = ui.as_weak();
        retry_chrome_install(ui_weak, 500, move |ui| {
            if to_tray {
                tray::hide_spotlight_window();
            } else if minimized {
                ui.window().set_minimized(true);
            }
        });
    }

    if ui.get_signed_in() {
        ui.invoke_open_home();
    }

    // While music plays, its place is saved every so often, so a crash or a shutdown that skips
    // the usual way out still opens close to where it was.
    let left_off_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let state = Arc::clone(&state);
        left_off_timer.start(slint::TimerMode::Repeated, Duration::from_secs(15), move || {
            if let Some(ui) = ui_weak.upgrade().filter(|ui| ui.get_is_playing()) {
                note_left_off(&ui, &state);
            }
        });
    }
    {
        let state = Arc::clone(&state);
        crash_restart::save_place_with(move || {
            if let Some(left_off) = left_off_now(&state) {
                let _ = left_off::save(&left_off);
            }
        });
    }
    {
        let state = Arc::clone(&state);
        config::before_exit(move || {
            if let Some(left_off) = left_off_now(&state) {
                let _ = left_off::save(&left_off);
            }
        });
    }

    window_chrome::center_on_screen(&ui);
    window_chrome::install_frame_driver(&ui);
    let ran = ui.run();
    if let Err(error) = &ran {
        // The window couldn't be drawn, as when the graphics driver is part way through updating.
        append_crash_log(&format!("the event loop failed: {error}"));
        crash_restart::restart();
    }
    crash_restart::quitting();
    ran.context("slint event loop failed")
}

/// Fits the title bar and the rest once the native window exists, then does `then`.
fn retry_chrome_install(ui_weak: slint::Weak<AppWindow>, attempts_left: u32, then: impl FnOnce(&AppWindow) + 'static) {
    slint::Timer::single_shot(Duration::from_millis(10), move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        if window_chrome::install(&ui) || attempts_left == 0 {
            then(&ui);
        } else {
            retry_chrome_install(ui_weak, attempts_left - 1, then);
        }
    });
}

fn bind_debounced_slider_callbacks(
    ui: &AppWindow,
    state: Arc<Mutex<AppState>>,
    volume_tx: mpsc::UnboundedSender<i32>,
    seek_tx: mpsc::UnboundedSender<i32>,
) {
    ui.on_set_volume(move |value| {
        // Heard at once when it's this PC's. Telling Spotify and saving it can wait for
        // the drag to end. Another device's volume never touches this PC's.
        if state.lock().unwrap().local_volume_is_target() {
            playback::set_volume_now(value.clamp(0, 100) as u16);
        }
        let _ = volume_tx.send(value);
    });

    ui.on_seek_to(move |value| {
        let _ = seek_tx.send(value);
    });
}

fn apply_theme_to_ui(ui: &AppWindow) {
    let accent = match ui.get_accent_color().as_str() {
        "Blue" => rgb(78, 156, 255),
        "Purple" => rgb(171, 132, 255),
        "White" => rgb(230, 230, 230),
        _ => rgb(30, 215, 96),
    };
    let light = matches!(ui.get_theme_mode().as_str(), "Light")
        || (matches!(ui.get_theme_mode().as_str(), "System") && system_prefers_light());
    // A white accent would vanish on light surfaces; it becomes its inverse.
    let mono_on_light = light && ui.get_accent_color() == "White";
    let accent = if mono_on_light { rgb(24, 24, 27) } else { accent };
    AppTheme::get(ui).set_on_accent(if mono_on_light { rgb(250, 250, 250) } else { rgb(11, 11, 12) });
    let theme = AppTheme::get(ui);
    theme.set_reduce_motion(ui.get_reduce_animations());
    window_chrome::set_smooth_scrolling(!ui.get_reduce_animations());
    theme.set_flat(!ui.get_album_colors());
    theme.set_is_light(light);
    // Surfaces and text are translucent, so they sit well on the album-colored
    // backdrop instead of cutting flat grey shapes out of it.
    let base = if light { 0u8 } else { 255u8 };
    let ink = |alpha: u8| Color::from_argb_u8(alpha, base, base, base);
    if light {
        theme.set_window_bg(rgb(242, 242, 245));
        theme.set_panel(Color::from_argb_u8(170, 255, 255, 255));
        theme.set_card_bg(rgb(250, 250, 252));
        theme.set_elevated_bg(rgb(255, 255, 255));
    } else {
        theme.set_window_bg(rgb(12, 12, 14));
        theme.set_panel(ink(13));
        theme.set_card_bg(rgb(24, 24, 27));
        theme.set_elevated_bg(rgb(36, 36, 40));
    }
    theme.set_border(ink(if light { 26 } else { 20 }));
    theme.set_hover_bg(ink(if light { 12 } else { 16 }));
    theme.set_selected_bg(ink(if light { 20 } else { 26 }));
    theme.set_active_bg(ink(if light { 28 } else { 36 }));
    theme.set_pressed_bg(ink(if light { 36 } else { 46 }));
    theme.set_button_bg(ink(if light { 14 } else { 22 }));
    theme.set_disabled_bg(ink(8));
    theme.set_disabled_border(ink(20));
    theme.set_text(if light { rgb(18, 18, 20) } else { rgb(246, 246, 248) });
    theme.set_secondary_text(ink(if light { 190 } else { 200 }));
    // The faintest text still reads at about 4.5:1 on the plain background.
    theme.set_subtle_text(ink(if light { 160 } else { 158 }));
    theme.set_muted_text(ink(if light { 140 } else { 128 }));
    theme.set_track_bg(ink(if light { 40 } else { 56 }));
    theme.set_control_fg(if light { rgb(18, 18, 20) } else { rgb(246, 246, 248) });
    theme.set_focus_ring(ink(if light { 70 } else { 60 }));
    // Text on the album-tinted panel, and the tint itself, follow the theme.
    let (ink, faint) = if light { (rgb(0, 0, 0), 150) } else { (rgb(255, 255, 255), 170) };
    theme.set_on_ambient(Color::from_argb_u8(235, ink.red(), ink.green(), ink.blue()));
    theme.set_on_ambient_soft(Color::from_argb_u8(faint, ink.red(), ink.green(), ink.blue()));
    let ambient = (!ui.get_current_track_uri().is_empty())
        .then(|| ambient_color_from_image(&ui.get_album_art(), light))
        .flatten()
        .unwrap_or_else(|| theme.get_window_bg());
    theme.set_ambient(ambient);
    theme.set_accent(accent);
    theme.set_accent_soft(Color::from_argb_u8(40, accent.red(), accent.green(), accent.blue()));
    theme.set_accent_border(Color::from_argb_u8(110, accent.red(), accent.green(), accent.blue()));
}

/// The settings that each need work done again when they change.
#[derive(PartialEq)]
struct SettingsInEffect {
    /// How the sidebar's rows look.
    sidebar: bool,
    theme: (String, String, bool, bool),
    shortcuts: (bool, bool, [String; 5]),
}

impl SettingsInEffect {
    fn of(config: &AppConfig) -> Self {
        Self {
            sidebar: config.artwork_enabled,
            theme: (config.theme_mode.clone(), config.accent_color.clone(), config.reduce_animations, config.album_colors),
            shortcuts: (config.global_hotkeys, config.media_keys_enabled, hotkey_bindings(config)),
        }
    }
}

/// Whether Windows has apps in light mode. Read straight from the registry, which takes
/// microseconds, where starting reg.exe for it held up the window for tens of milliseconds.
fn system_prefers_light() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
        let name: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut value as *mut u32 as *mut _, &mut size)
        };
        status == 0 && value == 1
    }
    #[cfg(not(target_os = "windows"))]
    false
}

fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::from_rgb_u8(red, green, blue)
}

fn bind_callbacks(
    ui: &AppWindow,
    runtime: Arc<Runtime>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    media_tx: mpsc::UnboundedSender<MediaCommand>,
    hotkeys_guard: Arc<Mutex<Option<MediaKeyGuard>>>,
) {
    // The setup guide's links, its Copy button, and its check on the Client ID.
    ui.on_open_link(|url| {
        let _ = webbrowser::open(url.as_str());
    });
    ui.on_copy_text(|text| window_chrome::copy_text(text.as_str()));
    ui.on_cancel_sign_in(auth::cancel_sign_in);
    ui.on_client_id_problem(|text| client_id_problem(text.as_str()).into());

    let ui_weak = ui.as_weak();
    ui.on_save_settings({
        let state = Arc::clone(&state);
        let media_tx = media_tx.clone();
        let hotkeys_guard = Arc::clone(&hotkeys_guard);
        let runtime = Arc::clone(&runtime);
        let spotify_api = spotify_api.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let (audio_settings_changed, settings_before) = {
                    let guard = state.lock().unwrap();
                    let audio_settings_changed = guard.playback_engine.is_some()
                        && (guard.config.audio_quality != ui.get_audio_quality().as_str()
                            || guard.config.normalize_volume != ui.get_normalize_volume());
                    (audio_settings_changed, SettingsInEffect::of(&guard.config))
                };
                let (playlists, liked_count, artwork_enabled, selected_playlist_uri, settings_now) = {
                    let mut guard = state.lock().unwrap();
                    guard.config.client_id = ui.get_client_id().to_string();
                    guard.config.redirect_uri = ui.get_redirect_uri().to_string();
                    guard.config.artwork_enabled = ui.get_artwork_enabled();
                    guard.config.show_artist_column = ui.get_show_artist_column();
                    guard.config.show_time_column = ui.get_show_time_column();
                    guard.config.compact_sidebars = ui.get_compact_sidebars();
                    guard.config.theme_mode = ui.get_theme_mode().to_string();
                    guard.config.accent_color = ui.get_accent_color().to_string();
                    guard.config.audio_quality = ui.get_audio_quality().to_string();
                    if guard.config.output_device != ui.get_output_device().as_str() {
                        guard.config.output_device = ui.get_output_device().to_string();
                        audio_output::choose_device(&guard.config.output_device);
                    }
                    guard.config.normalize_volume = ui.get_normalize_volume();
                    guard.config.autoplay_similar = ui.get_autoplay_similar();
                    guard.config.crossfade_seconds = ui.get_crossfade_seconds().max(0) as u32;
                    set_crossfade(guard.config.crossfade_seconds);
                    guard.config.remember_volume = ui.get_remember_volume();
                    guard.config.resume_on_startup = ui.get_resume_on_startup();
                    guard.config.global_hotkeys = ui.get_global_hotkeys();
                    guard.config.media_keys_enabled = ui.get_media_keys_enabled();
                    guard.config.play_pause_hotkey = ui.get_play_pause_hotkey().to_string();
                    guard.config.next_hotkey = ui.get_next_hotkey().to_string();
                    guard.config.previous_hotkey = ui.get_previous_hotkey().to_string();
                    guard.config.shuffle_hotkey = ui.get_shuffle_hotkey().to_string();
                    guard.config.repeat_hotkey = ui.get_repeat_hotkey().to_string();
                    guard.config.minimize_to_tray = ui.get_minimize_to_tray();
                    guard.config.close_to_tray = ui.get_close_to_tray();
                    guard.config.start_minimized = ui.get_start_minimized();
                    guard.config.tray_at_sign_in = ui.get_tray_at_sign_in();
                    guard.config.low_resource_mode = ui.get_low_resource_mode();
                    guard.config.reduce_animations = ui.get_reduce_animations();
                    guard.config.disable_background_effects = ui.get_disable_background_effects();
                    guard.config.album_colors = ui.get_album_colors();
                    guard.config.log_level = ui.get_log_level().to_string();
                    guard.config.developer_mode = ui.get_developer_mode();
                    guard.config.check_for_updates = ui.get_check_for_updates();
                    UPDATE_AUTO_CHECK.store(guard.config.check_for_updates, Ordering::Relaxed);
                    config::save_config_background(&guard.config);
                    let settings_now = SettingsInEffect::of(&guard.config);
                    // The sidebar's rows are made again only when how they look changed.
                    let playlists = if settings_now.sidebar != settings_before.sidebar {
                        guard.config.library_snapshot.playlists.clone()
                    } else {
                        Vec::new()
                    };
                    (
                        playlists,
                        liked_count(&guard.config.library_snapshot),
                        guard.config.artwork_enabled,
                        guard.selected_playlist_uri.clone(),
                        settings_now,
                    )
                };
                if audio_settings_changed {
                    // Clicking through the options should cause one restart, so
                    // only the last change after a short pause goes ahead.
                    static LATEST_CHANGE: AtomicU64 = AtomicU64::new(0);
                    let change = LATEST_CHANGE.fetch_add(1, Ordering::SeqCst) + 1;
                    let ui_weak = ui_weak.clone();
                    let state = Arc::clone(&state);
                    let spotify_api = spotify_api.clone();
                    runtime.spawn(async move {
                        tokio::time::sleep(Duration::from_millis(1_200)).await;
                        if LATEST_CHANGE.load(Ordering::SeqCst) != change {
                            return;
                        }
                        let result = restart_local_player(ui_weak.clone(), state, spotify_api).await;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                match result {
                                    Ok(()) => show_toast(&ui, "Audio settings applied", false),
                                    Err(error) => append_log(&ui, &format!("Applying audio settings failed: {error}")),
                                }
                            }
                        });
                    });
                }
                // Each toggle saves everything, so only what it changed is done again: making the
                // sidebar's rows, the theme and the shortcuts each take a moment.
                if settings_now.sidebar != settings_before.sidebar {
                    apply_playlist_rows(
                        &ui,
                        &playlists,
                        liked_count,
                        artwork_enabled,
                        Some(selected_playlist_uri.as_str()),
                    );
                }
                if settings_now.theme != settings_before.theme {
                    apply_theme_to_ui(&ui);
                }
                if settings_now.shortcuts != settings_before.shortcuts {
                    let mut hotkeys = hotkeys_guard.lock().unwrap();
                    // Release the old registrations before registering again,
                    // otherwise the same keys report "already registered".
                    *hotkeys = None;
                    if media_controls::set_enabled(ui.get_media_keys_enabled()) {
                        now_playing_changed(&ui, &state);
                    }
                    if ui.get_global_hotkeys() {
                        let bindings = hotkey_bindings(&state.lock().unwrap().config);
                        match MediaKeyGuard::install(media_tx.clone(), &bindings) {
                            Ok(guard) => {
                                for warning in &guard.warnings {
                                    append_log(&ui, warning);
                                }
                                ui.set_hotkey_warning(guard.warnings.join("\n").into());
                                *hotkeys = Some(guard);
                            }
                            Err(error) => {
                                let message = format!("Shortcuts unavailable: {error}");
                                append_log(&ui, &message);
                                ui.set_hotkey_warning(message.into());
                            }
                        }
                    } else {
                        ui.set_hotkey_warning("".into());
                    }
                }
                if ui.get_close_to_tray() || ui.get_minimize_to_tray() {
                    tray::ensure_started();
                }
                append_log(&ui, "Saved settings.");
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_measure_cover_cache(move || show_cover_cache_size(ui_weak.clone(), false));
    let ui_weak = ui.as_weak();
    ui.on_clear_cover_cache(move || {
        if let Some(ui) = ui_weak.upgrade() {
            // Pressed once is enough, while the clearing goes on.
            ui.set_cover_cache_clearable(false);
        }
        show_cover_cache_size(ui_weak.clone(), true);
    });

    let ui_weak = ui.as_weak();
    ui.on_refresh_output_devices(move || {
        let ui_weak = ui_weak.clone();
        let _ = std::thread::Builder::new().name("spotlight-audio-devices".to_string()).spawn(move || {
            let names = audio_output::device_names();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let chosen = ui.get_output_device();
                    ui.set_output_device_missing(!chosen.is_empty() && audio_output::find_device(&chosen, &names).is_none());
                    let rows: Vec<SharedString> = names.iter().map(SharedString::from).collect();
                    ui.set_output_devices(ModelRc::new(VecModel::from(rows)));
                }
            });
        });
    });

    let ui_weak = ui.as_weak();
    ui.on_cycle_hotkey(move |action| {
        if let Some(ui) = ui_weak.upgrade() {
            match action.as_str() {
                "play-pause" => ui.set_play_pause_hotkey(cycle_binding(&ui.get_play_pause_hotkey(), "Ctrl+Alt+P", "Ctrl+Shift+P").into()),
                "next" => ui.set_next_hotkey(cycle_binding(&ui.get_next_hotkey(), "Ctrl+Alt+N", "Ctrl+Shift+N").into()),
                "previous" => ui.set_previous_hotkey(cycle_binding(&ui.get_previous_hotkey(), "Ctrl+Alt+B", "Ctrl+Shift+B").into()),
                "shuffle" => ui.set_shuffle_hotkey(cycle_binding(&ui.get_shuffle_hotkey(), "Ctrl+Alt+S", "Ctrl+Shift+S").into()),
                "repeat" => ui.set_repeat_hotkey(cycle_binding(&ui.get_repeat_hotkey(), "Ctrl+Alt+R", "Ctrl+Shift+R").into()),
                _ => {}
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_record_shortcut({
        let hotkeys_guard = Arc::clone(&hotkeys_guard);
        move |action| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let was_listening = !ui.get_recording_shortcut().is_empty();
            if action.is_empty() {
                if was_listening {
                    stop_listening_for_shortcut(&ui, &hotkeys_guard, true);
                }
                return;
            }
            ui.set_recording_shortcut(action);
            ui.set_shortcut_keys("".into());
            ui.set_shortcut_refused(false);
            if !was_listening {
                window_chrome::listen_for_shortcut(true);
                if let Some(guard) = hotkeys_guard.lock().unwrap().as_ref() {
                    guard.pause_custom();
                }
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_shortcut_key({
        let hotkeys_guard = Arc::clone(&hotkeys_guard);
        move |key, ctrl, alt, shift, win| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let action = ui.get_recording_shortcut();
            if action.is_empty() {
                return;
            }
            match media_keys::record_key(key as u16, ctrl, alt, shift, win) {
                media_keys::Recorded::Holding(keys) => {
                    // Key 0 is a modifier being let go. A refusal stays up through that,
                    // so it can be read, until the next key goes down.
                    if key == 0 && ui.get_shortcut_refused() {
                        return;
                    }
                    ui.set_shortcut_keys(keys.into());
                    ui.set_shortcut_refused(false);
                }
                media_keys::Recorded::Refused(why) => {
                    ui.set_shortcut_keys(why.into());
                    ui.set_shortcut_refused(true);
                }
                media_keys::Recorded::Cancelled => stop_listening_for_shortcut(&ui, &hotkeys_guard, true),
                media_keys::Recorded::Done(shortcut) => {
                    let shortcuts = shortcut_settings(&ui);
                    let taken_by = shortcuts
                        .iter()
                        .find(|(other, _, current)| *other != action.as_str() && media_keys::same_shortcut(current, &shortcut));
                    if let Some((_, label, _)) = taken_by {
                        ui.set_shortcut_keys(format!("Used by {label}").into());
                        ui.set_shortcut_refused(true);
                        return;
                    }
                    let unchanged = shortcuts
                        .iter()
                        .any(|(own, _, current)| *own == action.as_str() && current.as_str() == shortcut);
                    if unchanged {
                        stop_listening_for_shortcut(&ui, &hotkeys_guard, true);
                        return;
                    }
                    let shortcut = SharedString::from(shortcut);
                    match action.as_str() {
                        "play-pause" => ui.set_play_pause_hotkey(shortcut),
                        "next" => ui.set_next_hotkey(shortcut),
                        "previous" => ui.set_previous_hotkey(shortcut),
                        "shuffle" => ui.set_shuffle_hotkey(shortcut),
                        "repeat" => ui.set_repeat_hotkey(shortcut),
                        _ => {}
                    }
                    // Saving registers every shortcut again, the new one included.
                    stop_listening_for_shortcut(&ui, &hotkeys_guard, false);
                    ui.invoke_save_settings();
                }
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_sign_out({
        let state = Arc::clone(&state);
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                {
                    let mut guard = state.lock().unwrap();
                    guard.session = guard.session.wrapping_add(1);
                    guard.track_view_generation = guard.track_view_generation.wrapping_add(1);
                    guard.token = None;
                    guard.local_playback_token = None;
                    guard.config.token = None;
                    guard.config.local_playback_token = None;
                    // Its Recently played stays on this PC, for when it signs in again. A song
                    // still waiting to count doesn't count for whoever comes next.
                    guard.config.account_id = None;
                    guard.account_asked_at = None;
                    guard.play_watch.stopped();
                    if let Some(engine) = guard.playback_engine.as_ref() {
                        engine.shutdown();
                    }
                    // The next account mustn't see this one's playlists, even for a moment,
                    // and its library loads straight away rather than after a wait.
                    guard.config.library_snapshot = Default::default();
                    guard.config.startup_snapshot = Default::default();
                    guard.next_allowed_library_refresh_at_epoch = 0;
                    guard.playlists_revision = guard.playlists_revision.wrapping_add(1);
                    guard.playlist_uris.clear();
                    guard.playlist_track_hrefs.clear();
                    guard.playlist_track_counts.clear();
                    guard.track_uris.clear();
                    guard.playlist_track_cache = HashMap::from([(LIKED_SONGS_PLAYLIST_URI.to_string(), Vec::new())]);
                    guard.playlist_cache_versions.clear();
                    guard.loaded.clear();
                    guard.selected_playlist_uri = LIKED_SONGS_PLAYLIST_URI.to_string();
                    // Nor its Next up.
                    guard.queue_override_tracks.clear();
                    guard.queue_override_active = false;
                    guard.spotify_queue_tracks = None;
                    guard.spotify_queue_current_uri = None;
                    guard.last_queue_display.clear();
                    guard.last_queue_summary.clear();
                    guard.queue_painted_uris.clear();
                    config::save_config_background(&guard.config);
                }
                apply_playlist_rows(&ui, &[], 0, false, None);
                ui.set_more_playlists(false);
                show_tracks(&ui, Vec::new());
                clear_queue_rows(&ui);
                clear_local_playback_engine(&state);
                {
                    let mut guard = state.lock().unwrap();
                    guard.active_device_id = None;
                    guard.device_id = None;
                    guard.preferred_device_id = None;
                }
                forget_home(&ui);
                clear_now_playing(&ui, &state);
                left_off::forget();
                ui.set_connection_status("Signed out".into());
                ui.set_player_status("Offline".into());
                ui.set_sync_trouble("".into());
                ui.set_signed_in(false);
                ui.set_connect_stage("".into());
                ui.set_player_ready(false);
                ui.set_show_settings(false);
                ui.set_onboarding_step(if ui.get_client_id().trim().is_empty() { 1 } else { 3 });
                append_log(&ui, "Signed out of Spotify.");
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_reset_app_data({
        let state = Arc::clone(&state);
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            // Signing out stops the player and lets go of everything the account had here.
            ui.invoke_sign_out();
            {
                let mut guard = state.lock().unwrap();
                guard.config = config::new_config();
                config::save_config_background(&guard.config);
            }
            ALBUM_ART_IMAGES.with(|cached| cached.borrow_mut().clear());
            PLAYLIST_COVERS.with(|cached| cached.borrow_mut().clear());
            if let Ok(dir) = config::config_dir() {
                let _ = std::fs::remove_dir_all(dir.join("album-art"));
                // Recently played, for every account that signed in here.
                for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("history") && name.ends_with(".json") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            // Everything else, from the theme to the window and the shortcuts, starts again from
            // the defaults too, as on a first run. Opening again is the sure way to get that.
            if let Ok(exe) = std::env::current_exe() {
                let reopened = std::process::Command::new(exe)
                    .arg(updates::AFTER_UPDATE_ARG)
                    .arg(std::process::id().to_string())
                    .spawn();
                if reopened.is_ok() {
                    config::exit_after_saving();
                }
            }
            ui.set_status_log("Ready.".into());
            append_log(&ui, "Reset app data. Open Spotlight again to start afresh.");
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_connect_spotify({
        let state = Arc::clone(&state);
        let runtime = Arc::clone(&runtime);
        let spotify_api = spotify_api.clone();
        move || {
            // Text boxes save when they lose focus, and clicking a button doesn't take it.
            if let Some(ui) = ui_weak.upgrade() {
                // Pasted IDs often come with a space or a line break at the end.
                let client_id = ui.get_client_id().trim().to_string();
                ui.set_client_id(client_id.clone().into());
                ui.set_connect_stage("library".into());
                ui.set_connect_error("".into());
                let mut guard = state.lock().unwrap();
                guard.config.client_id = client_id;
                guard.config.redirect_uri = ui.get_redirect_uri().to_string();
            }
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let config_snapshot = { state.lock().unwrap().config.clone() };
                if config_snapshot.client_id.is_empty() {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_connection_status("Client ID needed".into());
                            ui.set_connect_stage("failed".into());
                            ui.set_connect_error("Paste your Spotify app's Client ID first.".into());
                            append_log(&ui, "Add your Spotify app Client ID in Settings → Account, then Connect.");
                        }
                    });
                    return;
                }
                match auth::login(&config_snapshot).await {
                    Ok(token) => {
                        // Signed in to another account this time: the one before signs out
                        // first, so its player, library and Next up don't carry over to this one.
                        // Only an account known to be the same one keeps them. If either can't
                        // be told, it signs out too: signing the player in again is better than
                        // playing on someone else's.
                        let now_signed_in = spotify_api.account_id(&token).await.ok();
                        let (signed_in_before, other_account) = {
                            let guard = state.lock().unwrap();
                            let signed_in_before = guard.token.is_some() || guard.local_playback_token.is_some();
                            let same_account = matches!(
                                (&guard.config.account_id, &now_signed_in),
                                (Some(before), Some(now)) if before == now
                            );
                            (signed_in_before, signed_in_before && !same_account)
                        };
                        // Spotify asking to wait is no sign of another account, and signing out
                        // over it would lose the player's sign-in and Next up. Nothing changes.
                        if signed_in_before && now_signed_in.is_none() && spotify::rate_limit_wait().is_some() {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    ui.set_connect_stage("done".into());
                                    append_log(&ui, "Reconnect failed: Spotify asked to wait. Nothing changed; try again in a few minutes.");
                                }
                            });
                            return;
                        }
                        if other_account {
                            let (signed_out_tx, signed_out) = tokio::sync::oneshot::channel();
                            let ui_weak = ui_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    ui.invoke_sign_out();
                                }
                                let _ = signed_out_tx.send(());
                            });
                            let _ = signed_out.await;
                        }
                        let player_signed_in = {
                            let mut guard = state.lock().unwrap();
                            // It may be another account than before, so what's still being asked
                            // for the one before is out of date, and a song waiting to count
                            // doesn't count for this one.
                            guard.session = guard.session.wrapping_add(1);
                            guard.play_watch.stopped();
                            guard.token = Some(token.clone());
                            guard.config.token = Some(token.clone());
                            // Known now, so a Reconnect straight after can still tell.
                            guard.config.account_id = now_signed_in.clone();
                            guard.account_asked_at = None;
                            config::save_config_background(&guard.config);
                            guard.local_playback_token.is_some()
                        };
                        let ui_weak_signed = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_signed.upgrade() {
                                ui.set_signed_in(true);
                                ui.invoke_open_home();
                                ui.set_connection_status("Connected".into());
                                // Playing here needs a sign-in of its own, unless it has one.
                                if !player_signed_in {
                                    ui.set_connect_stage("player".into());
                                }
                            }
                        });

                        let state_clone = Arc::clone(&state);
                        let spotify_clone = spotify_api.clone();
                        let player = ensure_local_playback(Arc::clone(&state), ui_weak.clone()).await;
                        let player_ready = state.lock().unwrap().local_playback_token.is_some();
                        let ui_weak_ready = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_ready.upgrade() {
                                ui.set_player_ready(player_ready);
                                ui.set_connect_stage("done".into());
                                if let Err(error) = player {
                                    append_log(&ui, &format!("Playing on this PC isn't set up: {error}"));
                                }
                            }
                        });

                        let _ = refresh_everything(ui_weak, state_clone, spotify_clone, true, true, false).await;
                    }
                    Err(error) => {
                        let cancelled = error.to_string() == auth::CANCELLED;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_connection_status("Connect failed".into());
                                ui.set_player_status("Offline".into());
                                if cancelled {
                                    ui.set_connect_stage("".into());
                                } else {
                                    ui.set_connect_stage("failed".into());
                                    ui.set_connect_error(as_sentence(&error.to_string()).into());
                                }
                                // The setup guide shows what went wrong itself, and stopping on
                                // purpose isn't a failure worth a toast.
                                if cancelled || ui.get_onboarding_step() > 0 {
                                    write_log(&ui, &format!("Connect failed: {error}"));
                                } else {
                                    append_log(&ui, &format!("Connect failed: {error}"));
                                }
                            }
                        });
                    }
                }
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_refresh_library({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let _ = refresh_everything(ui_weak, state, spotify_api, true, true, false).await;
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_load_more_tracks({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_track_view_more_busy() {
                return;
            }
            let (list_uri, loaded, session) = {
                let guard = state.lock().unwrap();
                let list_uri = guard.selected_playlist_uri.clone();
                let Some(loaded) = guard.loaded.get(&list_uri).copied().filter(|loaded| loaded.more_from.is_some()) else {
                    return;
                };
                (list_uri, loaded, guard.session)
            };
            ui.set_track_view_more_busy(true);
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let result = load_more_songs(&state, &spotify_api, session, &list_uri, loaded).await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    let (same_session, same_list) = {
                        let guard = state.lock().unwrap();
                        (guard.session == session, guard.selected_playlist_uri == list_uri)
                    };
                    // Another list is shown now, which let go of the button. This one keeps what
                    // came, for when it's back.
                    if !same_list {
                        return;
                    }
                    ui.set_track_view_more_busy(false);
                    // Signed out or in again meanwhile: what came was for the session before.
                    if !same_session {
                        show_more_to_load(&ui, &state);
                        return;
                    }
                    match result {
                        Ok(Some(tracks)) => {
                            refresh_tracks_in_place(&ui, tracks);
                            show_more_to_load(&ui, &state);
                        }
                        Ok(None) => show_more_to_load(&ui, &state),
                        Err(error) => {
                            append_log(&ui, &format!("Loading more songs failed: {error}"));
                            show_toast(&ui, "Couldn't load more songs", true);
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_load_more_playlists({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_more_playlists_busy() {
                return;
            }
            let (from, session, revision) = {
                let guard = state.lock().unwrap();
                (guard.config.library_snapshot.playlists.len(), guard.session, guard.playlists_revision)
            };
            ui.set_more_playlists_busy(true);
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let fetched = match current_token(&state).await {
                    Ok(token) => spotify_api.playlists(&token, from, spotify::PLAYLISTS_PER_PART).await,
                    Err(error) => Err(error),
                };
                let failed = match fetched {
                    Ok(part) => {
                        let mut guard = state.lock().unwrap();
                        // Signed out, or the library changed meanwhile: it asks again if need be.
                        if ensure_same_session(&guard, session).is_ok() && guard.playlists_revision == revision {
                            guard.playlists_revision = revision.wrapping_add(1);
                            for item in &part.items {
                                guard.playlist_uris.push(item.uri.clone());
                                guard.playlist_track_hrefs.push(item.tracks.href.clone());
                                guard.playlist_track_counts.push(item.tracks.total);
                                guard.config.library_snapshot.playlists.push(cached_playlist(item));
                            }
                            guard.config.library_snapshot.playlists_total = part.total;
                            guard.config.library_snapshot.playlists_wanted = from + spotify::PLAYLISTS_PER_PART;
                            config::save_config_background(&guard.config);
                        }
                        None
                    }
                    Err(error) => Some(error.to_string()),
                };
                let _ = slint::invoke_from_event_loop({
                    let ui_weak = ui_weak.clone();
                    let state = Arc::clone(&state);
                    move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        ui.set_more_playlists_busy(false);
                        if let Some(error) = failed {
                            append_log(&ui, &format!("Loading more playlists failed: {error}"));
                            show_toast(&ui, "Couldn't load more playlists", true);
                            return;
                        }
                        let guard = state.lock().unwrap();
                        let snapshot = &guard.config.library_snapshot;
                        apply_playlist_rows(&ui, &snapshot.playlists, liked_count(snapshot), guard.config.artwork_enabled, Some(guard.selected_playlist_uri.as_str()));
                        ui.set_more_playlists(more_playlists(snapshot));
                    }
                });
                warm_playlist_art_and_repaint(ui_weak, state).await;
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_select_playlist({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |index| {
            if index < 0 {
                return;
            }
            let generation = {
                let mut guard = state.lock().unwrap();
                guard.track_view_generation = guard.track_view_generation.wrapping_add(1);
                // Nothing may act on the previous list while this one loads.
                guard.selected_playlist_uri.clear();
                guard.track_uris.clear();
                guard.track_view_generation
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_track_view_loading(true);
                show_tracks(&ui, Vec::new());
            }
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                if let Err(error) = populate_tracks_for_playlist(index as usize, generation, ui_weak.clone(), state, spotify_api).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_track_view_loading(false);
                            append_log(&ui, &format!("Playlist tracks failed to load: {error}"));
                        }
                    });
                }
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_filter_tracks(move |text| {
        if let Some(ui) = ui_weak.upgrade() {
            set_track_filter(&ui, text.as_str());
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_search_changed({
        let state = Arc::clone(&state);
        move |text| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let needle = text.trim().to_lowercase();
            let mut suggestions = Vec::new();
            if !needle.is_empty() {
                let images = ui.get_playlist_images();
                let guard = state.lock().unwrap();
                let names = std::iter::once(("Liked Songs".to_string(), format!("{} songs", liked_count(&guard.config.library_snapshot))))
                    .chain(guard.config.library_snapshot.playlists.iter().map(|playlist| {
                        (
                            playlist.name.clone(),
                            playlist.track_count.map(|count| format!("{count} songs")).unwrap_or_else(|| "Playlist".to_string()),
                        )
                    }));
                for (index, (name, subtitle)) in names.enumerate() {
                    if name.to_lowercase().contains(&needle) {
                        suggestions.push(SearchSuggestion {
                            title: name.into(),
                            subtitle: subtitle.into(),
                            index: index as i32,
                            art: images.row_data(index).unwrap_or_default(),
                        });
                        if suggestions.len() >= 6 {
                            break;
                        }
                    }
                }
            }
            ui.set_search_suggestions(ModelRc::from(Rc::new(VecModel::from(suggestions))));
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_refresh_devices({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let _ = refresh_everything(ui_weak, state, spotify_api, false, true, false).await;
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_search_tracks({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |query| {
            let query = query.trim().to_string();
            if query.len() < 2 {
                if let Some(ui) = ui_weak.upgrade() {
                    append_log(&ui, "Search needs at least 2 characters.");
                }
                return;
            }

            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            let generation = {
                let mut guard = state.lock().unwrap();
                guard.track_view_generation = guard.track_view_generation.wrapping_add(1);
                guard.track_view_generation
            };
            if let Some(ui) = ui_weak.upgrade() { ui.set_track_view_loading(true); }
            runtime.spawn(async move {
                let token = match current_token(&state).await {
                    Ok(token) => token,
                    Err(error) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_track_view_loading(false);
                                append_log(&ui, &format!("Search failed: {error}"));
                            }
                        });
                        return;
                    }
                };

                let result = spotify_api.search_tracks(&token, &query, 10).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        if state.lock().unwrap().track_view_generation != generation {
                            return;
                        }
                        ui.set_track_view_loading(false);
                        match result {
                            Ok(tracks) => {
                                {
                                    let mut guard = state.lock().unwrap();
                                    guard.selected_playlist_uri = SEARCH_RESULTS_URI.to_string();
                                    guard.track_uris = tracks.iter().map(|track| track.uri.clone()).collect();
                                    guard
                                        .playlist_track_cache
                                        .insert(SEARCH_RESULTS_URI.to_string(), tracks.clone());
                                }
                                let result_count = tracks.len();
                                show_tracks(&ui, tracks);
                                ui.set_track_view_title(query.clone().into());
                                ui.set_track_view_kicker("Search results".into());
                                ui.set_selected_playlist_index(-1);
                                if result_count == 0 {
                                    append_log(&ui, &format!("No results for \"{query}\"."));
                                }
                                ui.set_selected_liked_song_index(-1);
                                append_log(&ui, &format!("Search loaded results for \"{query}\"."));
                            }
                            Err(error) => append_log(&ui, &format!("Search failed: {error}")),
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_activate_device({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |index| {
            if index < 0 {
                return;
            }
            let (token, device_id, device_name, is_local_device, generation, previous_device_id) = {
                let mut guard = state.lock().unwrap();
                let device_id = guard.available_device_ids.get(index as usize).cloned();
                let device_name = guard
                    .available_device_names
                    .get(index as usize)
                    .cloned()
                    .unwrap_or_else(|| "selected device".to_string());
                let is_local_device = device_name == guard.config.device_name;
                guard.device_switch_generation = guard.device_switch_generation.wrapping_add(1);
                (
                    guard.token.clone(),
                    device_id,
                    device_name,
                    is_local_device,
                    guard.device_switch_generation,
                    guard.device_id.clone(),
                )
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_device_switching(true);
                // The row turns a ring while this happens.
            }
            if token.is_none() || device_id.as_deref().is_none_or(str::is_empty) {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_device_switching(false);
                    show_toast(&ui, "Device unavailable — refresh and try again", true);
                }
                return;
            }
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let token = token.expect("validated device switch token");
                let device_id = device_id.expect("validated device id");
                let result = switch_playback_device(
                    ui_weak.clone(),
                    Arc::clone(&state),
                    spotify_api.clone(),
                    token,
                    device_id,
                    device_name.clone(),
                    is_local_device,
                )
                .await;
                let is_current = {
                    let mut guard = state.lock().unwrap();
                    if guard.device_switch_generation != generation {
                        false
                    } else {
                        match &result {
                            Ok(actual_device_id) => {
                                guard.preferred_device_id = Some(actual_device_id.clone());
                                guard.device_id = Some(actual_device_id.clone());
                            }
                            Err(_) => guard.device_id = previous_device_id,
                        }
                        true
                    }
                };
                if !is_current {
                    return;
                }
                let _ = refresh_everything(ui_weak.clone(), Arc::clone(&state), spotify_api.clone(), false, false, false).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_device_switching(false);
                        match result {
                            Ok(_) => {
                                append_log(&ui, &format!("Transferred playback to selected device {}.", device_name));
                            }
                            Err(error) => {
                                append_log(&ui, &format!("Device switch failed: {error}"));
                            }
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_toggle_mute({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let (target_volume, use_local_volume) = {
                let mut guard = state.lock().unwrap();
                guard.last_volume_change = Some(Instant::now());
                let target = if ui.get_volume_percent() > 0 {
                    guard.previous_volume_percent = ui.get_volume_percent().clamp(1, 100);
                    guard.muted = true;
                    0
                } else {
                    let restored = guard.previous_volume_percent.clamp(1, 100);
                    guard.muted = false;
                    restored
                };
                let local = guard.local_volume_is_target();
                if local {
                    guard.local_volume_percent = target;
                }
                (target, local)
            };
            if use_local_volume {
                playback::set_volume_now(target_volume as u16);
            }
            ui.set_volume_percent(target_volume);
            ui.set_muted(target_volume == 0);
            ui.set_previous_volume_percent({
                let guard = state.lock().unwrap();
                guard.previous_volume_percent
            });

            if use_local_volume {
                if let Err(error) = run_local_player_command(Arc::clone(&state), |engine| {
                    engine.set_volume(target_volume as u16)
                }) {
                    append_log(&ui, &format!("Volume change failed: {error}"));
                }
                return;
            }

            let ui_weak_inner = ui_weak.clone();
            let state_inner = Arc::clone(&state);
            let spotify_api_inner = spotify_api.clone();
            runtime.spawn(async move {
                let result = apply_volume_change(
                    Arc::clone(&state_inner),
                    spotify_api_inner,
                    target_volume,
                )
                .await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        if let Err(error) = result {
                            append_log(&ui, &format!("Volume change failed: {error}"));
                        }
                    }
                });
            });
        }
    });

    ui.on_toggle_fullscreen(|| {
        toggle_spotlight_window_maximized();
    });

    ui.on_toggle_mini_player({
        let state = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_mini_mode() {
                let left_at = window_chrome::leave_mini();
                ui.set_mini_mode(false);
                if let Some(position) = left_at {
                    let mut guard = state.lock().unwrap();
                    guard.config.mini_position = Some(position);
                    config::save_config_background(&guard.config);
                }
            } else {
                let (on_top, position) = {
                    let guard = state.lock().unwrap();
                    (guard.config.mini_on_top, guard.config.mini_position)
                };
                // The stage's cover is drawn over everything else, so it can't stay out. Lyrics
                // it covered are there again on the way back.
                ui.invoke_close_stage();
                if window_chrome::enter_mini(on_top, position) {
                    ui.set_mini_on_top(on_top);
                    ui.set_mini_mode(true);
                }
            }
        }
    });
    ui.on_mini_on_top_changed({
        let state = Arc::clone(&state);
        move |on_top| {
            window_chrome::set_mini_on_top(on_top);
            let mut guard = state.lock().unwrap();
            guard.config.mini_on_top = on_top;
            config::save_config_background(&guard.config);
        }
    });
    ui.on_mini_moved({
        let state = Arc::clone(&state);
        move |x, y| {
            let mut guard = state.lock().unwrap();
            if guard.config.mini_position != Some((x, y)) {
                guard.config.mini_position = Some((x, y));
                config::save_config_background(&guard.config);
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_move_queue_item({
        let state = Arc::clone(&state);
        move |from, to| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            if from >= 0 && to >= 0 && from != to && ui.get_queue_reorder_enabled() {
                edit_queue(&ui, &state, |tracks, painted| {
                    move_dragged_track(tracks, painted, from as usize, to as usize);
                });
            } else {
                // Rows wait for a drag to end before changing, whether or not it moved anything.
                repaint_queue(&ui, &state);
            }
            window_chrome::sync_hover_to_pointer(&ui);
        }
    });

    // Artist and album links and Home's cards open as a list of songs, in the search view's place.
    let open_detail = {
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        let ui_weak = ui.as_weak();
        move |source: DetailSource| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let uri = ui.get_current_track_uri().to_string();

            let generation = {
                let mut guard = state.lock().unwrap();
                guard.track_view_generation = guard.track_view_generation.wrapping_add(1);
                guard.track_view_generation
            };
            ui.set_track_view_loading(true);
            ui.set_track_view_has_art(false);
            // On screen at once, while the songs load.
            let (title, kicker): (SharedString, &str) = match &source {
                DetailSource::PlayingAlbum => (ui.get_track_album(), "Album"),
                DetailSource::PlayingArtist(name) => (name.as_str().into(), "Artist"),
                DetailSource::Card { card, .. } => (card.title.as_str().into(), card.kicker()),
                DetailSource::Artist(artist) => (artist.name.as_str().into(), "Artist"),
                DetailSource::Album(album) => (album.name.as_str().into(), "Album"),
                DetailSource::Radio(song) => (format!("{} Radio", song.name).into(), "Radio"),
                DetailSource::History => (HISTORY_TITLE.into(), HISTORY_KICKER),
            };
            ui.set_track_view_title(title);
            ui.set_track_view_kicker(kicker.into());
            ui.set_track_view_meta("".into());
            ui.set_selected_playlist_index(-1);
            show_tracks(&ui, Vec::new());
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let result = load_detail_page(source, &uri, &state, &spotify_api).await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    if state.lock().unwrap().track_view_generation != generation {
                        return;
                    }
                    ui.set_track_view_loading(false);
                    match result {
                        Ok((title, kicker, tracks, photo)) => {
                            if let Some(photo) = photo {
                                ui.set_track_view_art(load_full_size_art(&photo));
                                ui.set_track_view_has_art(true);
                            }
                            {
                                let mut guard = state.lock().unwrap();
                                guard.selected_playlist_uri = SEARCH_RESULTS_URI.to_string();
                                guard.track_uris = tracks.iter().map(|track| track.uri.clone()).collect();
                                guard.playlist_track_cache.insert(SEARCH_RESULTS_URI.to_string(), tracks.clone());
                            }
                            show_tracks(&ui, tracks);
                            ui.set_track_view_title(title.into());
                            ui.set_track_view_kicker(kicker.into());
                            ui.set_selected_playlist_index(-1);
                            ui.set_selected_liked_song_index(-1);
                        }
                        Err(error) => {
                            append_log(&ui, &format!("Couldn't open that page: {error}"));
                            show_toast(&ui, "Couldn't open that page", true);
                        }
                    }
                });
            });
        }
    };
    ui.on_open_album({
        let open_detail = open_detail.clone();
        move || open_detail(DetailSource::PlayingAlbum)
    });
    ui.on_open_artist({
        let open_detail = open_detail.clone();
        move |name| open_detail(DetailSource::PlayingArtist(name.to_string()))
    });
    ui.on_open_history({
        let open_detail = open_detail.clone();
        move || open_detail(DetailSource::History)
    });
    ui.on_now_playing_changed({
        let state = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                now_playing_changed(&ui, &state);
            }
        }
    });
    ui.on_lyrics_wanted({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                show_lyrics(&ui, &runtime, &state);
            }
        }
    });

    // A song's menu, before it shows: whether the song is liked, from the saved library.
    ui.on_track_menu_opened({
        let state = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let guard = state.lock().unwrap();
            let song = guard.playlist_track_cache.get(&guard.selected_playlist_uri).and_then(|songs| songs.get(index.max(0) as usize));
            let liked = song.is_some_and(|song| {
                guard.selected_playlist_uri == LIKED_SONGS_PLAYLIST_URI
                    || guard.config.library_snapshot.liked_songs.iter().any(|liked| liked.uri == song.uri)
            });
            let has_album = song.is_some_and(|song| song.album.id.is_some());
            ui.set_track_context_liked(liked);
            ui.set_track_context_has_album(has_album);
        }
    });
    ui.on_track_menu_action({
        let open_detail = open_detail.clone();
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        let ui_weak = ui.as_weak();
        move |action, index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let song = {
                let guard = state.lock().unwrap();
                guard.playlist_track_cache.get(&guard.selected_playlist_uri).and_then(|songs| songs.get(index.max(0) as usize)).cloned()
            };
            let Some(song) = song else { return };
            match action.as_str() {
                "artist" => {
                    if let Some(artist) = song.artists.first() {
                        open_detail(DetailSource::Artist(artist.clone()));
                    }
                }
                "album" => open_detail(DetailSource::Album(song.album.clone())),
                "radio" => open_detail(DetailSource::Radio(song)),
                "copy-link" => {
                    let link = track_id_from_uri(&song.uri).map(|id| format!("https://open.spotify.com/track/{id}"));
                    match link.map(|link| window_chrome::copy_text(&link)) {
                        Some(true) => show_toast(&ui, "Link copied", false),
                        _ => show_toast(&ui, "Couldn't copy the link", true),
                    }
                }
                "like" | "unlike" => {
                    let liked = action == "like";
                    let ui_weak = ui_weak.clone();
                    let state = Arc::clone(&state);
                    let spotify_api = spotify_api.clone();
                    let session = state.lock().unwrap().session;
                    runtime.spawn(async move {
                        let result = async {
                            let token = current_token(&state).await?;
                            spotify_api.set_in_library(&token, &song.uri, liked).await
                        }
                        .await;
                        {
                            // As with the heart: only for the account that asked.
                            let mut guard = state.lock().unwrap();
                            if ensure_same_session(&guard, session).is_err() {
                                return;
                            }
                            if result.is_ok() {
                                remember_liked(&mut guard, &song.uri, liked.then_some(&song));
                            }
                        }
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = ui_weak.upgrade() else { return };
                            if state.lock().unwrap().session != session {
                                return;
                            }
                            match result {
                                Ok(()) => {
                                    if ui.get_current_track_uri() == song.uri.as_str() {
                                        ui.set_track_liked(liked);
                                    }
                                    show_toast(&ui, if liked { "Added to Liked Songs" } else { "Removed from Liked Songs" }, false);
                                    let liked_songs = {
                                        let guard = state.lock().unwrap();
                                        (guard.selected_playlist_uri == LIKED_SONGS_PLAYLIST_URI)
                                            .then(|| guard.playlist_track_cache.get(LIKED_SONGS_PLAYLIST_URI).cloned())
                                            .flatten()
                                    };
                                    if let Some(songs) = liked_songs {
                                        refresh_tracks_in_place(&ui, songs);
                                    }
                                }
                                Err(error) => {
                                    append_log(&ui, &format!("Liked Songs update failed: {error}"));
                                    show_toast(&ui, "Couldn't update Liked Songs", true);
                                }
                            }
                        });
                    });
                }
                _ => {}
            }
        }
    });

    // A new playlist, empty or with one song in it. It goes to the top of the library, where
    // Spotify puts it too, without waiting for the library to be fetched again.
    ui.on_create_playlist({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        let ui_weak = ui.as_weak();
        move |name, song_uri| {
            let name = name.trim().to_string();
            let song_uri = song_uri.to_string();
            if name.is_empty() {
                return;
            }
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let result = async {
                    let token = current_token(&state).await?;
                    let playlist = spotify_api.create_playlist(&token, &name).await?;
                    // Made already, so a song that won't go in still leaves the playlist.
                    let added = match track_id_from_uri(&playlist.uri).filter(|_| !song_uri.is_empty()) {
                        Some(id) => Some(spotify_api.add_tracks_to_playlist(&token, &id, &[song_uri.clone()]).await.is_ok()),
                        None => None,
                    };
                    Ok::<_, anyhow::Error>((playlist, added))
                }
                .await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    match result {
                        Ok((playlist, added)) => {
                            let shown_name = playlist.name.clone();
                            add_new_playlist_to_library(&ui, &state, playlist, added == Some(true));
                            match added {
                                None => ui.invoke_open_playlist(1),
                                Some(true) => show_toast(&ui, &format!("Added to {shown_name}"), false),
                                Some(false) => show_toast(&ui, &format!("Made {shown_name}, but couldn't add the song"), true),
                            }
                        }
                        Err(error) => {
                            append_log(&ui, &format!("Creating a playlist failed: {error}"));
                            show_toast(&ui, "Couldn't create the playlist", true);
                        }
                    }
                });
            });
        }
    });

    // Home
    ui.on_home_opened({
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        let ui_weak = ui.as_weak();
        move || {
            // Nothing still loading may replace the list behind Home.
            {
                let mut guard = state.lock().unwrap();
                guard.track_view_generation = guard.track_view_generation.wrapping_add(1);
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_track_view_loading(false);
            }
            let stale = HOME_VIEW.with(|view| view.borrow().fetched_at.is_none_or(|at| at.elapsed() > HOME_MAX_AGE));
            if stale {
                refresh_home(ui_weak.clone(), Arc::clone(&state), spotify_api.clone());
            }
        }
    });
    ui.on_save_library_size({
        let state = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut guard = state.lock().unwrap();
            guard.config.compact_sidebars = ui.get_compact_sidebars();
            guard.config.library_width = ui.get_library_width().round().max(0.0) as u32;
            guard.config.panel_width = ui.get_panel_width().round().max(0.0) as u32;
            config::save_config_background(&guard.config);
        }
    });
    ui.on_refresh_home({
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        let ui_weak = ui.as_weak();
        move || refresh_home(ui_weak.clone(), Arc::clone(&state), spotify_api.clone())
    });
    ui.on_home_section_shown({
        let ui_weak = ui.as_weak();
        move |section, shown| {
            let ui_weak = ui_weak.clone();
            // This comes while the list is being laid out, which can't change its data.
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                // The mini player's window is far shorter than Home, which is still there behind it:
                // judged by that, rows still on Home's screen would lose their covers, and rows
                // whose layout doesn't change with the width never ask for them again.
                let sections_kept = if ui.get_mini_mode() {
                    usize::MAX
                } else {
                    let window = ui.window();
                    let rows_on_screen = window.size().to_logical(window.scale_factor()).height / HOME_SHORTEST_ROW;
                    rows_on_screen as usize + 2
                };
                home_section_shown(section.max(0) as usize, shown.max(0) as usize, sections_kept);
            });
        }
    });
    ui.on_home_toggle_section(|section| {
        HOME_VIEW.with(|view| {
            let mut view = view.borrow_mut();
            let Some(model) = view.model.clone() else { return };
            let Some(entry) = view.sections.get_mut(section.max(0) as usize) else { return };
            entry.expanded = !entry.expanded;
            if let Some(mut row) = model.row_data(section as usize) {
                row.expanded = entry.expanded;
                model.set_row_data(section as usize, row);
            }
        });
    });
    ui.on_open_home_card({
        let state = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        move |section, card| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let chosen = home_card_at(section, card);
            let Some((chosen, cover)) = chosen else { return };
            // Liked Songs and the owner's playlists already have their own pages.
            match library_index_of(&chosen, &state) {
                Some(index) => ui.invoke_open_playlist(index),
                None => {
                    open_detail(DetailSource::Card { card: chosen, cover_shown: cover.is_some() });
                    // Already decoded for the card, so it needn't wait for the songs.
                    if let Some(cover) = cover {
                        ui.set_track_view_art(cover);
                        ui.set_track_view_has_art(true);
                    }
                }
            }
        }
    });

    // A card's play button plays it from the top and stays on Home. The songs load first if
    // they haven't been, and a later press wins over one still loading.
    ui.on_play_home_card({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        let ui_weak = ui.as_weak();
        move |section, card| {
            static LATEST_PRESS: AtomicU64 = AtomicU64::new(0);
            let chosen = home_card_at(section, card);
            let Some((chosen, _)) = chosen else { return };
            let library_index = library_index_of(&chosen, &state);
            let press = LATEST_PRESS.fetch_add(1, Ordering::SeqCst) + 1;
            let session = state.lock().unwrap().session;
            let ui_weak = ui_weak.clone();
            let player_runtime = Arc::clone(&runtime);
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let loaded = match library_index {
                    Some(index) => library_playlist_songs(index as usize, &state, &spotify_api).await.map(|_| ()),
                    None => home_card_songs(&chosen, &state, &spotify_api).await.map(|_| ()),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    // Pressed again since, or by an account that has since signed out.
                    if LATEST_PRESS.load(Ordering::SeqCst) != press || state.lock().unwrap().session != session {
                        return;
                    }
                    match (loaded, library_index) {
                        (Ok(()), Some(index)) => ui.invoke_activate_playlist(index),
                        (Ok(()), None) => play_from_list(&ui_weak, &player_runtime, &state, &spotify_api, chosen.uri, 0, false),
                        (Err(error), _) => {
                            append_log(&ui, &format!("Couldn't play {}: {error}", chosen.title));
                            show_toast(&ui, "Couldn't play that", true);
                        }
                    }
                });
            });
        }
    });

    // Liked state follows the current song.
    let ui_weak = ui.as_weak();
    ui.on_check_liked({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let uri = ui.get_current_track_uri().to_string();
            let cached = !uri.is_empty()
                && state
                    .lock()
                    .unwrap()
                    .playlist_track_cache
                    .get(LIKED_SONGS_PLAYLIST_URI)
                    .is_some_and(|tracks| tracks.iter().any(|track| track.uri == uri));
            ui.set_track_liked(cached);
            if uri.is_empty() {
                return;
            }
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let Ok(token) = current_token(&state).await else { return };
                let Ok(liked) = spotify_api.library_contains(&token, &uri).await else { return };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        if ui.get_current_track_uri() == uri.as_str() {
                            ui.set_track_liked(liked);
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_toggle_like({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let uri = ui.get_current_track_uri().to_string();
            if uri.is_empty() {
                return;
            }
            let liked = !ui.get_track_liked();
            ui.set_track_liked(liked);
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            let session = state.lock().unwrap().session;
            runtime.spawn(async move {
                let result = async {
                    let token = current_token(&state).await?;
                    spotify_api.set_in_library(&token, &uri, liked).await?;
                    Ok::<_, anyhow::Error>(if liked {
                        match track_id_from_uri(&uri) {
                            Some(id) => spotify_api.track(&token, &id).await.ok(),
                            None => None,
                        }
                    } else {
                        None
                    })
                }
                .await;
                {
                    // Signed out or into another account meanwhile: this was for the one before.
                    // Checked in the same hold of the lock as the change, so a sign-out can't
                    // come in between.
                    let mut guard = state.lock().unwrap();
                    if ensure_same_session(&guard, session).is_err() {
                        return;
                    }
                    match (&result, liked) {
                        (Ok(track), false) | (Ok(track @ Some(_)), true) => remember_liked(&mut guard, &uri, track.as_ref()),
                        // Liked, but the song's details didn't come. The song playing has them,
                        // if it's this one. If not, Liked Songs is fetched afresh when it next
                        // opens, rather than taking this as an unlike.
                        (Ok(None), true) => match guard.now_playing.clone().filter(|song| song.uri == uri) {
                            Some(song) => remember_liked(&mut guard, &uri, Some(&song)),
                            None => forget_liked_songs_list(&mut guard),
                        },
                        (Err(_), _) => {}
                    }
                }
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    if state.lock().unwrap().session != session {
                        return;
                    }
                    match result {
                        Ok(_) => {
                            show_toast(&ui, if liked { "Added to Liked Songs" } else { "Removed from Liked Songs" }, false);
                            let liked_tracks = {
                                let guard = state.lock().unwrap();
                                (guard.selected_playlist_uri == LIKED_SONGS_PLAYLIST_URI)
                                    .then(|| guard.playlist_track_cache.get(LIKED_SONGS_PLAYLIST_URI).cloned())
                                    .flatten()
                            };
                            if let Some(tracks) = liked_tracks {
                                refresh_tracks_in_place(&ui, tracks);
                            }
                        }
                        Err(error) => {
                            if ui.get_current_track_uri() == uri.as_str() {
                                ui.set_track_liked(!liked);
                            }
                            append_log(&ui, &format!("Liked Songs update failed: {error}"));
                            let needs_permission = error.to_string().contains("403") || error.to_string().to_lowercase().contains("scope");
                            show_toast(
                                &ui,
                                if needs_permission { "Reconnect Spotify in Settings once to allow liking songs" } else { "Couldn't update Liked Songs" },
                                true,
                            );
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    // Covers are decoded for the scale they're shown at, which isn't known until the
    // window first opens, and changes when it moves to another monitor.
    {
        let ui_weak = ui.as_weak();
        let state = Arc::clone(&state);
        ui.on_scale_changed(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            PLAYLIST_COVERS.with(|cached| cached.borrow_mut().clear());
            let guard = state.lock().unwrap();
            apply_playlist_rows(
                &ui,
                &guard.config.library_snapshot.playlists,
                liked_count(&guard.config.library_snapshot),
                guard.config.artwork_enabled,
                Some(guard.selected_playlist_uri.as_str()),
            );
        });
    }

    ui.on_filter_playlist_picker(move |query| {
        let Some(ui) = ui_weak.upgrade() else { return };
        let titles = ui.get_playlist_titles().iter().collect::<Vec<_>>();
        let indices = picker_matches(titles.iter().map(|title| title.as_str()), &query);
        ui.set_picker_indices(ModelRc::from(Rc::new(VecModel::from(indices))));
    });

    let ui_weak = ui.as_weak();
    ui.on_add_to_playlist({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |uri, playlist_index| {
            let uri = uri.to_string();
            if uri.is_empty() || playlist_index <= 0 {
                return;
            }
            let playlist = {
                let guard = state.lock().unwrap();
                let at = playlist_index as usize - 1;
                guard.playlist_uris.get(at).cloned().zip(
                    guard.config.library_snapshot.playlists.get(at).map(|playlist| playlist.name.clone()),
                )
            };
            let Some((playlist_uri, playlist_name)) = playlist else { return };
            let Some(playlist_id) = track_id_from_uri(&playlist_uri) else { return };
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let result = async {
                    let token = current_token(&state).await?;
                    spotify_api.add_tracks_to_playlist(&token, &playlist_id, &[uri]).await
                }
                .await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        match result {
                            Ok(()) => show_toast(&ui, &format!("Added to {playlist_name}"), false),
                            Err(error) => {
                                append_log(&ui, &format!("Add to playlist failed: {error}"));
                                show_toast(&ui, "Couldn't add to that playlist", true);
                            }
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_play_queue_item({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |index| {
            if index < 0 {
                return;
            }
            let index = index as usize;
            let queue = current_queue_tracks(&state);
            let Some(selected) = queue.get(index).cloned() else {
                return;
            };
            let queue_uris = queue.iter().map(|track| track.uri.clone()).collect::<Vec<_>>();
            let generation = {
                let mut guard = state.lock().unwrap();
                let source = guard
                    .playlist_track_cache
                    .get(&guard.active_queue_playlist_uri)
                    .map(|tracks| tracks.iter().map(|track| track.uri.clone()).collect::<Vec<_>>())
                    .unwrap_or_default();
                guard.queue_override_tracks = queue[index + 1..].to_vec();
                guard.queue_override_active = true;
                guard.track_start_generation = guard.track_start_generation.wrapping_add(1);
                (guard.track_start_generation, source)
            };
            let (generation, source) = generation;
            let uris = uris_from_queue_click(&queue_uris, index, &source);
            remember_last_track_uri(&state, current_session(&state), &selected.uri);
            if let Some(ui) = ui_weak.upgrade() {
                set_now_playing_from_track(&ui, &state, &selected, NowPlayingUpdate::Optimistic);
                repaint_queue(&ui, &state);
            }
            spawn_track_start_command(
                ui_weak.clone(),
                Arc::clone(&runtime),
                Arc::clone(&state),
                spotify_api.clone(),
                selected.uri.clone(),
                uris,
                generation,
            );
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_remove_queue_item({
        let state = Arc::clone(&state);
        move |index| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            if index >= 0 {
                edit_queue(&ui, &state, |tracks, painted| {
                    remove_painted_track(tracks, painted, index as usize);
                });
                // The next row has come up under the pointer, ready for its X to be clicked too.
                window_chrome::sync_hover_to_pointer(&ui);
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_clear_queue({
        let state = Arc::clone(&state);
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                edit_queue(&ui, &state, |tracks, _| tracks.clear());
            }
        }
    });

    bind_playback_callbacks(ui, runtime, state, spotify_api);
}

fn bind_playback_callbacks(
    ui: &AppWindow,
    runtime: Arc<Runtime>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) {
    let ui_weak = ui.as_weak();
    ui.on_play_pause({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            if state.lock().unwrap().parked.is_some() {
                pick_up_left_off(&ui_weak, &runtime, &state, PickUp::WhereItWas);
                return;
            }
            let mut should_start_when_empty = false;
            if let Some(ui) = ui_weak.upgrade() {
                let next_is_playing = !ui.get_is_playing();
                should_start_when_empty = next_is_playing;
                ui.set_is_playing(next_is_playing);
                ui.set_connection_status(if next_is_playing {
                    "Connected - Playing".into()
                } else {
                    "Connected - Paused".into()
                });

                let mut guard = state.lock().unwrap();
                if guard.local_is_active() {
                    audio_output::set_paused(!next_is_playing);
                }
                let current_progress = guard.playback_clock.displayed_progress_ms();
                let duration_ms = guard.playback_clock.duration_ms;
                guard.playback_clock.sync(current_progress, duration_ms, next_is_playing);
                guard.optimistic_is_playing = Some(next_is_playing);
                guard.optimistic_playback_until = Some(Instant::now() + Duration::from_secs(1));
            }
            let ui_weak_inner = ui_weak.clone();
            let state_inner = Arc::clone(&state);
            let spotify_api_inner = spotify_api.clone();
            runtime.spawn(async move {
                let (token, device, use_local) = {
                    let guard = state_inner.lock().unwrap();
                    (
                        guard.token.clone(),
                        guard.device_id.clone(),
                        guard.local_is_active(),
                    )
                };

                let command_result = if use_local {
                    run_local_player_action_with_recovery(
                        Arc::clone(&state_inner),
                        ui_weak_inner.clone(),
                        spotify_api_inner.clone(),
                        LocalPlayerAction::PlayPause,
                    )
                    .await
                } else if let Some(token) = token {
                    toggle_playback(&spotify_api_inner, &token, device.as_deref()).await
                } else {
                    Err(anyhow!("not connected to Spotify"))
                };

                let result = match command_result {
                    Err(error)
                        if should_start_when_empty
                            && (error.to_string().contains("nothing is currently queued to resume")
                                || error.to_string().contains("no local Spotify device ready")) =>
                    {
                        if error.to_string().contains("no local Spotify device ready") {
                            clear_local_playback_engine(&state_inner);
                            let _ = ensure_local_playback(Arc::clone(&state_inner), ui_weak_inner.clone()).await;
                            wait_for_local_device_refresh(
                                ui_weak_inner.clone(),
                                Arc::clone(&state_inner),
                                spotify_api_inner.clone(),
                            )
                            .await;
                        }
                        let _ = refresh_everything(
                            ui_weak_inner.clone(),
                            Arc::clone(&state_inner),
                            spotify_api_inner.clone(),
                            false,
                            false,
                            false,
                        )
                        .await;
                        start_current_visible_tracks(Arc::clone(&state_inner), spotify_api_inner.clone()).await
                    }
                    other => other,
                };
                if result.is_ok() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    {
                        let mut guard = state_inner.lock().unwrap();
                        guard.optimistic_is_playing = None;
                        guard.optimistic_playback_until = None;
                    }
                    let _ = refresh_playback_only(
                        ui_weak_inner.clone(),
                        Arc::clone(&state_inner),
                        spotify_api_inner.clone(),
                    )
                    .await;
                }

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        if let Err(error) = result {
                            if let Ok(mut guard) = state_inner.lock() {
                                let current_progress = guard.playback_clock.displayed_progress_ms();
                                let duration_ms = guard.playback_clock.duration_ms;
                                let reverted = !ui.get_is_playing();
                                guard.playback_clock.sync(current_progress, duration_ms, reverted);
                                guard.optimistic_is_playing = None;
                                guard.optimistic_playback_until = None;
                            }
                            ui.set_is_playing(!ui.get_is_playing());
                            audio_output::set_paused(false);
                            ui.set_connection_status(if ui.get_is_playing() {
                                "Connected - Playing".into()
                            } else {
                                "Connected - Paused".into()
                            });
                            append_log(&ui, &format!("Playback command failed: {error}"));
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_previous_track({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            if state.lock().unwrap().parked.is_some() {
                pick_up_left_off(&ui_weak, &runtime, &state, PickUp::FromTheStart);
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                optimistic_previous(&ui, &state);
            }
            let use_local = {
                let guard = state.lock().unwrap();
                guard.local_is_active()
            };
            if use_local {
                audio_output::begin_skip();
                begin_fast_playback_refresh(&state, Duration::from_secs(3));
                let ui_weak_inner = ui_weak.clone();
                let state_inner = Arc::clone(&state);
                let spotify_api_inner = spotify_api.clone();
                runtime.spawn(async move {
                    let result = run_local_player_action_with_recovery(
                        Arc::clone(&state_inner),
                        ui_weak_inner.clone(),
                        spotify_api_inner.clone(),
                        LocalPlayerAction::Previous,
                    )
                    .await;
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let _ = refresh_playback_only(
                        ui_weak_inner.clone(),
                        Arc::clone(&state_inner),
                        spotify_api_inner.clone(),
                    )
                    .await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            if let Err(error) = result {
                                append_log(&ui, &format!("Playback command failed: {error}"));
                            }
                        }
                    });
                });
            } else {
                spawn_device_command(ui_weak.clone(), Arc::clone(&runtime), Arc::clone(&state), spotify_api.clone(), |api, token, device| async move {
                    api.previous(&token, device.as_deref()).await
                });
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_next_track({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            if state.lock().unwrap().parked.is_some() {
                pick_up_left_off(&ui_weak, &runtime, &state, PickUp::NextSong);
                return;
            }
            let use_local = {
                let guard = state.lock().unwrap();
                guard.local_is_active()
            };
            if use_local {
                skip_here(&ui_weak, &runtime, &state, &spotify_api, LocalPlayerAction::Next);
            } else {
                spawn_device_command(ui_weak.clone(), Arc::clone(&runtime), Arc::clone(&state), spotify_api.clone(), |api, token, device| async move {
                    api.next(&token, device.as_deref()).await
                });
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_toggle_shuffle({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let enabled = if let Some(ui) = ui_weak.upgrade() {
                let next = !ui.get_shuffle_enabled();
                ui.set_shuffle_enabled(next);
                let mut guard = state.lock().unwrap();
                if guard.parked.is_some() {
                    // Nothing is playing to tell. It plays this way when it starts.
                    drop(guard);
                    note_left_off(&ui, &state);
                    return;
                }
                guard.optimistic_shuffle_enabled = Some(next);
                guard.optimistic_shuffle_until = Some(Instant::now() + Duration::from_secs(1));
                next
            } else {
                true
            };
            let ui_weak_inner = ui_weak.clone();
            let state_inner = Arc::clone(&state);
            let spotify_api_inner = spotify_api.clone();
            runtime.spawn(async move {
                let (token, device) = {
                    let guard = state_inner.lock().unwrap();
                    (guard.token.clone(), guard.device_id.clone())
                };
                let Some(token) = token else {
                    return;
                };
                let result = spotify_api_inner.shuffle(&token, device.as_deref(), enabled).await;
                if result.is_ok() {
                    // The new order takes a moment to reach Spotify. The queue then follows it
                    // now, rather than at the next regular refresh, so that no edit lands on the
                    // old order.
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    {
                        let mut guard = state_inner.lock().unwrap();
                        guard.queue_edited_at = None;
                        guard.last_spotify_queue_refresh = None;
                    }
                    let _ = refresh_playback_only(
                        ui_weak_inner.clone(),
                        Arc::clone(&state_inner),
                        spotify_api_inner.clone(),
                    )
                    .await;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        if let Err(error) = result {
                            if let Ok(mut guard) = state_inner.lock() {
                                guard.optimistic_shuffle_enabled = None;
                                guard.optimistic_shuffle_until = None;
                            }
                            ui.set_shuffle_enabled(!enabled);
                            append_log(&ui, &format!("Playback command failed: {error}"));
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_cycle_repeat({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move || {
            let (previous_mode, next_mode) = if let Some(ui) = ui_weak.upgrade() {
                let previous = ui.get_repeat_mode().to_string();
                let next = cycle_repeat_mode(&previous);
                ui.set_repeat_mode(next.clone().into());
                let mut guard = state.lock().unwrap();
                if guard.parked.is_some() {
                    drop(guard);
                    note_left_off(&ui, &state);
                    return;
                }
                guard.optimistic_repeat_mode = Some(next.clone());
                guard.optimistic_repeat_until = Some(Instant::now() + Duration::from_secs(1));
                (previous, next)
            } else {
                ("off".to_string(), "context".to_string())
            };
            let ui_weak_inner = ui_weak.clone();
            let state_inner = Arc::clone(&state);
            let spotify_api_inner = spotify_api.clone();
            runtime.spawn(async move {
                let (token, device) = {
                    let guard = state_inner.lock().unwrap();
                    (guard.token.clone(), guard.device_id.clone())
                };
                let Some(token) = token else {
                    return;
                };
                let result = spotify_api_inner.repeat(&token, device.as_deref(), &next_mode).await;
                if result.is_ok() {
                    let _ = refresh_playback_only(
                        ui_weak_inner.clone(),
                        Arc::clone(&state_inner),
                        spotify_api_inner.clone(),
                    )
                    .await;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        if let Err(error) = result {
                            if let Ok(mut guard) = state_inner.lock() {
                                guard.optimistic_repeat_mode = None;
                                guard.optimistic_repeat_until = None;
                            }
                            ui.set_repeat_mode(previous_mode.into());
                            append_log(&ui, &format!("Playback command failed: {error}"));
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_activate_playlist({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |index| {
            if index < 0 {
                return;
            }
            let liked_track_uris = if index == 0 {
                Some(state.lock().unwrap().playlist_track_cache.get(LIKED_SONGS_PLAYLIST_URI).cloned().unwrap_or_default()
                    .into_iter()
                    .map(|track| track.uri)
                    .collect::<Vec<_>>())
            } else {
                None
            };
            let uri = if index == 0 {
                Some(LIKED_SONGS_PLAYLIST_URI.to_string())
            } else {
                state.lock().unwrap().playlist_uris.get(index as usize - 1).cloned()
            };
              if let Some(context_uri) = uri {
                  if context_uri == LIKED_SONGS_PLAYLIST_URI {
                      {
                          let liked_tracks = state
                              .lock()
                              .unwrap()
                              .playlist_track_cache
                              .get(LIKED_SONGS_PLAYLIST_URI)
                              .cloned()
                              .unwrap_or_default();
                          let mut guard = state.lock().unwrap();
                          guard.active_queue_playlist_uri = LIKED_SONGS_PLAYLIST_URI.to_string();
                          guard.queue_override_tracks = liked_tracks.into_iter().skip(1).take(MAX_LOCAL_QUEUE_TRACKS).collect();
                          guard.queue_override_active = true;
                      }
                      if let Some(track_uris) = liked_track_uris.filter(|tracks| !tracks.is_empty()) {
                          spawn_start_command(ui_weak.clone(), Arc::clone(&runtime), Arc::clone(&state), spotify_api.clone(), move |api, token, device_id| async move {
                              api.play_tracks(&token, &device_id, &track_uris).await
                        });
                    }
                    return;
                  }
                  remember_last_playlist_uri(&state, current_session(&state), &context_uri);
                  {
                      let playlist_tracks = state
                          .lock()
                          .unwrap()
                          .playlist_track_cache
                          .get(&context_uri)
                          .cloned()
                          .unwrap_or_default();
                      let mut guard = state.lock().unwrap();
                      guard.active_queue_playlist_uri = context_uri.clone();
                      guard.queue_override_tracks = playlist_tracks.into_iter().skip(1).take(MAX_LOCAL_QUEUE_TRACKS).collect();
                      guard.queue_override_active = true;
                  }
                  spawn_start_command(ui_weak.clone(), Arc::clone(&runtime), Arc::clone(&state), spotify_api.clone(), move |api, token, device_id| async move {
                      api.play_playlist(&token, &device_id, &context_uri).await
                  });
            }
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_activate_liked_song({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |index| {
            if index < 0 {
                return;
            }
            let list_uri = state.lock().unwrap().selected_playlist_uri.clone();
            if list_uri == SEARCH_RESULTS_URI
                && ui_weak
                    .upgrade()
                    .is_some_and(|ui| play_search_result_next(&ui, &runtime, &state, &spotify_api, index as usize))
            {
                return;
            }
            let remember = list_uri != LIKED_SONGS_PLAYLIST_URI && list_uri != SEARCH_RESULTS_URI;
            play_from_list(&ui_weak, &runtime, &state, &spotify_api, list_uri, index as usize, remember);
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_track_context_action({
        let runtime = Arc::clone(&runtime);
        let state = Arc::clone(&state);
        let spotify_api = spotify_api.clone();
        move |action, index| {
            if index < 0 {
                return;
            }
            let action = action.to_string();
            if action != "queue" && action != "play-next" {
                return;
            }

            let (track, token, device_id, is_local) = {
                let guard = state.lock().unwrap();
                let track = guard
                    .playlist_track_cache
                    .get(&guard.selected_playlist_uri)
                    .and_then(|tracks| tracks.get(index as usize))
                    .cloned();
                (
                    track,
                    guard.token.clone(),
                    guard.device_id.clone(),
                    guard.local_is_active(),
                )
            };

            let Some(track) = track else {
                if let Some(ui) = ui_weak.upgrade() {
                    append_log(&ui, "Queue action failed: no track selected.");
                }
                return;
            };
            let track_uri = track.uri.clone();
            let track_name = track.name.clone();

            if is_local {
                if let Some(ui) = ui_weak.upgrade() {
                    edit_queue(&ui, &state, |tracks, _| {
                        if action == "play-next" {
                            tracks.insert(0, track);
                        } else {
                            tracks.push(track);
                        }
                    });
                    append_log(
                        &ui,
                        &format!("{} {}.", if action == "play-next" { "Playing next:" } else { "Added to queue:" }, track_name),
                    );
                }
                return;
            }
            let Some(token) = token else {
                if let Some(ui) = ui_weak.upgrade() {
                    append_log(&ui, "Queue action failed: not connected to Spotify.");
                }
                return;
            };
            if action == "play-next" {
                // Spotify's API can only append to a remote device's queue, so
                // say so instead of quietly doing something else.
                if let Some(ui) = ui_weak.upgrade() {
                    show_toast(&ui, "Play next only works while playing on this PC. Use Add to queue instead.", true);
                }
                return;
            }

            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                let result = spotify_api.add_to_queue(&token, device_id.as_deref(), &track_uri).await;
                if result.is_ok() {
                    let _ = refresh_playback_only(
                        ui_weak.clone(),
                        Arc::clone(&state),
                        spotify_api.clone(),
                    )
                    .await;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        match result {
                            Ok(()) => append_log(&ui, &format!("Added {} to queue.", track_name)),
                            Err(error) => append_log(&ui, &format!("Add to queue failed: {error}")),
                        }
                    }
                });
            });
        }
    });
}

/// Keeps Liked Songs and the saved library in step with a song just liked (`Some`) or
/// unliked (`None`), so menus know without asking Spotify.
fn remember_liked(guard: &mut AppState, uri: &str, song: Option<&FullTrack>) {
    let mut was_loaded = false;
    if let Some(songs) = guard.playlist_track_cache.get_mut(LIKED_SONGS_PLAYLIST_URI) {
        was_loaded = songs.iter().any(|known| known.uri == uri);
        songs.retain(|known| known.uri != uri);
        if let Some(song) = song {
            songs.insert(0, song.clone());
        }
    }
    // A new like goes at the top, moving the rest down one, and an unlike takes one out. A part
    // still loading was counted from before either, so it's numbered afresh.
    let (more, fewer) = (song.is_some() && !was_loaded, song.is_none());
    if more || fewer {
        guard.load_stamp = guard.load_stamp.wrapping_add(1);
        let stamp = guard.load_stamp;
        if let Some(loaded) = guard.loaded.get_mut(LIKED_SONGS_PLAYLIST_URI) {
            loaded.stamp = stamp;
            if more {
                loaded.total += 1;
                loaded.more_from = loaded.more_from.map(|from| from + 1);
            } else {
                loaded.total = loaded.total.saturating_sub(1);
                if was_loaded {
                    loaded.more_from = loaded.more_from.map(|from| from.saturating_sub(1));
                }
            }
        }
    }
    let total = &mut guard.config.library_snapshot.liked_songs_total;
    if *total > 0 {
        *total = if more { *total + 1 } else if fewer { total.saturating_sub(1) } else { *total };
    }
    let saved = &mut guard.config.library_snapshot.liked_songs;
    saved.retain(|known| known.uri != uri);
    if let Some(song) = song {
        saved.insert(0, CachedLikedSong {
            name: song.name.clone(),
            artists: song.artists.iter().map(|artist| artist.name.clone()).collect(),
            uri: song.uri.clone(),
            duration_ms: song.duration_ms,
            album: song.album.name.clone(),
            image_url: song.album.images.last().map(|image| image.url.clone()),
        });
    }
}

/// Drops the songs of Liked Songs held here, so they're fetched afresh when it next opens.
fn forget_liked_songs_list(guard: &mut AppState) {
    guard.playlist_track_cache.remove(LIKED_SONGS_PLAYLIST_URI);
    guard.loaded.remove(LIKED_SONGS_PLAYLIST_URI);
    guard.load_stamp = guard.load_stamp.wrapping_add(1);
}

/// A playlist just made, at the top of the library.
fn add_new_playlist_to_library(ui: &AppWindow, state: &Mutex<AppState>, playlist: models::PlaylistItem, has_song: bool) {
    let (playlists, liked_count, artwork_enabled, selected_uri) = {
        let mut guard = state.lock().unwrap();
        let count = Some(u32::from(has_song));
        guard.playlists_revision = guard.playlists_revision.wrapping_add(1);
        // One more in the library too, so "Load more" still knows how many are left.
        if guard.config.library_snapshot.playlists_total > 0 {
            guard.config.library_snapshot.playlists_total += 1;
        }
        guard.playlist_uris.insert(0, playlist.uri.clone());
        guard.playlist_track_hrefs.insert(0, playlist.tracks.href.clone());
        guard.playlist_track_counts.insert(0, count);
        guard.config.library_snapshot.playlists.insert(0, CachedPlaylist {
            name: playlist.name.clone(),
            uri: playlist.uri.clone(),
            image_url: None,
            track_count: count,
            snapshot_id: playlist.snapshot_id.clone(),
        });
        config::save_config_background(&guard.config);
        (
            guard.config.library_snapshot.playlists.clone(),
            liked_count(&guard.config.library_snapshot),
            guard.config.artwork_enabled,
            guard.selected_playlist_uri.clone(),
        )
    };
    // Everything below Liked Songs moves down one.
    let selected = ui.get_selected_playlist_index();
    apply_playlist_rows(ui, &playlists, liked_count, artwork_enabled, Some(selected_uri.as_str()));
    if selected > 0 {
        ui.set_selected_playlist_index(selected + 1);
    }
}

/// Which sidebar rows the playlist picker shows. Row 0 is Liked Songs, which
/// isn't a playlist you can add to, so it is never offered.
fn picker_matches<'a>(titles: impl Iterator<Item = &'a str>, query: &str) -> Vec<i32> {
    let query = query.trim().to_lowercase();
    titles
        .enumerate()
        .skip(1)
        .filter(|(_, title)| title.to_lowercase().contains(&query))
        .map(|(index, _)| index as i32)
        .collect()
}

fn apply_cached_library_to_ui(ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let guard = state.lock().unwrap();
    apply_playlist_rows(
        ui,
        &guard.config.library_snapshot.playlists,
        liked_count(&guard.config.library_snapshot),
        guard.config.artwork_enabled,
        Some(guard.selected_playlist_uri.as_str()),
    );
    ui.set_more_playlists(more_playlists(&guard.config.library_snapshot));
    ui.set_queue_summary("Queue unavailable until playback starts".into());
    clear_queue_rows(ui);
    ui.set_device_titles(ModelRc::from(Rc::new(VecModel::<SharedString>::from(Vec::new()))));
    ui.set_device_subtitles(ModelRc::from(Rc::new(VecModel::<SharedString>::from(Vec::new()))));
    ui.set_device_badges(ModelRc::from(Rc::new(VecModel::<SharedString>::from(Vec::new()))));
    ui.set_device_images(ModelRc::from(Rc::new(VecModel::<slint::Image>::from(Vec::new()))));
    ui.set_device_is_active(ModelRc::from(Rc::new(VecModel::<bool>::from(Vec::new()))));
    ui.set_device_is_local(ModelRc::from(Rc::new(VecModel::<bool>::from(Vec::new()))));
}

fn apply_playlist_rows(
    ui: &AppWindow,
    playlists: &[CachedPlaylist],
    liked_count: usize,
    artwork_enabled: bool,
    selected_playlist_uri: Option<&str>,
) {
    let mut titles = vec![SharedString::from("Liked Songs")];
    let mut subtitles = vec![SharedString::from(format!("{liked_count} songs"))];
    let mut images = vec![load_icon("liked-songs.svg")];
    let mut covers_in_use = std::collections::HashSet::new();

    titles.extend(playlists.iter().map(|playlist| SharedString::from(playlist.name.clone())));
    subtitles.extend(playlists.iter().map(|playlist| {
        SharedString::from(
            playlist
                .track_count
                .map(|count| format!("{count} songs"))
                .unwrap_or_else(|| "Playlist".to_string()),
        )
    }));
    images.extend(playlists.iter().enumerate().map(|(index, playlist)| {
        let should_load_art = index < MAX_EAGER_PLAYLIST_ARTWORK_ROWS
            || selected_playlist_uri.is_some_and(|uri| uri == playlist.uri);
        if artwork_enabled && should_load_art {
            playlist
                .image_url
                .as_deref()
                .and_then(cached_album_art_path)
                .map(|path| {
                    let cover = load_playlist_cover(&path, ui.window().scale_factor());
                    covers_in_use.insert(path);
                    cover
                })
                .unwrap_or_default()
        } else {
            slint::Image::default()
        }
    }));
    PLAYLIST_COVERS.with(|cached| cached.borrow_mut().retain(|path, _| covers_in_use.contains(path)));

    ui.set_playlist_titles(ModelRc::from(Rc::new(VecModel::from(titles))));
    ui.set_playlist_subtitles(ModelRc::from(Rc::new(VecModel::from(subtitles))));
    ui.set_playlist_images(ModelRc::from(Rc::new(VecModel::from(images))));
}

/// Tracks currently shown in the main table. Lives on the UI thread because
/// the Slint model is `Rc`-based.
#[derive(Default)]
struct TrackView {
    source: Vec<FullTrack>,
    filter: String,
    model: Option<Rc<VecModel<TrackItem>>>,
    generation: u64,
    /// Thumbnails already decoded, so filtering never re-fetches them.
    art_by_url: HashMap<String, slint::Image>,
    /// Displayed rows waiting for each thumbnail.
    rows_by_url: HashMap<String, Vec<usize>>,
    /// Songs shown, songs in the list, and how long those shown play.
    counts: (usize, usize, i64),
    /// How long the whole list is on Spotify, while only part of it is loaded.
    in_spotify: Option<usize>,
}

thread_local! {
    static TRACK_VIEW: RefCell<TrackView> = RefCell::new(TrackView::default());
    static TOAST_SEQ: RefCell<u64> = const { RefCell::new(0) };
}

/// Rows decoded eagerly when a list is shown; the rest stream in.
const EAGER_ROW_ART: usize = 60;

fn row_art_url(track: &FullTrack) -> Option<&str> {
    // Spotify lists images largest-first; rows only need the smallest one.
    track.album.images.last().map(|image| image.url.as_str())
}

fn track_matches(track: &FullTrack, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    track.name.to_lowercase().contains(needle)
        || track.album.name.to_lowercase().contains(needle)
        || track.artists.iter().any(|artist| artist.name.to_lowercase().contains(needle))
}

fn track_item(source_index: usize, track: &FullTrack, art: Option<slint::Image>) -> TrackItem {
    TrackItem {
        source_index: source_index as i32,
        number: SharedString::from((source_index + 1).to_string()),
        title: SharedString::from(track.name.as_str()),
        artists: SharedString::from(track.artists.iter().map(|artist| artist.name.as_str()).collect::<Vec<_>>().join(", ")),
        album: SharedString::from(track.album.name.as_str()),
        duration: SharedString::from(if track.duration_ms > 0 { format_time(track.duration_ms) } else { String::new() }),
        uri: SharedString::from(track.uri.as_str()),
        has_art: art.is_some(),
        art: art.unwrap_or_default(),
    }
}

/// Only touches the links when the artists changed: a new list rebuilds them, which
/// would lose the hover and any click in progress on every playback update.
fn show_track_artists(ui: &AppWindow, artists: &[models::Artist]) {
    let names: Vec<SharedString> = if artists.is_empty() {
        // One empty link keeps the line's height, with nothing to click.
        vec![SharedString::default()]
    } else {
        artists.iter().map(|artist| SharedString::from(artist.name.as_str())).collect()
    };
    let shown = ui.get_track_artists();
    if shown.row_count() == names.len() && names.iter().enumerate().all(|(index, name)| shown.row_data(index).as_ref() == Some(name)) {
        return;
    }
    ui.set_track_artists(ModelRc::from(Rc::new(VecModel::from(names))));
}

/// The artist behind a clicked name. If Spotify's details spell it differently, the
/// page still opens for the name clicked, only without a photo.
fn clicked_artist(artists: Vec<models::Artist>, clicked: &str) -> models::Artist {
    artists
        .into_iter()
        .find(|artist| artist.name == clicked)
        .unwrap_or_else(|| models::Artist { id: None, name: clicked.to_string() })
}

/// Where a list page's songs come from.
#[derive(Clone)]
enum DetailSource {
    PlayingAlbum,
    PlayingArtist(String),
    Card { card: home::HomeCard, cover_shown: bool },
    /// From a song's menu.
    Artist(models::Artist),
    Album(models::Album),
    Radio(FullTrack),
    History,
}

type DetailPage = (String, &'static str, Vec<FullTrack>, Option<PathBuf>);

/// The connection Home reads through. It comes up with the player, a moment after launch.
/// Recently played songs, read with a token from the player's own login.
async fn recently_played_through_player(state: &Arc<Mutex<AppState>>, spotify_api: &spotify::SpotifyApi) -> Result<Vec<(i64, FullTrack)>> {
    let session = spotify_session(state).await?;
    let token = session
        .token_provider()
        .get_token("user-read-recently-played")
        .await
        .map_err(|error| anyhow!("couldn't get a token for history: {error}"))?;
    let token = TokenStore { access_token: token.access_token, refresh_token: String::new(), expires_at_epoch: 0 };
    spotify_api.recently_played(&token).await
}

async fn spotify_session(state: &Arc<Mutex<AppState>>) -> Result<librespot_core::Session> {
    for attempt in 0..40 {
        let session = state.lock().unwrap().playback_engine.as_ref().map(|engine| engine.session());
        if let Some(session) = session.filter(|session| !session.is_invalid()) {
            return Ok(session);
        }
        if attempt < 39 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    Err(anyhow!("the player isn't connected to Spotify yet"))
}

/// An artist's most played songs, then more from search.
async fn artist_page(
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    token: &TokenStore,
    artist: models::Artist,
    with_photo: bool,
) -> Result<DetailPage> {
    let mut songs = Vec::new();
    let session = state.lock().unwrap().playback_engine.as_ref().map(|engine| engine.session());
    if let (Some(id), Some(session)) = (artist.id.as_deref(), session) {
        if let Ok(top) = home::artist_top_song_uris(&session, id).await {
            songs = home::songs_by_uri(&session, &top).await;
        }
    }
    match spotify_api.artist_tracks(token, &artist.name).await {
        Ok(found) => {
            for song in found {
                if !songs.iter().any(|known| known.uri == song.uri) {
                    songs.push(song);
                }
            }
        }
        Err(error) if songs.is_empty() => return Err(error),
        Err(_) => {}
    }
    // A missing photo shouldn't lose the page.
    let photo = match artist.id.as_deref().filter(|_| with_photo) {
        Some(artist_id) => match spotify_api.artist_image_url(token, artist_id).await {
            Ok(Some(url)) => load_album_art(&url).await.ok(),
            _ => None,
        },
        None => None,
    };
    Ok((artist.name, "Artist", songs, photo))
}

async fn load_detail_page(
    source: DetailSource,
    playing_uri: &str,
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
) -> Result<DetailPage> {
    let token = current_token(state).await?;
    let playing = || async {
        let id = track_id_from_uri(playing_uri).ok_or_else(|| anyhow!("no song playing"))?;
        spotify_api.track(&token, &id).await
    };
    match source {
        DetailSource::PlayingAlbum => {
            let track = playing().await?;
            let tracks = spotify_api.album_tracks(&token, &track.album).await?;
            Ok((track.album.name, "Album", tracks, None))
        }
        DetailSource::PlayingArtist(clicked) => {
            let track = playing().await?;
            artist_page(state, spotify_api, &token, clicked_artist(track.artists, &clicked), true).await
        }
        DetailSource::Card { card, cover_shown } if card.kind == home::CardKind::Artist => {
            let artist = models::Artist { id: Some(card.id().to_string()), name: card.title.clone() };
            artist_page(state, spotify_api, &token, artist, !cover_shown).await
        }
        DetailSource::Artist(artist) => artist_page(state, spotify_api, &token, artist, true).await,
        DetailSource::Album(album) => {
            let tracks = spotify_api.album_tracks(&token, &album).await?;
            Ok((album.name, "Album", tracks, None))
        }
        DetailSource::Radio(song) => {
            let session = spotify_session(state).await?;
            let radio = home::song_radio_uri(&session, &song.uri).await?;
            let uris = home::context_song_uris(&session, &radio).await?;
            let songs = home::songs_by_uri(&session, &uris).await;
            if songs.is_empty() {
                return Err(anyhow!("Spotify sent an empty radio"));
            }
            let cover = match song.album.images.first() {
                Some(image) => load_album_art(&image.url).await.ok(),
                None => None,
            };
            Ok((format!("{} Radio", song.name), "Radio", songs, cover))
        }
        DetailSource::History => {
            let from_spotify = match spotify_api.recently_played(&token).await {
                Ok(songs) => Ok(songs),
                // A login from before history came didn't ask Spotify for it. The player's own
                // login can, without signing in again.
                Err(error) if error.to_string().contains("Reconnect Spotify") => {
                    recently_played_through_player(state, spotify_api).await.map_err(|_| error)
                }
                Err(error) => Err(error),
            };
            // Spotify leaves out what this PC's player plays, so those come from Spotlight's
            // own list.
            let account = state.lock().unwrap().config.account_id.clone();
            let played_here = tokio::task::spawn_blocking(move || history::load(account.as_deref())).await.unwrap_or_default();
            let from_spotify = match from_spotify {
                Ok(songs) => songs,
                Err(_) if !played_here.is_empty() => Vec::new(),
                Err(error) => return Err(error),
            };
            Ok((HISTORY_TITLE.to_string(), HISTORY_KICKER, history::merge(from_spotify, played_here), None))
        }
        DetailSource::Card { card, cover_shown } => {
            let songs = home_card_songs(&card, state, spotify_api).await?;
            let cover = match card.image_url.as_deref().filter(|_| !cover_shown) {
                Some(url) => load_album_art(url).await.ok(),
                None => None,
            };
            let kicker = card.kicker();
            Ok((card.title, kicker, songs, cover))
        }
    }
}

/// A Home card's songs, from the cache or fetched and kept there under the card's link, which
/// is where playing them looks.
async fn home_card_songs(
    card: &home::HomeCard,
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
) -> Result<Vec<FullTrack>> {
    let (known, session) = {
        let guard = state.lock().unwrap();
        // A playlist in the library is fresh while it hasn't been edited since, as when it's
        // opened from the sidebar. Anything else, such as a mix, is only as fresh as when it
        // was fetched, which Home's refresh forgets.
        let fresh = match guard.home_songs_at.get(&card.uri) {
            Some(fetched) => fetched.elapsed() < HOME_MAX_AGE,
            None => guard
                .config
                .library_snapshot
                .playlists
                .iter()
                .find(|playlist| playlist.uri == card.uri)
                .is_some_and(|playlist| {
                    playlist.snapshot_id.is_none() || guard.playlist_cache_versions.get(&card.uri) == playlist.snapshot_id.as_ref()
                }),
        };
        (fresh.then(|| guard.playlist_track_cache.get(&card.uri).cloned()).flatten(), guard.session)
    };
    if let Some(songs) = known.filter(|songs| !songs.is_empty()) {
        return Ok(songs);
    }
    let songs = if card.kind == home::CardKind::Artist {
        let token = current_token(state).await?;
        let artist = models::Artist { id: Some(card.id().to_string()), name: card.title.clone() };
        artist_page(state, spotify_api, &token, artist, false).await?.2
    } else {
        let session = spotify_session(state).await?;
        let uris = home::context_song_uris(&session, &card.uri).await?;
        home::songs_by_uri(&session, &uris).await
    };
    if songs.is_empty() {
        return Err(anyhow!("no songs to show"));
    }
    let mut guard = state.lock().unwrap();
    // Signed out or into another account meanwhile: these were for the one before.
    ensure_same_session(&guard, session)?;
    let in_library = guard.config.library_snapshot.playlists.iter().find(|playlist| playlist.uri == card.uri).map(|playlist| playlist.snapshot_id.clone());
    match in_library {
        Some(Some(version)) => {
            guard.playlist_cache_versions.insert(card.uri.clone(), version);
        }
        Some(None) => {}
        None => {
            guard.home_songs_at.insert(card.uri.clone(), Instant::now());
        }
    }
    guard.playlist_track_cache.insert(card.uri.clone(), songs.clone());
    Ok(songs)
}

/// Fetching the Home page again after this long, when it's opened. The refresh button
/// does it any time.
const HOME_MAX_AGE: Duration = Duration::from_secs(30 * 60);
const HOME_CARD_PIXELS: u32 = 220;
const HOME_TILE_PIXELS: u32 = 96;
/// Home's shortest row of cards, for working out how many rows fit on screen. Covers
/// further away than that are let go.
const HOME_SHORTEST_ROW: f32 = 240.0;
static HOME_ART_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);

#[derive(Clone, Copy, PartialEq)]
enum CoverState {
    Missing,
    Loading,
    Shown,
}

struct HomeSectionView {
    data: home::HomeSection,
    cards: Rc<VecModel<HomeCard>>,
    covers: Vec<CoverState>,
    expanded: bool,
}

#[derive(Default)]
struct HomeView {
    sections: Vec<HomeSectionView>,
    model: Option<Rc<VecModel<HomeSection>>>,
    fetched_at: Option<Instant>,
    loading: bool,
    generation: u64,
    /// Goes up when the account goes, so a load for the account before is ignored.
    account: u64,
}

thread_local! {
    static HOME_VIEW: RefCell<HomeView> = RefCell::new(HomeView::default());
}

/// A card on Home, with its cover when one is on screen.
fn home_card_at(section: i32, card: i32) -> Option<(home::HomeCard, Option<slint::Image>)> {
    HOME_VIEW.with(|view| {
        let view = view.borrow();
        let section = view.sections.get(section.max(0) as usize)?;
        let index = card.max(0) as usize;
        let cover = section.cards.row_data(index).filter(|row| row.has_art).map(|row| row.art);
        Some((section.data.cards.get(index)?.clone(), cover))
    })
}

/// Where a Home card is in the library list, when it's Liked Songs (0) or one of the owner's
/// playlists, which have their own pages.
fn library_index_of(card: &home::HomeCard, state: &Mutex<AppState>) -> Option<i32> {
    match card.kind {
        home::CardKind::LikedSongs => Some(0),
        home::CardKind::Playlist => state.lock().unwrap().playlist_uris.iter().position(|uri| *uri == card.uri).map(|index| index as i32 + 1),
        _ => None,
    }
}

fn show_home_sections(ui: &AppWindow, sections: Vec<home::HomeSection>) {
    let model = Rc::new(VecModel::from(Vec::<HomeSection>::new()));
    let views: Vec<HomeSectionView> = sections
        .into_iter()
        .map(|data| {
            let cards: Vec<HomeCard> = data
                .cards
                .iter()
                .map(|card| HomeCard {
                    title: card.title.as_str().into(),
                    subtitle: card.subtitle.as_str().into(),
                    art: Default::default(),
                    has_art: false,
                    round: card.kind == home::CardKind::Artist,
                })
                .collect();
            let covers = vec![CoverState::Missing; cards.len()];
            let cards = Rc::new(VecModel::from(cards));
            model.push(HomeSection { title: data.title.as_str().into(), cards: ModelRc::from(cards.clone()), tiles: data.tiles, expanded: false });
            HomeSectionView { data, cards, covers, expanded: false }
        })
        .collect();
    // An empty last row lets the final cards scroll clear of the player bar.
    model.push(HomeSection::default());
    HOME_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        view.generation = view.generation.wrapping_add(1);
        view.sections = views;
        view.model = Some(model.clone());
        view.fetched_at = Some(Instant::now());
    });
    ui.set_home_sections(ModelRc::from(model));
}

/// Home belongs to the account, so it goes when the account does.
fn forget_home(ui: &AppWindow) {
    HOME_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        view.generation = view.generation.wrapping_add(1);
        view.sections.clear();
        view.model = None;
        view.account = view.account.wrapping_add(1);
        view.fetched_at = None;
        // A load still under way was for the account before, and is ignored when it ends.
        view.loading = false;
    });
    ui.set_show_home(false);
    ui.set_home_loading(false);
    ui.set_home_error("".into());
    ui.set_home_sections(ModelRc::default());
}

fn refresh_home(ui_weak: slint::Weak<AppWindow>, state: Arc<Mutex<AppState>>, spotify_api: spotify::SpotifyApi) {
    let (already, account) = HOME_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        (std::mem::replace(&mut view.loading, true), view.account)
    });
    let Some(handle) = RUNTIME_HANDLE.get().filter(|_| !already) else {
        return;
    };
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_home_loading(true);
        ui.set_home_error("".into());
    }
    handle.spawn(async move {
        let result = async {
            let session = spotify_session(&state).await?;
            let mut sections = home::fetch_home(&session).await?;
            // Home still works without these.
            let followed = match current_token(&state).await {
                Ok(token) => spotify_api.followed_artists(&token).await.unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let followed = followed
                .into_iter()
                .map(|(name, id, picture)| home::HomeCard {
                    title: name,
                    subtitle: String::new(),
                    uri: format!("spotify:artist:{id}"),
                    image_url: picture,
                    kind: home::CardKind::Artist,
                })
                .collect();
            home::add_followed_artists(&mut sections, followed);
            Ok::<_, anyhow::Error>(sections)
        }
        .await;
        let _ = slint::invoke_from_event_loop(move || {
            let current = HOME_VIEW.with(|view| {
                let mut view = view.borrow_mut();
                let current = view.account == account;
                if current {
                    view.loading = false;
                }
                current
            });
            // Signed out meanwhile: this Home was for the account before.
            if !current {
                return;
            }
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_home_loading(false);
            match result {
                Ok(sections) if !sections.is_empty() => {
                    // Its mixes may have new songs under the same links now.
                    state.lock().unwrap().home_songs_at.clear();
                    show_home_sections(&ui, sections);
                }
                Ok(_) => ui.set_home_error("Spotify sent an empty Home page.".into()),
                Err(error) => {
                    append_log(&ui, &format!("Home failed to load: {error}"));
                    ui.set_home_error(format!("Couldn't load Home: {error}").into());
                }
            }
        });
    });
}

fn home_section_shown(section_index: usize, shown: usize, sections_kept: usize) {
    let (generation, tiles, wanted) = HOME_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        let generation = view.generation;
        // Far from view, a cover is only using memory. It comes back from disk.
        for (index, section) in view.sections.iter_mut().enumerate() {
            if index.abs_diff(section_index) > sections_kept {
                for (card, cover) in section.covers.iter_mut().enumerate() {
                    if *cover != CoverState::Missing {
                        *cover = CoverState::Missing;
                        if let Some(mut row) = section.cards.row_data(card) {
                            row.art = Default::default();
                            row.has_art = false;
                            section.cards.set_row_data(card, row);
                        }
                    }
                }
            }
        }
        let Some(section) = view.sections.get_mut(section_index) else {
            return (generation, false, Vec::new());
        };
        let mut wanted = Vec::new();
        for card in 0..shown.min(section.data.cards.len()) {
            if section.covers[card] == CoverState::Missing {
                if let Some(url) = section.data.cards[card].image_url.clone() {
                    section.covers[card] = CoverState::Loading;
                    wanted.push((card, url));
                }
            }
        }
        (generation, section.data.tiles, wanted)
    });
    let Some(handle) = RUNTIME_HANDLE.get() else { return };
    let size = if tiles { HOME_TILE_PIXELS } else { HOME_CARD_PIXELS };
    for (card, url) in wanted {
        handle.spawn(async move {
            let pixels = async {
                let _slot = HOME_ART_SLOTS.acquire().await.ok()?;
                let path = load_album_art(&url).await.ok()?;
                tokio::task::spawn_blocking(move || home::thumbnail(&path, size)).await.ok()?
            }
            .await
            .map(|(width, height, pixels)| slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&pixels, width, height));
            let _ = slint::invoke_from_event_loop(move || {
                HOME_VIEW.with(|view| {
                    let mut view = view.borrow_mut();
                    if view.generation != generation {
                        return;
                    }
                    let Some(section) = view.sections.get_mut(section_index) else { return };
                    // It may have been let go while this was on its way.
                    if section.covers[card] != CoverState::Loading {
                        return;
                    }
                    match pixels {
                        Some(pixels) => {
                            section.covers[card] = CoverState::Shown;
                            if let Some(mut row) = section.cards.row_data(card) {
                                row.art = slint::Image::from_rgba8(pixels);
                                row.has_art = true;
                                section.cards.set_row_data(card, row);
                            }
                        }
                        None => section.covers[card] = CoverState::Missing,
                    }
                });
            });
        });
    }
}

fn format_total_duration(total_ms: i64) -> String {
    let minutes = total_ms / 60_000;
    if minutes >= 60 {
        format!("{} hr {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

/// Replace the main track list. Resets the in-playlist filter.
fn show_tracks(ui: &AppWindow, tracks: Vec<FullTrack>) {
    ui.set_track_filter("".into());
    // A long library list offers the rest again once it's shown, by `show_more_to_load`.
    ui.set_track_view_more(false);
    ui.set_track_view_more_busy(false);
    TRACK_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        view.source = tracks;
        view.filter.clear();
        view.in_spotify = None;
        // 64px thumbnails are ~16 KB each. Other lists' covers decode again quickly from the
        // disk, so once there are many only this list's stay.
        if view.art_by_url.len() > 300 {
            let view = &mut *view;
            let shown: std::collections::HashSet<&str> = view.source.iter().filter_map(row_art_url).collect();
            view.art_by_url.retain(|url, _| shown.contains(url.as_str()));
        }
    });
    apply_track_filter(ui);
    ui.invoke_scroll_tracks_top();
}

/// Swap in fresher data for the same list (e.g. a library refresh) without
/// disturbing the filter or scroll, and keep the same song selected.
fn refresh_tracks_in_place(ui: &AppWindow, tracks: Vec<FullTrack>) {
    let selected_uri = usize::try_from(ui.get_selected_liked_song_index())
        .ok()
        .and_then(|index| ui.get_track_rows().row_data(index))
        .map(|row| row.uri);
    TRACK_VIEW.with(|view| view.borrow_mut().source = tracks);
    apply_track_filter(ui);
    let new_index = selected_uri.and_then(|uri| {
        let rows = ui.get_track_rows();
        (0..rows.row_count()).find(|index| rows.row_data(*index).is_some_and(|row| row.uri == uri))
    });
    ui.set_selected_liked_song_index(new_index.map(|index| index as i32).unwrap_or(-1));
}

fn set_track_filter(ui: &AppWindow, filter: &str) {
    TRACK_VIEW.with(|view| view.borrow_mut().filter = filter.trim().to_lowercase());
    apply_track_filter(ui);
}

fn apply_track_filter(ui: &AppWindow) {
    let (mut rows, generation, missing) = TRACK_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        // A new list supersedes any thumbnail stream still running for the old one.
        view.generation = view.generation.wrapping_add(1);
        let view = &mut *view;
        let mut rows = Vec::new();
        let mut rows_by_url: HashMap<String, Vec<usize>> = HashMap::new();
        let mut total_ms: i64 = 0;
        for (index, track) in view.source.iter().enumerate() {
            if !track_matches(track, &view.filter) {
                continue;
            }
            total_ms += track.duration_ms.max(0) as i64;
            let url = row_art_url(track);
            let mut art = url.and_then(|url| view.art_by_url.get(url).cloned());
            if art.is_none() && rows.len() < EAGER_ROW_ART {
                if let Some(url) = url {
                    if let Some(path) = cached_album_art_path(url) {
                        let image = load_image_from_path(&path);
                        view.art_by_url.insert(url.to_string(), image.clone());
                        art = Some(image);
                    }
                }
            }
            if art.is_none() {
                if let Some(url) = url {
                    rows_by_url.entry(url.to_string()).or_default().push(rows.len());
                }
            }
            rows.push(track_item(index, track, art));
        }
        // Stream in display order so the visible rows fill first.
        let mut missing = rows_by_url.keys().cloned().collect::<Vec<_>>();
        missing.sort_by_key(|url| rows_by_url[url].first().copied().unwrap_or(usize::MAX));
        view.rows_by_url = rows_by_url;
        view.counts = (rows.len(), view.source.len(), total_ms);
        (rows, view.generation, missing)
    });
    ui.set_tracks_have_album(rows.iter().any(|row| !row.album.is_empty()));
    if !rows.is_empty() {
        // An empty last row lets the final songs scroll clear of the player bar.
        rows.push(TrackItem { source_index: -1, ..Default::default() });
    }
    let model = Rc::new(VecModel::from(rows));
    ui.set_track_rows(ModelRc::from(model.clone()));
    TRACK_VIEW.with(|view| view.borrow_mut().model = Some(model));
    show_track_meta(ui);
    stream_row_art(ui.as_weak(), generation, missing);
}

/// The line under the list's title: how many songs, and how long they play.
fn show_track_meta(ui: &AppWindow) {
    let meta = TRACK_VIEW.with(|view| {
        let view = view.borrow();
        let (shown, total, total_ms) = view.counts;
        let songs = |n: usize| if n == 1 { "1 song".to_string() } else { format!("{n} songs") };
        match view.in_spotify {
            // Only part is loaded, which says why the list ends where it does.
            Some(in_spotify) => format!("{shown} of {}", songs(in_spotify)),
            None if shown == total && total_ms > 0 => format!("{} • {}", songs(total), format_total_duration(total_ms)),
            None if shown == total => songs(total),
            None => format!("{shown} of {}", songs(total)),
        }
    });
    ui.set_track_view_meta(meta.into());
}

/// Offers the rest of the library list shown, if only part of it is loaded.
fn show_more_to_load(ui: &AppWindow, state: &Mutex<AppState>) {
    let in_spotify = {
        let guard = state.lock().unwrap();
        guard
            .loaded
            .get(&guard.selected_playlist_uri)
            .filter(|loaded| loaded.more_from.is_some())
            .map(|loaded| loaded.total)
    };
    TRACK_VIEW.with(|view| view.borrow_mut().in_spotify = in_spotify);
    ui.set_track_view_more(in_spotify.is_some());
    show_track_meta(ui);
}

/// Whether the library has more playlists than are loaded.
fn more_playlists(snapshot: &models::LibrarySnapshot) -> bool {
    snapshot.playlists_total > snapshot.playlists.len()
}

/// A library list's songs from the start, as far as was asked for, fetched and kept with how
/// much more there is. Liked Songs too.
async fn fetch_list_songs(
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    token: &TokenStore,
    session: u64,
    list_uri: &str,
) -> Result<Vec<FullTrack>> {
    let wanted = songs_wanted(&state.lock().unwrap(), list_uri);
    let part = fetch_songs(state, spotify_api, token, list_uri, 0, wanted).await?;
    let mut guard = state.lock().unwrap();
    ensure_same_session(&guard, session)?;
    mark_loaded(&mut guard, list_uri, Loaded { total: part.total, wanted, more_from: part.more_from, stamp: 0 });
    guard.playlist_track_cache.insert(list_uri.to_string(), part.items.clone());
    Ok(part.items)
}

/// About `count` of a library list's songs from `from` on.
async fn fetch_songs(
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    token: &TokenStore,
    list_uri: &str,
    from: usize,
    count: usize,
) -> Result<spotify::Part<FullTrack>> {
    if list_uri == LIKED_SONGS_PLAYLIST_URI {
        return spotify_api.liked_songs(token, from, count).await;
    }
    let href = {
        let guard = state.lock().unwrap();
        guard
            .playlist_uris
            .iter()
            .position(|uri| uri == list_uri)
            .and_then(|index| guard.playlist_track_hrefs.get(index).filter(|href| !href.is_empty()).cloned())
    };
    match href {
        Some(href) => spotify_api.playlist_tracks_by_href(token, &href, from, count).await,
        None => {
            let playlist_id = spotify_playlist_id_from_uri(list_uri).ok_or_else(|| anyhow!("invalid playlist URI"))?;
            spotify_api.playlist_tracks(token, &playlist_id, from, count).await
        }
    }
}

/// The next part of a long list, added to what's kept of it. Gives the whole list as it now
/// stands, or `None` if the list changed meanwhile, as a library refresh can.
async fn load_more_songs(
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    session: u64,
    list_uri: &str,
    loaded: Loaded,
) -> Result<Option<Vec<FullTrack>>> {
    let Some(from) = loaded.more_from else { return Ok(None) };
    let token = current_token(state).await?;
    let part = fetch_songs(state, spotify_api, &token, list_uri, from, spotify::SONGS_PER_PART).await?;
    let mut guard = state.lock().unwrap();
    ensure_same_session(&guard, session)?;
    if guard.loaded.get(list_uri) != Some(&loaded) {
        return Ok(None);
    }
    let Some(tracks) = guard.playlist_track_cache.get_mut(list_uri) else { return Ok(None) };
    if list_uri == LIKED_SONGS_PLAYLIST_URI {
        // A like elsewhere can move a song across the cut. A playlist can hold a song twice.
        let known: std::collections::HashSet<String> = tracks.iter().map(|track| track.uri.clone()).collect();
        tracks.extend(part.items.into_iter().filter(|track| !known.contains(&track.uri)));
    } else {
        tracks.extend(part.items);
    }
    let tracks = tracks.clone();
    mark_loaded(
        &mut guard,
        list_uri,
        Loaded { total: part.total, wanted: from + spotify::SONGS_PER_PART, more_from: part.more_from, stamp: 0 },
    );
    if list_uri == LIKED_SONGS_PLAYLIST_URI {
        guard.config.library_snapshot.liked_songs_total = part.total;
    }
    if guard.selected_playlist_uri == list_uri {
        guard.track_uris = tracks.iter().map(|track| track.uri.clone()).collect();
    }
    Ok(Some(tracks))
}

/// Download (if needed) and decode row thumbnails in the background, patching
/// rows in place so scroll position and selection are kept.
fn stream_row_art(ui_weak: slint::Weak<AppWindow>, generation: u64, urls: Vec<String>) {
    if urls.is_empty() {
        return;
    }
    let Some(handle) = RUNTIME_HANDLE.get() else {
        return;
    };
    handle.spawn(async move {
        for url in urls {
            let still_current = {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = slint::invoke_from_event_loop(move || {
                    let _ = tx.send(TRACK_VIEW.with(|view| view.borrow().generation) == generation);
                });
                rx.await.unwrap_or(false)
            };
            if !still_current {
                return;
            }
            let Ok(path) = load_album_art(&url).await else {
                continue;
            };
            let ui_weak = ui_weak.clone();
            let url_for_ui = url.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if ui_weak.upgrade().is_none() {
                    return;
                }
                TRACK_VIEW.with(|view| {
                    let mut view = view.borrow_mut();
                    if view.generation != generation {
                        return;
                    }
                    let image = load_image_from_path(&path);
                    view.art_by_url.insert(url_for_ui.clone(), image.clone());
                    let rows = view.rows_by_url.remove(&url_for_ui).unwrap_or_default();
                    let Some(model) = view.model.as_ref() else {
                        return;
                    };
                    for row_index in rows {
                        if let Some(mut row) = model.row_data(row_index) {
                            row.art = image.clone();
                            row.has_art = true;
                            model.set_row_data(row_index, row);
                        }
                    }
                });
            });
        }
    });
}

/// Brief confirmation/error message shown above the player bar.
/// Settings' update rows, and the check when Spotlight opens and once a day after.
fn bind_updates(ui: &AppWindow, runtime: &Arc<Runtime>, http: reqwest::Client, check_automatically: bool) {
    let kind = updates::this_copy();
    ui.set_app_version(updates::CURRENT_VERSION.into());
    ui.set_update_installs_here(kind.installs_itself());
    UPDATE_AUTO_CHECK.store(check_automatically, Ordering::Relaxed);

    ui.on_check_for_update({
        let ui_weak = ui.as_weak();
        let runtime = Arc::clone(runtime);
        let http = http.clone();
        move || {
            if UPDATE_BUSY.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_update_busy("checking".into());
                ui.set_update_note("".into());
            }
            let (ui_weak, http) = (ui_weak.clone(), http.clone());
            runtime.spawn(async move {
                check_for_update(ui_weak, http, kind, false).await;
            });
        }
    });

    ui.on_open_update_notes(|| {
        let page = UPDATE_FOUND.lock().unwrap().as_ref().map_or_else(updates::releases_page, |release| release.page.clone());
        let _ = webbrowser::open(&page);
    });

    ui.on_install_update({
        let ui_weak = ui.as_weak();
        let runtime = Arc::clone(runtime);
        let http = http.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let Some(release) = UPDATE_FOUND.lock().unwrap().clone() else {
                return;
            };
            // A copy that can't install it gets the release page, to download it by hand.
            let Some(download) = release.download.clone().filter(|_| kind.installs_itself()) else {
                let _ = webbrowser::open(&release.page);
                return;
            };
            // Without a checksum the download can't be checked against the release, so the
            // user says first that it's fine.
            if download.sha256.is_none() && !ui.get_update_confirming() {
                ui.set_update_confirming(true);
                return;
            }
            ui.set_update_confirming(false);
            let destination = match updates::download_path(kind, &download) {
                Ok(destination) => destination,
                Err(error) => {
                    ui.set_update_error(error.to_string().into());
                    return;
                }
            };
            if UPDATE_BUSY.swap(true, Ordering::SeqCst) {
                return;
            }
            ui.set_update_error("".into());
            ui.set_update_progress(0.0);
            ui.set_update_busy("downloading".into());
            let ui_weak = ui_weak.clone();
            let http = http.clone();
            runtime.spawn(async move {
                let progress_ui = ui_weak.clone();
                let result = updates::download(&http, &download, &destination, move |share| {
                    let ui_weak = progress_ui.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_update_progress(share);
                        }
                    });
                })
                .await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else {
                        return;
                    };
                    match result.and_then(|()| updates::start_install(kind, &destination)) {
                        Ok(()) => {
                            // The new version takes over from here.
                            tray::remove_icon();
                            config::exit_after_saving();
                        }
                        Err(error) => {
                            // Shown in the update's own banner, where Update was pressed.
                            write_log(&ui, &format!("Update failed: {error:#}"));
                            ui.set_update_error(error.to_string().into());
                            ui.set_update_busy("".into());
                            UPDATE_BUSY.store(false, Ordering::SeqCst);
                        }
                    }
                });
            });
        }
    });

    let ui_weak = ui.as_weak();
    runtime.spawn(async move {
        // Out of the way of everything Spotlight does as it opens. By now a Setup that
        // started this copy has finished, so what it left can go.
        tokio::time::sleep(Duration::from_secs(20)).await;
        let _ = tokio::task::spawn_blocking(updates::clean_up).await;
        let _ = tokio::task::spawn_blocking(trim_album_art_cache).await;
        const DAY: Duration = Duration::from_secs(24 * 60 * 60);
        let mut last_check: Option<std::time::SystemTime> = None;
        let mut hours_since_trim = 0u32;
        loop {
            // By the clock on the wall, so a day asleep counts.
            let due = last_check.map_or(true, |at| at.elapsed().map_or(true, |since| since >= DAY));
            if due && UPDATE_AUTO_CHECK.load(Ordering::Relaxed) && !UPDATE_BUSY.swap(true, Ordering::SeqCst) {
                let started = std::time::SystemTime::now();
                // One that fails, say with no connection yet, is tried again in an hour.
                if check_for_update(ui_weak.clone(), http.clone(), kind, true).await {
                    last_check = Some(started);
                }
            }
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
            // A copy left running in the tray for weeks keeps fetching covers too.
            hours_since_trim += 1;
            if hours_since_trim >= 24 {
                hours_since_trim = 0;
                let _ = tokio::task::spawn_blocking(trim_album_art_cache).await;
            }
        }
    });
}

/// Asks GitHub for a newer version and shows the answer. The caller has set `UPDATE_BUSY`.
/// Found without being asked, a new version gets a toast; failures then stay quiet. True if
/// GitHub answered.
async fn check_for_update(ui_weak: slint::Weak<AppWindow>, http: reqwest::Client, kind: updates::Kind, automatic: bool) -> bool {
    let result = updates::check(&http, kind).await;
    let answered = result.is_ok();
    let _ = slint::invoke_from_event_loop(move || {
        UPDATE_BUSY.store(false, Ordering::SeqCst);
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        if ui.get_update_busy() == "checking" {
            ui.set_update_busy("".into());
        }
        match result {
            Ok(Some(release)) => {
                let new_to_us = ui.get_update_version() != release.version.as_str();
                if new_to_us {
                    // A yes was for the file asked about, not for this one.
                    ui.set_update_confirming(false);
                }
                ui.set_update_version(release.version.as_str().into());
                ui.set_update_note("".into());
                if automatic && new_to_us {
                    show_toast(&ui, &format!("Spotlight {} is out. Update it in Settings.", release.version), false);
                }
                *UPDATE_FOUND.lock().unwrap() = Some(release);
            }
            Ok(None) => {
                ui.set_update_version("".into());
                ui.set_update_note("Up to date".into());
                *UPDATE_FOUND.lock().unwrap() = None;
            }
            Err(error) => {
                // A check by hand shows why beside its button. The daily one stays quiet: no
                // connection, or GitHub being busy, will pass.
                write_log(&ui, &format!("Update check failed: {error}"));
                if !automatic {
                    ui.set_update_note(error.to_string().into());
                }
            }
        }
    });
    answered
}

fn show_toast(ui: &AppWindow, message: &str, is_error: bool) {
    let seq = TOAST_SEQ.with(|seq| {
        let mut seq = seq.borrow_mut();
        *seq += 1;
        *seq
    });
    ui.set_toast_text(message.into());
    ui.set_toast_error(is_error);
    ui.set_toast_visible(true);
    let ui_weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(if is_error { 4_500 } else { 2_400 }), move || {
        if TOAST_SEQ.with(|current| *current.borrow()) != seq {
            return;
        }
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_toast_visible(false);
        }
    });
}

/// Average the artwork into one muted color for tinting surfaces.
fn ambient_color_from_image(image: &slint::Image, light: bool) -> Option<Color> {
    let buffer = image.to_rgba8()?;
    let pixels = buffer.as_slice();
    if pixels.is_empty() {
        return None;
    }
    let step = (pixels.len() / 1024).max(1);
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    for pixel in pixels.iter().step_by(step) {
        // Favor colorful pixels; a plain mean of most covers is mud.
        let max = pixel.r.max(pixel.g).max(pixel.b) as u64;
        let min = pixel.r.min(pixel.g).min(pixel.b) as u64;
        let weight = 1 + (max - min) * (max - min) / 64;
        r += pixel.r as u64 * weight;
        g += pixel.g as u64 * weight;
        b += pixel.b as u64 * weight;
        n += weight;
    }
    let (r, g, b) = ((r / n) as f32 / 255.0, (g / n) as f32 / 255.0, (b / n) as f32 / 255.0);
    let (h, s, _) = rgb_to_hsl(r, g, b);
    let (s, l) = if light { (s.min(0.55), 0.86) } else { (s.min(0.48), 0.22) };
    let (r, g, b) = even_brightness(hsl_to_rgb(h, s, l), light);
    Some(Color::from_rgb_u8((r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8))
}

/// HSL lightness makes a yellow or green tint several times brighter than a blue one, enough
/// that faint text on it gets hard to read. This brings the tint to an even brightness: no
/// brighter than a blue one in the dark theme, and no darker than a pale one in the light.
fn even_brightness((r, g, b): (f32, f32, f32), light: bool) -> (f32, f32, f32) {
    let to_linear = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    let to_srgb = |c: f32| if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    let (lr, lg, lb) = (to_linear(r), to_linear(g), to_linear(b));
    let luminance = 0.2126 * lr + 0.7152 * lg + 0.0722 * lb;
    let (lr, lg, lb) = if light {
        const LEAST: f32 = 0.72;
        if luminance >= LEAST {
            return (r, g, b);
        }
        let toward_white = (LEAST - luminance) / (1.0 - luminance);
        (lr + (1.0 - lr) * toward_white, lg + (1.0 - lg) * toward_white, lb + (1.0 - lb) * toward_white)
    } else {
        const MOST: f32 = 0.04;
        if luminance <= MOST {
            return (r, g, b);
        }
        let scale = MOST / luminance;
        (lr * scale, lg * scale, lb * scale)
    };
    (to_srgb(lr).clamp(0.0, 1.0), to_srgb(lg).clamp(0.0, 1.0), to_srgb(lb).clamp(0.0, 1.0))
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l);
    }
    let hue = |p: f32, q: f32, mut t: f32| {
        if t < 0.0 { t += 1.0; }
        if t > 1.0 { t -= 1.0; }
        if t < 1.0 / 6.0 { p + (q - p) * 6.0 * t }
        else if t < 0.5 { q }
        else if t < 2.0 / 3.0 { p + (q - p) * (2.0 / 3.0 - t) * 6.0 }
        else { p }
    };
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    (hue(p, q, h + 1.0 / 3.0), hue(p, q, h), hue(p, q, h - 1.0 / 3.0))
}

/// Liked songs are cached without full album metadata; rebuild lightweight
/// `FullTrack`s from the snapshot.
fn liked_tracks_from_snapshot(config: &AppConfig) -> Vec<FullTrack> {
    config
        .library_snapshot
        .liked_songs
        .iter()
        .map(|item| FullTrack {
            name: item.name.clone(),
            uri: item.uri.clone(),
            duration_ms: item.duration_ms,
            album: crate::models::Album {
                id: None,
                name: item.album.clone(),
                release_date: None,
                images: item
                    .image_url
                    .iter()
                    .map(|url| crate::models::AlbumImage { url: url.clone() })
                    .collect(),
            },
            artists: item.artists.iter().cloned().map(|name| crate::models::Artist { id: None, name }).collect(),
        })
        .collect()
}

fn build_queue_items(state: &Arc<Mutex<AppState>>) -> Vec<String> {
    current_queue_tracks(state)
        .iter()
        .map(|track| {
            format!(
                "{} - {}",
                track.name,
                track.artists.iter().map(|artist| artist.name.clone()).collect::<Vec<_>>().join(", ")
            )
        })
        .collect()
}

fn queue_summary_text(upcoming: usize) -> String {
    if upcoming == 0 {
        "Queue unavailable until playback starts".to_string()
    } else {
        format!("{upcoming} upcoming tracks")
    }
}

fn apply_device_rows(
    ui: &AppWindow,
    devices: &[crate::models::Device],
    device_name: &str,
) {
    let titles = devices
        .iter()
        .map(|d| SharedString::from(d.name.clone()))
        .collect::<Vec<_>>();
    let subtitles = devices
        .iter()
        .map(|d| {
            let kind = if d.name == device_name {
                "This device"
            } else if d.device_type.is_empty() {
                "Device"
            } else {
                d.device_type.as_str()
            };
            let state = if d.is_active { "Active" } else { "" };
            let volume = d.volume_percent.map(|v| format!("{}%", v)).unwrap_or_default();
            let parts = [kind, state, volume.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            SharedString::from(parts.join(" • "))
        })
        .collect::<Vec<_>>();
    let badges = devices
        .iter()
        .map(|d| SharedString::from(device_type_badge(&d.device_type)))
        .collect::<Vec<_>>();
    let images = devices
        .iter()
        .map(|d| load_icon(device_type_icon(&d.device_type)))
        .collect::<Vec<_>>();
    let active = devices.iter().map(|d| d.is_active).collect::<Vec<_>>();
    let local = devices.iter().map(|d| d.name == device_name).collect::<Vec<_>>();

    ui.set_device_titles(ModelRc::from(Rc::new(VecModel::from(titles))));
    ui.set_device_subtitles(ModelRc::from(Rc::new(VecModel::from(subtitles))));
    ui.set_device_badges(ModelRc::from(Rc::new(VecModel::from(badges))));
    ui.set_device_images(ModelRc::from(Rc::new(VecModel::from(images))));
    ui.set_device_is_active(ModelRc::from(Rc::new(VecModel::from(active))));
    ui.set_device_is_local(ModelRc::from(Rc::new(VecModel::from(local))));
}

thread_local! {
    /// What the queue shows: its songs, which have covers, and whether it can be edited.
    static QUEUE_PAINTED: RefCell<Option<(Vec<String>, Vec<bool>, bool)>> = const { RefCell::new(None) };
}

fn apply_queue_rows(
    ui: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    reorder_enabled: bool,
) {
    // Replacing the rows would lose one in motion. It repaints when it lands.
    if ui.get_queue_busy() {
        return;
    }
    let tracks = current_queue_tracks(state);
    let uris: Vec<String> = tracks.iter().map(|track| track.uri.clone()).collect();
    state.lock().unwrap().queue_painted_uris = uris.clone();
    let mut missing = Vec::new();
    let art_paths = tracks
        .iter()
        .map(|track| {
            let url = row_art_url(track)?;
            let path = cached_album_art_path(url);
            if path.is_none() {
                missing.push(url.to_string());
            }
            path
        })
        .collect::<Vec<_>>();
    // Spotify's queue is fetched every few seconds while music plays, and is mostly the same.
    let painted = (uris, art_paths.iter().map(Option::is_some).collect::<Vec<_>>(), reorder_enabled);
    if QUEUE_PAINTED.with(|last| last.borrow().as_ref() == Some(&painted)) {
        return;
    }
    QUEUE_PAINTED.with(|last| *last.borrow_mut() = Some(painted));
    let titles = tracks.iter().map(|t| SharedString::from(t.name.clone())).collect::<Vec<_>>();
    let artists = tracks
        .iter()
        .map(|t| SharedString::from(t.artists.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")))
        .collect::<Vec<_>>();
    let images = art_paths
        .iter()
        .map(|path| path.as_ref().map_or_else(slint::Image::default, load_image_from_path))
        .collect::<Vec<_>>();

    ui.set_queue_titles(ModelRc::from(Rc::new(VecModel::from(titles))));
    ui.set_queue_artists(ModelRc::from(Rc::new(VecModel::from(artists))));
    ui.set_queue_images(ModelRc::from(Rc::new(VecModel::from(images))));
    ui.set_queue_reorder_enabled(reorder_enabled);
    fetch_missing_queue_art(ui, state, missing);
}

/// However the queue changed, any thumbnail it lacks gets fetched, then the
/// queue repaints. Each URL is tried once so a failed download can't loop.
fn fetch_missing_queue_art(ui: &AppWindow, state: &Arc<Mutex<AppState>>, missing: Vec<String>) {
    thread_local! {
        static ATTEMPTED: RefCell<std::collections::HashSet<String>> = RefCell::new(Default::default());
    }
    let urls = ATTEMPTED.with(|attempted| {
        let mut attempted = attempted.borrow_mut();
        missing.into_iter().filter(|url| attempted.insert(url.clone())).collect::<Vec<_>>()
    });
    let Some(handle) = RUNTIME_HANDLE.get().filter(|_| !urls.is_empty()) else {
        return;
    };
    let ui_weak = ui.as_weak();
    let state = Arc::clone(state);
    handle.spawn(async move {
        let mut downloads = tokio::task::JoinSet::new();
        for url in urls {
            downloads.spawn(async move { load_album_art(&url).await.is_ok() });
        }
        let mut fetched_any = false;
        while let Some(result) = downloads.join_next().await {
            fetched_any |= result.unwrap_or(false);
        }
        if fetched_any {
            repaint_queue_on_event_loop(ui_weak, state);
        }
    });
}

fn optimistic_advance_queue(ui: &AppWindow, state: &Arc<Mutex<AppState>>) -> bool {
    let shuffled = ui.get_shuffle_enabled();
    let next_track = {
        let mut guard = state.lock().unwrap();
        if guard.queue_override_active {
            if guard.queue_override_tracks.is_empty() {
                return false;
            }
            let track = guard.queue_override_tracks.remove(0);
            track
        } else {
            let Some(current_track_uri) = guard.current_track_uri.as_ref() else {
                return false;
            };
            // The first row on screen, if that list is for this song.
            let spotify_next = guard
                .spotify_queue_tracks
                .as_ref()
                .filter(|_| guard.spotify_queue_current_uri.as_ref() == Some(current_track_uri))
                .and_then(|tracks| tracks.first());
            if let Some(next_track) = spotify_next {
                next_track.clone()
            } else {
                // In shuffle the next in the playlist isn't what's coming, so wait to be told.
                if shuffled {
                    return false;
                }
                let Some(tracks) = guard.playlist_track_cache.get(&guard.active_queue_playlist_uri) else {
                    return false;
                };
                let Some(current_index) = tracks.iter().position(|track| &track.uri == current_track_uri) else {
                    return false;
                };
                let Some(next_track) = tracks.get(current_index.saturating_add(1)).cloned() else {
                    return false;
                };
                next_track
            }
        }
    };

    set_now_playing_from_track(ui, state, &next_track, NowPlayingUpdate::Optimistic);
    begin_fast_playback_refresh(state, Duration::from_secs(4));
    ui.set_queue_summary(queue_summary_text(current_queue_tracks(state).len()).into());
    let reorder_enabled = {
        let guard = state.lock().unwrap();
        guard.local_is_selected()
    };
    apply_queue_rows(ui, state, reorder_enabled);
    true
}

fn optimistic_previous(ui: &AppWindow, state: &Arc<Mutex<AppState>>) -> bool {
    let previous_track = {
        let mut guard = state.lock().unwrap();
        if guard.playback_clock.displayed_progress_ms() > 3_000 {
            let duration = guard.playback_clock.duration_ms;
            let playing = guard.playback_clock.is_playing;
            guard.playback_clock.sync(0, duration, playing);
            guard.optimistic_playback_until = Some(Instant::now() + Duration::from_secs(2));
            ui.set_progress_ms(0);
            ui.set_elapsed("0:00".into());
            return true;
        }
        let Some(current_track_uri) = guard.current_track_uri.as_ref() else {
            return false;
        };
        let Some(tracks) = guard.playlist_track_cache.get(&guard.active_queue_playlist_uri) else {
            return false;
        };
        let Some(current_index) = tracks.iter().position(|track| &track.uri == current_track_uri) else {
            return false;
        };
        if current_index == 0 {
            return false;
        }
        tracks.get(current_index - 1).cloned()
    };
    let Some(previous_track) = previous_track else {
        return false;
    };
    set_now_playing_from_track(ui, state, &previous_track, NowPlayingUpdate::Optimistic);
    begin_fast_playback_refresh(state, Duration::from_secs(4));
    true
}

/// What the title bar says while Spotify asks Spotlight to slow down.
const RATE_LIMITED: &str = "Spotify asked Spotlight to wait";

/// What keeps Spotlight from keeping up with Spotify, after a background check, or "" for
/// nothing. A single miss passes unremarked, since the next check usually works.
fn sync_trouble(result: &Result<()>, misses: &mut u32) -> &'static str {
    let Err(error) = result else {
        *misses = 0;
        return "";
    };
    *misses = misses.saturating_add(1);
    let offline = error
        .chain()
        .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some_and(|error| error.is_connect() || error.is_timeout()));
    if error.downcast_ref::<spotify::RateLimitError>().is_some() || spotify::rate_limit_wait().is_some() {
        RATE_LIMITED
    } else if error.downcast_ref::<auth::SignInExpired>().is_some() {
        "Reconnect Spotify in Settings"
    } else if *misses < 2 {
        ""
    } else if offline {
        "Offline · trying again"
    } else {
        "Reconnecting to Spotify…"
    }
}

/// Puts `trouble` in the title bar, unless it's there already.
fn show_sync_trouble(ui_weak: &slint::Weak<AppWindow>, shown: &mut &'static str, trouble: &'static str) {
    if *shown == trouble {
        return;
    }
    *shown = trouble;
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_sync_trouble(trouble.into());
        }
    });
}

fn begin_fast_playback_refresh(state: &Arc<Mutex<AppState>>, duration: Duration) {
    let mut guard = state.lock().unwrap();
    guard.fast_playback_refresh_until = Some(Instant::now() + duration);
}

fn pending_track_blocks_observation(
    expected: Option<&str>,
    until: Option<Instant>,
    observed: &str,
    now: Instant,
) -> bool {
    expected.is_some_and(|expected| expected != observed)
        && until.is_some_and(|until| now < until)
}

/// While this PC is playing, librespot knows what is really coming out of the
/// speakers. Spotify's Web API can lag behind or keep reporting an old session,
/// so it never overrides the local player's track.
fn local_player_disagrees(state: &Arc<Mutex<AppState>>, track_uri: &str) -> bool {
    let guard = state.lock().unwrap();
    local_track_conflict(guard.local_is_active(), guard.latest_local_track_uri.as_deref(), track_uri)
}

fn local_track_conflict(local_active: bool, local_track: Option<&str>, reported: &str) -> bool {
    local_active && local_track.is_some_and(|local| local != reported)
}

fn should_ignore_stale_playback_track(state: &Arc<Mutex<AppState>>, track_uri: &str) -> bool {
    if local_player_disagrees(state, track_uri) {
        return true;
    }
    let mut guard = state.lock().unwrap();
    if guard.optimistic_track_uri.is_none() {
        return false;
    }
    if pending_track_blocks_observation(
        guard.optimistic_track_uri.as_deref(),
        guard.optimistic_track_until,
        track_uri,
        Instant::now(),
    ) {
        return true;
    }
    guard.optimistic_track_uri = None;
    guard.optimistic_track_until = None;
    false
}

async fn warm_queue_art_and_repaint(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) {
    let enriched_any = enrich_visible_queue_metadata(
        Arc::clone(&state),
        spotify_api,
        QUEUE_ARTWORK_WARM_LIMIT,
    )
    .await;
    if enriched_any {
        repaint_queue_on_event_loop(ui_weak.clone(), Arc::clone(&state));
    }
    let warmed_any = warm_queue_album_art_cache(Arc::clone(&state), QUEUE_ARTWORK_WARM_LIMIT).await;
    if !warmed_any {
        return;
    }

    repaint_queue_on_event_loop(ui_weak, state);
}

fn repaint_queue_on_event_loop(ui_weak: slint::Weak<AppWindow>, state: Arc<Mutex<AppState>>) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let reorder_enabled = {
                let guard = state.lock().unwrap();
                guard.local_is_selected()
            };
            apply_queue_rows(&ui, &state, reorder_enabled);
        }
    });
}

async fn warm_playlist_art_and_repaint(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
) {
    let image_urls = {
        let guard = state.lock().unwrap();
        if !guard.config.artwork_enabled {
            return;
        }
        guard
            .config
            .library_snapshot
            .playlists
            .iter()
            .filter_map(|playlist| playlist.image_url.clone())
            .filter(|url| cached_album_art_path(url).is_none())
            .take(PLAYLIST_ARTWORK_WARM_LIMIT)
            .collect::<Vec<_>>()
    };

    if image_urls.is_empty() {
        return;
    }

    let mut warmed_any = false;
    for url in image_urls {
        if load_album_art(&url).await.is_ok() {
            warmed_any = true;
        }
        tokio::time::sleep(Duration::from_millis(35)).await;
    }

    if !warmed_any {
        return;
    }

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let (playlists, liked_count, artwork_enabled, selected_playlist_uri) = {
                let guard = state.lock().unwrap();
                (
                    guard.config.library_snapshot.playlists.clone(),
                    liked_count(&guard.config.library_snapshot),
                    guard.config.artwork_enabled,
                    guard.selected_playlist_uri.clone(),
                )
            };
            apply_playlist_rows(
                &ui,
                &playlists,
                liked_count,
                artwork_enabled,
                Some(selected_playlist_uri.as_str()),
            );
            append_log(&ui, "Playlist artwork refreshed.");
        }
    });
}

async fn backfill_missing_playlist_image_urls(
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    token: &TokenStore,
    limit: usize,
) -> bool {
    let missing = {
        let guard = state.lock().unwrap();
        if !guard.config.artwork_enabled {
            return false;
        }
        guard
            .config
            .library_snapshot
            .playlists
            .iter()
            .filter(|playlist| playlist.image_url.as_deref().unwrap_or_default().is_empty())
            .filter_map(|playlist| spotify_playlist_id_from_uri(&playlist.uri).map(|id| (playlist.uri.clone(), id)))
            .take(limit)
            .collect::<Vec<_>>()
    };

    if missing.is_empty() {
        return false;
    }

    let mut found = Vec::new();
    for (uri, playlist_id) in missing {
        if let Ok(Some(image_url)) = spotify_api.playlist_image_url(token, &playlist_id).await {
            found.push((uri, image_url));
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    if found.is_empty() {
        return false;
    }

    let mut guard = state.lock().unwrap();
    // By link, as the list may have changed meanwhile.
    for (uri, image_url) in found {
        if let Some(playlist) = guard.config.library_snapshot.playlists.iter_mut().find(|playlist| playlist.uri == uri) {
            playlist.image_url = Some(image_url);
        }
    }
    config::save_config_background(&guard.config);
    true
}

async fn enrich_visible_queue_metadata(
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    limit: usize,
) -> bool {
    let token = state.lock().unwrap().token.clone();
    let tracks_to_enrich = current_queue_tracks(&state)
        .into_iter()
        .take(limit)
        .filter(|track| track.duration_ms <= 0 || track.album.images.is_empty() || track.album.name.is_empty())
        .filter_map(|track| track_id_from_uri(&track.uri).map(|track_id| (track.uri, track_id)))
        .collect::<Vec<_>>();

    let Some(token) = token else {
        return false;
    };

    let track_ids = tracks_to_enrich
        .into_iter()
        .map(|(_, track_id)| track_id)
        .collect::<Vec<_>>();
    let enriched_tracks = spotify_api.tracks(&token, &track_ids).await.unwrap_or_default();

    if enriched_tracks.is_empty() {
        return false;
    }

    let mut guard = state.lock().unwrap();
    for enriched in enriched_tracks {
        update_track_metadata_in_state(&mut guard, enriched);
    }
    true
}

fn update_track_metadata_in_state(guard: &mut AppState, enriched: FullTrack) {
    for track in guard.queue_override_tracks.iter_mut() {
        if track.uri == enriched.uri {
            *track = enriched.clone();
        }
    }

    for tracks in guard.playlist_track_cache.values_mut() {
        for track in tracks.iter_mut() {
            if track.uri == enriched.uri {
                *track = enriched.clone();
            }
        }
    }
}

async fn warm_queue_album_art_cache(state: Arc<Mutex<AppState>>, limit: usize) -> bool {
    let image_urls = {
        current_queue_tracks(&state)
            .into_iter()
            .take(limit)
            .filter_map(|track| row_art_url(&track).map(str::to_string))
            .filter(|url| cached_album_art_path(url).is_none())
            .collect::<Vec<_>>()
    };

    let mut downloads = tokio::task::JoinSet::new();
    for url in image_urls {
        downloads.spawn(async move { load_album_art(&url).await.is_ok() });
    }
    let mut warmed_any = false;
    while let Some(result) = downloads.join_next().await {
        warmed_any |= result.unwrap_or(false);
    }
    warmed_any
}

fn clear_queue_rows(ui: &AppWindow) {
    if ui.get_queue_busy() {
        return;
    }
    ui.set_queue_titles(ModelRc::from(Rc::new(VecModel::<SharedString>::from(Vec::new()))));
    ui.set_queue_artists(ModelRc::from(Rc::new(VecModel::<SharedString>::from(Vec::new()))));
    ui.set_queue_images(ModelRc::from(Rc::new(VecModel::<slint::Image>::from(Vec::new()))));
    ui.set_queue_reorder_enabled(false);
    QUEUE_PAINTED.with(|painted| *painted.borrow_mut() = None);
}

fn set_now_playing_from_track(
    ui: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    track: &FullTrack,
    update: NowPlayingUpdate,
) {
    let visible_index = {
        let mut guard = state.lock().unwrap();
        let visible_index = guard
            .playlist_track_cache
            .get(&guard.selected_playlist_uri)
            .and_then(|tracks| tracks.iter().position(|candidate| candidate.uri == track.uri));
        guard.current_track_uri = Some(track.uri.clone());
        guard.now_playing = Some(track.clone());
        if guard.parked.as_ref().is_some_and(|parked| parked.track.uri != track.uri) {
            guard.parked = None;
        }
        guard.missing_playback_since = None;
        guard.playback_clock.sync(0, track.duration_ms, true);
        match update {
            NowPlayingUpdate::Optimistic => {
                guard.optimistic_track_uri = Some(track.uri.clone());
                guard.optimistic_track_until = Some(Instant::now() + OPTIMISTIC_TRACK_TTL);
            }
            NowPlayingUpdate::Authoritative => {
                guard.optimistic_track_uri = None;
                guard.optimistic_track_until = None;
                guard.latest_local_track_uri = Some(track.uri.clone());
                if let Some(position) = guard
                    .queue_override_tracks
                    .iter()
                    .position(|queued| queued.uri == track.uri)
                {
                    guard.queue_override_tracks.drain(..=position);
                }
            }
            NowPlayingUpdate::Enrichment => {}
        }
        visible_index
    };
    let _ = visible_index;
    ui.set_current_track_uri(track.uri.clone().into());
    if matches!(update, NowPlayingUpdate::Authoritative) {
        // Authoritative updates come from the local librespot player.
        ui.set_playing_device_name("".into());
        ui.set_active_device_name(ui.get_local_device_name());
        ui.set_device_icon(load_icon("computer.svg"));
    }
    ui.set_is_playing(true);
    ui.set_connection_status("Connected - Playing".into());
    ui.set_track_title(track.name.clone().into());
    show_track_artists(ui, &track.artists);

    ui.set_track_album(track.album.name.clone().into());
    ui.set_track_release_date(track.album.release_date.clone().unwrap_or_default().into());
    ui.set_progress_ms(0);
    ui.set_progress_max_ms(track.duration_ms.max(1));
    ui.set_elapsed("0:00".into());
    ui.set_duration(format_time(track.duration_ms).into());
    set_album_art_if_changed(
        ui,
        state,
        track
            .album
            .images
            .first()
            .and_then(|img| cached_album_art_path(&img.url))
    );
}

fn reset_album_art_to_logo(ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    {
        let mut guard = state.lock().unwrap();
        guard.current_album_art_url = None;
        guard.current_album_art_path = None;
        guard.last_album_art_ui_path = None;
    }
    ui.set_album_art(slint::Image::default());
}

fn build_track_queue_from_playlist(
    state: &Arc<Mutex<AppState>>,
    playlist_uri: &str,
    start_index: usize,
) -> Vec<String> {
    state
        .lock()
        .unwrap()
        .playlist_track_cache
        .get(playlist_uri)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .skip(start_index)
        .map(|track| track.uri)
        .collect()
}

async fn build_startup_queue_from_playlist(
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    token: &TokenStore,
    playlist_uri: &str,
    track_uri: &str,
) -> Option<Vec<String>> {
    if playlist_uri == SEARCH_RESULTS_URI {
        return None;
    }

    let cached_tracks = state
        .lock()
        .unwrap()
        .playlist_track_cache
        .get(playlist_uri)
        .cloned()
        .unwrap_or_default();
    if let Some(start_index) = cached_tracks.iter().position(|track| track.uri == track_uri) {
        return Some(
            cached_tracks
                .into_iter()
                .skip(start_index)
                .map(|track| track.uri)
                .collect(),
        );
    }

    let session = current_session(state);
    let fetched_tracks = fetch_list_songs(state, spotify_api, token, session, playlist_uri).await.ok()?;
    let start_index = fetched_tracks.iter().position(|track| track.uri == track_uri)?;
    let queue = fetched_tracks
        .iter()
        .skip(start_index)
        .map(|track| track.uri.clone())
        .collect::<Vec<_>>();

    let mut guard = state.lock().unwrap();
    if guard.selected_playlist_uri == playlist_uri {
        guard.track_uris = fetched_tracks.into_iter().map(|track| track.uri).collect();
    }

    Some(queue)
}

fn current_queue_tracks(state: &Arc<Mutex<AppState>>) -> Vec<FullTrack> {
    let guard = state.lock().unwrap();
    if guard.queue_override_active {
        return guard.queue_override_tracks.clone();
    }
    if guard.spotify_queue_current_uri.as_deref() == guard.current_track_uri.as_deref() {
        if let Some(tracks) = guard.spotify_queue_tracks.as_ref() {
            return tracks.iter().take(MAX_LOCAL_QUEUE_TRACKS).cloned().collect();
        }
    }
    if guard.current_track_uri.is_none() {
        return Vec::new();
    }
    if let Some(tracks) = guard.spotify_queue_tracks.as_ref().filter(|_| guard.spotify_queue_current_uri.is_none()) {
        return tracks.iter().take(MAX_LOCAL_QUEUE_TRACKS).cloned().collect();
    }
    let Some(current_track_uri) = guard.current_track_uri.as_ref() else {
        return Vec::new();
    };
    // The song has moved on and Spotify hasn't given the new queue yet. The new
    // song is usually in the old queue, so what follows it is what's coming up.
    let last_queue = guard.spotify_queue_tracks.as_deref().unwrap_or_default();
    if let Some(start) = queue_tail_start(last_queue.iter().map(|track| track.uri.as_str()), current_track_uri) {
        return last_queue.iter().skip(start).take(MAX_LOCAL_QUEUE_TRACKS).cloned().collect();
    }
    if let Some(tracks) = guard.playlist_track_cache.get(&guard.active_queue_playlist_uri) {
        if let Some(start) = queue_tail_start(tracks.iter().map(|track| track.uri.as_str()), current_track_uri) {
            return tracks.iter().skip(start).take(MAX_LOCAL_QUEUE_TRACKS).cloned().collect();
        }
    }
    // Something else started. Hold the old list for the moment the refresh takes
    // rather than flashing "Nothing queued", but never leave it up for long.
    let refresh_due = guard.fast_playback_refresh_until.is_some_and(|until| Instant::now() < until);
    if refresh_due {
        return last_queue.iter().take(MAX_LOCAL_QUEUE_TRACKS).cloned().collect();
    }
    Vec::new()
}

/// Where the songs after `current` start in a queue, if it's in there at all.
fn queue_tail_start<'a>(mut queue: impl Iterator<Item = &'a str>, current: &str) -> Option<usize> {
    queue.position(|uri| uri == current).map(|at| at + 1)
}

fn reserve_spotify_queue_refresh(state: &Arc<Mutex<AppState>>) -> bool {
    if spotify::rate_limit_wait().is_some() {
        return false;
    }
    let mut guard = state.lock().unwrap();
    // A song change shouldn't wait out the usual gap for its new queue.
    let queue_is_behind = guard.spotify_queue_current_uri != guard.current_track_uri
        && guard.fast_playback_refresh_until.is_some_and(|until| Instant::now() < until);
    let minimum_interval = if queue_is_behind {
        Duration::from_secs(1)
    } else if guard.optimistic_track_uri.is_some() {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(5)
    };
    if guard
        .last_spotify_queue_refresh
        .is_some_and(|last| last.elapsed() < minimum_interval)
    {
        return false;
    }
    // Reserve the slot before starting I/O. A rejected/mismatched response must
    // not cause every fast playback poll to issue another queue request.
    guard.last_spotify_queue_refresh = Some(Instant::now());
    true
}

fn update_spotify_queue(
    state: &Arc<Mutex<AppState>>,
    session: u64,
    playback: Option<&PlaybackState>,
    queue: crate::models::QueueResponse,
) {
    let playback_uri = playback.and_then(|p| p.item.as_ref()).map(|track| track.uri.as_str());
    let queue_uri = queue.currently_playing.as_ref().map(|track| track.uri.as_str());
    let mut guard = state.lock().unwrap();
    // Asked for the account that has since signed out.
    if ensure_same_session(&guard, session).is_err() {
        return;
    }
    // Next up is where it was left, until something plays. Music on another device takes
    // over, queue and all, even if its queue arrives before its song has cleared the park.
    if guard.parked.is_some() && playback_uri.is_none() {
        return;
    }
    // A queue for a session the local player isn't playing must not replace ours.
    if guard.local_is_active()
        && guard.latest_local_track_uri.is_some()
        && guard.latest_local_track_uri.as_deref() != queue_uri
    {
        return;
    }
    if playback_uri != queue_uri
        || guard
            .optimistic_track_uri
            .as_deref()
            .is_some_and(|expected| Some(expected) != playback_uri)
    {
        return;
    }
    let just_edited = guard.queue_edited_at.is_some_and(|at| at.elapsed() < QUEUE_EDIT_SETTLE);
    if spotify_queue_replaces_ours(&queue.queue, &guard.queue_override_tracks, just_edited) {
        guard.queue_override_active = false;
    }
    guard.spotify_queue_current_uri = queue_uri.map(str::to_string);
    guard.spotify_queue_tracks = Some(queue.queue);
    guard.last_spotify_queue_refresh = Some(Instant::now());
}

fn starts_with_tracks(tracks: &[FullTrack], start: &[FullTrack]) -> bool {
    tracks.len() >= start.len() && tracks.iter().zip(start).all(|(track, other)| track.uri == other.uri)
}

/// Spotify's list is the truth, unless it can't have heard of an edit yet. It only reports the
/// next few songs, so while they match, the longer list this app holds stays on screen.
fn spotify_queue_replaces_ours(spotify: &[FullTrack], ours: &[FullTrack], just_edited: bool) -> bool {
    !just_edited && (spotify.is_empty() || !starts_with_tracks(ours, spotify))
}

/// The list an edit starts from, given the rows on screen. Spotify only reports the next
/// few songs, so more follows them: the list this app holds if the rows are the start of it,
/// otherwise the rest of the playlist. Without that, the edit would end the music after them.
/// In shuffle the rows are enough: the player keeps its own shuffled order behind them, and the
/// playlist's order would replace it.
fn queue_to_edit(shown: &[FullTrack], held: &[FullTrack], playlist: &[FullTrack], shuffled: bool) -> Vec<FullTrack> {
    if starts_with_tracks(held, shown) {
        return held.to_vec();
    }
    if shuffled {
        return shown.to_vec();
    }
    let rest = shown
        .last()
        .and_then(|last| playlist.iter().position(|track| track.uri == last.uri))
        .map_or(&[][..], |at| &playlist[at + 1..]);
    shown.iter().chain(rest).take(MAX_LOCAL_QUEUE_TRACKS).cloned().collect()
}

/// Puts a dragged song where it was dropped. A song can end during a drag, which takes the
/// top rows off the real list, so if the list has moved on from the rows that were dragged
/// the song lands below the same neighbour instead.
fn move_dragged_track(tracks: &mut Vec<FullTrack>, painted: &[String], from: usize, to: usize) {
    let Some(moved) = painted.get(from) else {
        return;
    };
    let unchanged = tracks.len() >= painted.len() && tracks.iter().zip(painted).all(|(track, uri)| &track.uri == uri);
    if unchanged {
        let track = tracks.remove(from);
        tracks.insert(to.min(painted.len() - 1), track);
        return;
    }
    let Some(at) = tracks.iter().position(|track| &track.uri == moved) else {
        return;
    };
    let track = tracks.remove(at);
    let mut others = painted.to_vec();
    others.remove(from);
    let above = others[..to.min(others.len())]
        .iter()
        .rev()
        .find_map(|uri| tracks.iter().position(|track| &track.uri == uri));
    tracks.insert(above.map_or(0, |position| position + 1), track);
}

fn remove_painted_track(tracks: &mut Vec<FullTrack>, painted: &[String], index: usize) {
    let Some(uri) = painted.get(index) else {
        return;
    };
    let at = if tracks.get(index).is_some_and(|track| &track.uri == uri) {
        Some(index)
    } else {
        tracks.iter().position(|track| &track.uri == uri)
    };
    if let Some(at) = at {
        tracks.remove(at);
    }
}

/// Applies a change to the queue as it stands on screen, then shows it and passes it to the
/// player, whose list changes without the song being touched.
fn edit_queue(
    ui: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    edit: impl FnOnce(&mut Vec<FullTrack>, &[String]),
) {
    let shown = current_queue_tracks(state);
    let shuffled = ui.get_shuffle_enabled();
    let (before, after, send_to_player) = {
        let mut guard = state.lock().unwrap();
        let playlist = guard.playlist_track_cache.get(&guard.active_queue_playlist_uri).map_or(&[][..], Vec::as_slice);
        let held = if guard.queue_override_active { guard.queue_override_tracks.as_slice() } else { &[] };
        let mut tracks = queue_to_edit(&shown, held, playlist, shuffled);
        let before = track_uris(&tracks);
        edit(&mut tracks, &guard.queue_painted_uris);
        let after = track_uris(&tracks);
        guard.queue_override_tracks = tracks;
        guard.queue_override_active = true;
        guard.queue_edited_at = Some(Instant::now());
        // Only while this PC is playing: another device's session isn't ours to change.
        (before, after, guard.local_is_active())
    };
    repaint_queue(ui, state);
    note_left_off(ui, state);
    if send_to_player && run_local_player_command(Arc::clone(state), |engine| engine.set_upcoming(before, after)).is_err() {
        show_toast(ui, "Couldn't update the queue", true);
    }
}

fn track_uris(tracks: &[FullTrack]) -> Vec<String> {
    tracks.iter().map(|track| track.uri.clone()).collect()
}

fn repaint_queue(ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    ui.set_queue_summary(queue_summary_text(current_queue_tracks(state).len()).into());
    let editable = {
        let guard = state.lock().unwrap();
        guard.local_is_selected()
    };
    apply_queue_rows(ui, state, editable);
}

fn device_type_badge(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "computer" => "PC",
        "speaker" => "SPK",
        "smartphone" => "PHN",
        "castvideo" | "tv" | "avr" => "TV",
        _ => "DEV",
    }
}

fn device_type_icon(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "computer" => "computer.svg",
        "speaker" => "speaker.svg",
        "smartphone" => "phone.svg",
        "castvideo" | "tv" | "avr" => "tv.svg",
        _ => "device.svg",
    }
}

fn track_id_from_uri(track_uri: &str) -> Option<String> {
    track_uri
        .rsplit(':')
        .next()
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
}

fn spotify_playlist_id_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("spotify:playlist:")
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
}

fn should_refresh_library_at_startup(state: &Arc<Mutex<AppState>>) -> bool {
    let guard = state.lock().unwrap();
    let snapshot = &guard.config.library_snapshot;
    let now = config::now_epoch();
    let cache_empty = snapshot.playlists.is_empty() && snapshot.liked_songs.is_empty();
    let stale = snapshot.fetched_at_epoch == 0 || now.saturating_sub(snapshot.fetched_at_epoch) > 21_600;
    let missing_liked_durations = snapshot
        .liked_songs
        .iter()
        .any(|track| track.duration_ms <= 0);
    // Older Spotlight builds rewrite the cache without artwork and album names.
    let missing_liked_details = !snapshot.liked_songs.is_empty()
        && snapshot.liked_songs.iter().all(|track| track.image_url.is_none());
    let needs_repair = missing_liked_durations || missing_liked_details;
    // Spotlight 1.1.0 kept no totals, so whether there's more to load isn't known yet.
    let missing_totals = (snapshot.liked_songs_total == 0 && snapshot.liked_songs.len() >= spotify::SONGS_PER_PART)
        || (snapshot.playlists_total == 0 && snapshot.playlists.len() >= spotify::PLAYLISTS_PER_PART);
    let rate_limited = now < guard.next_allowed_library_refresh_at_epoch && !needs_repair;
    (cache_empty || stale || needs_repair || missing_totals) && !rate_limited
}

/// The session now, for work done straight away rather than after a wait.
fn current_session(state: &Mutex<AppState>) -> u64 {
    state.lock().unwrap().session
}

fn remember_last_track_uri(state: &Arc<Mutex<AppState>>, session: u64, track_uri: &str) {
    let mut guard = state.lock().unwrap();
    if ensure_same_session(&guard, session).is_err() {
        return;
    }
    if guard.config.startup_snapshot.last_track_uri.as_deref() == Some(track_uri) {
        return;
    }
    guard.config.startup_snapshot.last_track_uri = Some(track_uri.to_string());
    guard.config.startup_snapshot.last_played_at_epoch = config::now_epoch();
    config::save_config_background(&guard.config);
}

/// Plays a song from search ahead of what's playing, which then carries on where it was: the
/// song goes to the front of the queue and is skipped to. Returns false when nothing is
/// playing, as there's nothing to go back to.
fn play_search_result_next(
    ui: &AppWindow,
    runtime: &Arc<Runtime>,
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    index: usize,
) -> bool {
    let was_paused = !ui.get_is_playing();
    let (track, has_song, here, elsewhere) = {
        let guard = state.lock().unwrap();
        let track = guard.playlist_track_cache.get(SEARCH_RESULTS_URI).and_then(|tracks| tracks.get(index)).cloned();
        // A song only on show, like the last one from before a restart, isn't playing or
        // paused part way, so there's nothing to go back to.
        let has_song = guard.current_track_uri.is_some() && (!was_paused || guard.playback_clock.displayed_progress_ms() > 0);
        (track, has_song, guard.local_is_active(), guard.active_device_id.is_some())
    };
    let Some(track) = track else { return false };
    if !has_song {
        return false;
    }
    if here {
        edit_queue(ui, state, |tracks, _| tracks.insert(0, track));
        if was_paused {
            // Skipping keeps a paused player paused, and a song you pick should play.
            audio_output::set_paused(false);
            ui.set_is_playing(true);
            ui.set_connection_status("Connected - Playing".into());
            let mut guard = state.lock().unwrap();
            guard.optimistic_is_playing = Some(true);
            guard.optimistic_playback_until = Some(Instant::now() + Duration::from_secs(1));
        }
        skip_here(&ui.as_weak(), runtime, state, spotify_api, LocalPlayerAction::NextAndPlay);
        return true;
    }
    if !elsewhere {
        return false;
    }
    // Another device's queue can only be added to, after any songs queued by hand there, and
    // Spotify takes a moment to show it. Skipping before this song is next would play another.
    let track_uri = track.uri;
    let ui_weak = ui.as_weak();
    spawn_device_command(ui.as_weak(), Arc::clone(runtime), Arc::clone(state), spotify_api.clone(), move |api, token, device| async move {
        api.add_to_queue(&token, device.as_deref(), &track_uri).await?;
        let mut queued_behind_others = false;
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Ok(queue) = api.queue(&token).await else { continue };
            if queue.queue.first().is_some_and(|next| next.uri == track_uri) {
                api.next(&token, device.as_deref()).await?;
                if was_paused {
                    api.resume(&token, device.as_deref()).await?;
                }
                return Ok(());
            }
            if queue.queue.iter().any(|queued| queued.uri == track_uri) {
                queued_behind_others = true;
                break;
            }
        }
        let message = if queued_behind_others { "Added to the queue, after the songs already in it" } else { "Added to the queue" };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                show_toast(&ui, message, false);
            }
        });
        Ok(())
    });
    true
}

/// Skips to the next song on this PC. What's on screen moves at once; the player is told after.
fn skip_here(
    ui_weak: &slint::Weak<AppWindow>,
    runtime: &Arc<Runtime>,
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    action: LocalPlayerAction,
) {
    audio_output::begin_skip();
    begin_fast_playback_refresh(state, Duration::from_secs(3));
    if let Some(ui) = ui_weak.upgrade() {
        optimistic_advance_queue(&ui, state);
    }
    let ui_weak = ui_weak.clone();
    let state = Arc::clone(state);
    let spotify_api = spotify_api.clone();
    runtime.spawn(async move {
        let result = run_local_player_action_with_recovery(Arc::clone(&state), ui_weak.clone(), spotify_api.clone(), action).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = refresh_playback_only(ui_weak.clone(), Arc::clone(&state), spotify_api).await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Err(error) = result {
                    append_log(&ui, &format!("Playback command failed: {error}"));
                }
            }
        });
    });
}

/// Plays the list kept in the cache under `list_uri`, starting with the song at `index`, with
/// the songs after it queued when autoplay is on. `remember` keeps the list as the one to come
/// back to on the next start. A song from search plays alone: nobody wants all the results.
fn play_from_list(
    ui_weak: &slint::Weak<AppWindow>,
    runtime: &Arc<Runtime>,
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
    list_uri: String,
    index: usize,
    remember: bool,
) {
    let (selected_track, track_start_generation) = {
        let mut guard = state.lock().unwrap();
        let selected_track = guard
            .playlist_track_cache
            .get(&list_uri)
            .and_then(|tracks| tracks.get(index))
            .cloned();
        if selected_track.is_some() {
            guard.track_start_generation = guard.track_start_generation.wrapping_add(1);
        }
        (selected_track, guard.track_start_generation)
    };
    if let Some(selected_track) = selected_track {
        // The displayed metadata and command URI must come from the
        // same immutable row snapshot. `track_uris` can be replaced
        // independently by an async library refresh.
        let track_uri = selected_track.uri.clone();
        remember_last_track_uri(state, current_session(state), &track_uri);
        if remember {
            remember_last_playlist_uri(state, current_session(state), &list_uri);
        }
        let rest_follows = list_uri != SEARCH_RESULTS_URI && state.lock().unwrap().config.autoplay_similar;
        {
            let mut guard = state.lock().unwrap();
            let override_tracks = if rest_follows {
                guard
                    .playlist_track_cache
                    .get(&list_uri)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .skip(index + 1)
                    .take(MAX_LOCAL_QUEUE_TRACKS)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            guard.active_queue_playlist_uri = list_uri.clone();
            guard.queue_override_tracks = override_tracks;
            guard.queue_override_active = true;
        }
        if let Some(ui) = ui_weak.upgrade() {
            set_now_playing_from_track(&ui, &state, &selected_track, NowPlayingUpdate::Optimistic);
            ui.set_queue_summary(queue_summary_text(current_queue_tracks(&state).len()).into());
            let reorder_enabled = {
                let guard = state.lock().unwrap();
                guard.local_is_selected()
            };
            apply_queue_rows(&ui, &state, reorder_enabled);
        }
        {
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            runtime.spawn(async move {
                warm_queue_art_and_repaint(ui_weak, state, spotify_api).await;
            });
        }
        {
            let ui_weak = ui_weak.clone();
            let state = Arc::clone(&state);
            let spotify_api = spotify_api.clone();
            let selected_track = selected_track.clone();
            let track_uri = track_uri.clone();
            runtime.spawn(async move {
                let needs_enrichment = selected_track.duration_ms <= 0
                    || selected_track.album.images.is_empty()
                    || selected_track.album.name.is_empty();
                if !needs_enrichment {
                    return;
                }
                let token = {
                    let guard = state.lock().unwrap();
                    guard.token.clone()
                };
                let Some(token) = token else {
                    return;
                };
                let Some(track_id) = track_id_from_uri(&track_uri) else {
                    return;
                };
                let Ok(track) = spotify_api.track(&token, &track_id).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        let should_apply = {
                            let guard = state.lock().unwrap();
                            guard.optimistic_track_uri.as_deref() == Some(track.uri.as_str())
                        };
                        if should_apply {
                            set_now_playing_from_track(&ui, &state, &track, NowPlayingUpdate::Enrichment);
                        }
                    }
                });
            });
        }
        if list_uri != LIKED_SONGS_PLAYLIST_URI {
            let mut queue_tracks = if rest_follows {
                build_track_queue_from_playlist(&state, &list_uri, index)
            } else {
                Vec::new()
            };
            if queue_tracks.is_empty() {
                queue_tracks.push(track_uri.clone());
            }
            spawn_track_start_command(
                ui_weak.clone(),
                Arc::clone(&runtime),
                Arc::clone(&state),
                spotify_api.clone(),
                track_uri.clone(),
                queue_tracks,
                track_start_generation,
            );
        } else {
            let autoplay = state.lock().unwrap().config.autoplay_similar;
            let mut queue_tracks = if autoplay {
                state
                    .lock()
                    .unwrap()
                    .playlist_track_cache
                    .get(LIKED_SONGS_PLAYLIST_URI)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .skip(index)
                    .map(|track| track.uri)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if queue_tracks.is_empty() {
                queue_tracks.push(track_uri.clone());
            }
            spawn_track_start_command(
                ui_weak.clone(),
                Arc::clone(&runtime),
                Arc::clone(&state),
                spotify_api.clone(),
                track_uri.clone(),
                queue_tracks,
                track_start_generation,
            );
        }
    }
}

/// The longest crossfade Settings offers.
const MAX_CROSSFADE_SECONDS: u32 = 12;

/// How long the player fades one song into the next. Zero turns it off.
fn set_crossfade(seconds: u32) {
    librespot_playback::player::set_crossfade(Duration::from_secs(seconds.min(MAX_CROSSFADE_SECONDS).into()));
}

fn remember_last_playlist_uri(state: &Arc<Mutex<AppState>>, session: u64, playlist_uri: &str) {
    let mut guard = state.lock().unwrap();
    if ensure_same_session(&guard, session).is_err() {
        return;
    }
    if guard.config.startup_snapshot.last_playlist_uri.as_deref() == Some(playlist_uri) {
        return;
    }
    guard.config.startup_snapshot.last_playlist_uri = Some(playlist_uri.to_string());
    guard.config.startup_snapshot.last_played_at_epoch = config::now_epoch();
    config::save_config_background(&guard.config);
}

/// A library playlist's songs, from the cache when it has them in full, otherwise fetched and
/// kept there. Index 0 is Liked Songs. Returns the playlist's link with them.
async fn library_playlist_songs(
    playlist_index: usize,
    state: &Arc<Mutex<AppState>>,
    spotify_api: &spotify::SpotifyApi,
) -> Result<(String, Vec<FullTrack>)> {
    let session = state.lock().unwrap().session;
    let playlist_uri = if playlist_index == 0 {
        LIKED_SONGS_PLAYLIST_URI.to_string()
    } else {
        state
            .lock()
            .unwrap()
            .playlist_uris
            .get(playlist_index.saturating_sub(1))
            .cloned()
            .ok_or_else(|| anyhow!("playlist selection out of range"))?
    };
    let (cached_tracks, latest_version) = {
        let guard = state.lock().unwrap();
        let latest_version = guard
            .config
            .library_snapshot
            .playlists
            .iter()
            .find(|playlist| playlist.uri == playlist_uri)
            .and_then(|playlist| playlist.snapshot_id.clone());
        // Edited since its songs were fetched, maybe on another device.
        let changed = latest_version
            .as_ref()
            .is_some_and(|latest| guard.playlist_cache_versions.get(&playlist_uri) != Some(latest));
        let cached = guard.playlist_track_cache.get(&playlist_uri).filter(|_| !changed).cloned();
        (cached, latest_version)
    };

    let cached_tracks_are_full = cached_tracks
        .as_ref()
        .is_some_and(|tracks| tracks.iter().any(|track| track.duration_ms > 0 && !track.album.images.is_empty()));

    let tracks = if let Some(cached_tracks) = cached_tracks.filter(|_| cached_tracks_are_full) {
        cached_tracks
    } else {
        let token = current_token(state).await?;
        let tracks = fetch_list_songs(state, spotify_api, &token, session, &playlist_uri).await?;
        if playlist_uri != LIKED_SONGS_PLAYLIST_URI {
            let mut guard = state.lock().unwrap();
            ensure_same_session(&guard, session)?;
            match latest_version {
                Some(version) => guard.playlist_cache_versions.insert(playlist_uri.clone(), version),
                None => guard.playlist_cache_versions.remove(&playlist_uri),
            };
        }
        tracks
    };
    Ok((playlist_uri, tracks))
}

async fn populate_tracks_for_playlist(
    playlist_index: usize,
    generation: u64,
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) -> Result<()> {
    let (playlist_uri, tracks) = library_playlist_songs(playlist_index, &state, &spotify_api).await?;
    let playlist_label = if playlist_uri == LIKED_SONGS_PLAYLIST_URI {
        "Liked Songs".to_string()
    } else {
        state
            .lock()
            .unwrap()
            .config
            .library_snapshot
            .playlists
            .iter()
            .find(|item| item.uri == playlist_uri)
            .map(|item| item.name.clone())
            .unwrap_or_else(|| "playlist".to_string())
    };

    let track_uris = tracks.iter().map(|track| track.uri.clone()).collect::<Vec<_>>();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            if state.lock().unwrap().track_view_generation != generation {
                return;
            }
            ui.set_track_view_loading(false);
            {
                // Browsing a playlist must not touch the queue of whatever is
                // currently playing; only starting playback replaces it.
                let mut guard = state.lock().unwrap();
                guard.track_uris = track_uris;
                guard.selected_playlist_uri = playlist_uri.clone();
            }
            show_tracks(&ui, tracks);
            show_more_to_load(&ui, &state);
            ui.set_track_view_title(playlist_label.clone().into());
            ui.set_selected_liked_song_index(-1);
        }
    });

    Ok(())
}

fn spawn_device_command<F, Fut>(
    ui_weak: slint::Weak<AppWindow>,
    runtime: Arc<Runtime>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    f: F,
)
where
    F: FnOnce(spotify::SpotifyApi, TokenStore, Option<String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    runtime.spawn(async move {
        let token = match current_token(&state).await {
            Ok(token) => token,
            Err(error) => {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        append_log(&ui, &format!("Playback command failed: {error}"));
                    }
                });
                return;
            }
        };
        let device = state.lock().unwrap().device_id.clone();

        let result = f(spotify_api.clone(), token, device).await;
        if result.is_ok() {
            let _ = refresh_playback_only(ui_weak.clone(), Arc::clone(&state), spotify_api).await;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                match result {
                    Ok(()) => {}
                    Err(error) => append_log(&ui, &format!("Playback command failed: {error}")),
                }
            }
        });
    });
}

fn spawn_track_start_command(
    ui_weak: slint::Weak<AppWindow>,
    runtime: Arc<Runtime>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    expected_track_uri: String,
    track_uris: Vec<String>,
    generation: u64,
) {
    runtime.spawn(async move {
        let start_lock = state.lock().unwrap().track_start_lock.clone();
        let result = {
            let _start_guard = start_lock.lock().await;
            if state.lock().unwrap().track_start_generation != generation {
                return;
            }
            start_track_uris_with_recovery(
                ui_weak.clone(),
                Arc::clone(&state),
                spotify_api.clone(),
                track_uris,
            )
            .await
        };
        if state.lock().unwrap().track_start_generation != generation {
            return;
        }
        if result.is_ok() {
            let mut matched_confirmed_track = false;
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(400)).await;
                if state.lock().unwrap().track_start_generation != generation {
                    return;
                }
                let _ = refresh_playback_only(
                    ui_weak.clone(),
                    Arc::clone(&state),
                    spotify_api.clone(),
                ).await;
                let matched = {
                    let guard = state.lock().unwrap();
                    guard.current_track_uri.as_deref() == Some(expected_track_uri.as_str())
                        && guard.optimistic_track_uri.is_none()
                        && guard.playback_clock.duration_ms > 0
                };
                if matched {
                    matched_confirmed_track = true;
                    break;
                }
            }
            if !matched_confirmed_track {
                {
                    let mut guard = state.lock().unwrap();
                    if guard.track_start_generation == generation
                        && guard.optimistic_track_uri.as_deref() == Some(expected_track_uri.as_str())
                    {
                        guard.optimistic_track_uri = None;
                        guard.optimistic_track_until = None;
                    }
                }
                let _ = refresh_playback_only(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await;
            }
        } else {
            {
                let mut guard = state.lock().unwrap();
                if guard.track_start_generation == generation
                    && guard.optimistic_track_uri.as_deref() == Some(expected_track_uri.as_str())
                {
                    guard.optimistic_track_uri = None;
                    guard.optimistic_track_until = None;
                }
            }
            let _ = refresh_playback_only(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Err(error) = result {
                    append_log(&ui, &format!("Playback command failed: {error}"));
                }
            }
        });
    });
}

async fn start_track_uris_with_recovery(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    track_uris: Vec<String>,
) -> Result<()> {
    if track_uris.is_empty() {
        return Err(anyhow!("no track selected"));
    }

    let result = start_track_uris_once(
        ui_weak.clone(),
        Arc::clone(&state),
        spotify_api.clone(),
        &track_uris,
    )
    .await;
    if result.is_ok() || !is_stale_spotify_device_error(&result) {
        return result;
    }
    let targets_other_device = {
        let guard = state.lock().unwrap();
        pick_start_device(guard.active_device_id.as_ref(), guard.preferred_device_id.as_ref(), &guard.available_device_ids)
            .is_some_and(|id| Some(&id) != guard.local_device_id.as_ref())
    };
    if targets_other_device {
        return result;
    }

    clear_local_playback_engine(&state);
    ensure_local_playback(Arc::clone(&state), ui_weak.clone()).await?;
    wait_for_local_device_refresh(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await;
    start_track_uris_once(ui_weak, state, spotify_api, &track_uris).await
}

async fn start_track_uris_once(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    track_uris: &[String],
) -> Result<()> {
    let device_id = resolve_start_device(ui_weak, Arc::clone(&state), spotify_api.clone()).await?;
    let token = current_token(&state).await?;

    if track_uris.len() == 1 {
        spotify_api.play_track(&token, &device_id, &track_uris[0]).await
    } else {
        spotify_api.play_tracks(&token, &device_id, track_uris).await
    }
}

/// The device new music goes to: whatever is current, else the last one in use.
/// `None` means there has never been one, so this PC is the only choice.
fn pick_start_device(active: Option<&String>, last_used: Option<&String>, available: &[String]) -> Option<String> {
    active
        .or_else(|| last_used.filter(|id| available.contains(id)))
        .cloned()
}

/// Songs to send when a queue row is clicked: that row, the rows after it, then
/// whatever follows the last row in the list the queue came from (the panel only
/// shows part of it).
fn uris_from_queue_click(queue: &[String], index: usize, source: &[String]) -> Vec<String> {
    let Some(from_queue) = queue.get(index..).filter(|rows| !rows.is_empty()) else {
        return Vec::new();
    };
    let mut uris = from_queue.to_vec();
    if let Some(after_last) = queue
        .last()
        .and_then(|last| source.iter().position(|uri| uri == last))
    {
        uris.extend(source[after_last + 1..].iter().cloned());
    }
    uris
}

/// Only clicking a device changes where music plays: every way of starting a
/// song goes to the current device. Starting the local player is only done
/// here, i.e. on an explicit play action, and only if this PC is the target.
async fn resolve_start_device(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) -> Result<String> {
    let (mut target, local) = {
        let guard = state.lock().unwrap();
        (
            pick_start_device(guard.active_device_id.as_ref(), guard.preferred_device_id.as_ref(), &guard.available_device_ids),
            guard.local_device_id.clone(),
        )
    };
    if target.is_none() {
        // Not refreshed yet, e.g. just after launch: ask rather than take over.
        if let Ok(token) = current_token(&state).await {
            if let Ok(Some(playback)) = spotify_api.current_playback(&token).await {
                target = playback.device.filter(|device| device.is_active).and_then(|device| device.id);
            }
        }
    }
    if let Some(device_id) = target.filter(|id| Some(id) != local.as_ref()) {
        return Ok(device_id);
    }

    if !local_player_is_running(&state) {
        clear_local_playback_engine(&state);
        ensure_local_playback(Arc::clone(&state), ui_weak.clone()).await?;
    }
    if state.lock().unwrap().local_device_id.is_none() {
        // Freshly started players take a moment to appear in Spotify's list.
        for _ in 0..5 {
            wait_for_local_device_refresh(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await;
            if state.lock().unwrap().local_device_id.is_some() {
                break;
            }
        }
    }
    state
        .lock()
        .unwrap()
        .local_device_id
        .clone()
        .ok_or_else(|| anyhow!("no local Spotify device ready"))
}

fn spawn_start_command<F, Fut>(
    ui_weak: slint::Weak<AppWindow>,
    runtime: Arc<Runtime>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    f: F,
) where
    F: FnOnce(spotify::SpotifyApi, TokenStore, String) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    runtime.spawn(async move {
        let result = async {
            let device_id = resolve_start_device(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await?;
            let token = current_token(&state).await?;
            f(spotify_api.clone(), token, device_id).await
        }
        .await;
        if result.is_ok() {
            begin_fast_playback_refresh(&state, Duration::from_secs(4));
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = refresh_playback_only(ui_weak.clone(), Arc::clone(&state), spotify_api).await;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Err(error) = result {
                    append_log(&ui, &format!("Playback command failed: {error}"));
                }
            }
        });
    });
}

fn is_stale_spotify_device_error(result: &Result<()>) -> bool {
    result
        .as_ref()
        .err()
        .map(|error| {
            let message = error.to_string().to_ascii_lowercase();
            message.contains("no active device")
                || message.contains("device not found")
                || message.contains("not found")
                || message.contains("restriction violated")
                || message.contains("no local spotify device ready")
        })
        .unwrap_or(false)
}

async fn refresh_until_local_device_ready(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    resume_on_local_device: bool,
) -> Result<()> {
    for _ in 0..8 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let resumed = refresh_everything(
            ui_weak.clone(),
            Arc::clone(&state),
            spotify_api.clone(),
            false,
            false,
            resume_on_local_device,
        )
        .await?;
        if state.lock().unwrap().local_device_id.is_some() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    append_log(&ui, "Local Spotify device detected after startup.");
                    if resumed {
                        append_log(&ui, "Started playback on Spotlight PC after startup.");
                    }
                }
            });
            break;
        }
    }

    Ok(())
}

async fn current_token(state: &Arc<Mutex<AppState>>) -> Result<TokenStore> {
    let read_token = || {
        let guard = state.lock().unwrap();
        guard
            .token
            .clone()
            .map(|token| (token, guard.config.client_id.clone()))
            .ok_or_else(|| anyhow!("not connected to Spotify"))
    };
    let (token, _) = read_token()?;
    if token.expires_at_epoch > config::now_epoch() {
        return Ok(token);
    }

    // Spotify rotates PKCE refresh tokens. If several tasks refreshed at once
    // they would all send the same old refresh token and all but one would
    // fail, so serialise refreshes and re-check after taking the lock.
    static REFRESH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _refresh_guard = REFRESH_LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    let (token, client_id) = read_token()?;
    if token.expires_at_epoch > config::now_epoch() {
        return Ok(token);
    }
    let refreshed = auth::refresh_token(&client_id, &token.refresh_token).await?;
    {
        let mut guard = state.lock().unwrap();
        // Signing out while the refresh was out mustn't sign back in when it returns.
        if guard.token.as_ref().map(|current| &current.refresh_token) != Some(&token.refresh_token) {
            return Err(anyhow!("not connected to Spotify"));
        }
        guard.token = Some(refreshed.clone());
        guard.config.token = Some(refreshed.clone());
        config::save_config_background(&guard.config);
    }
    Ok(refreshed)
}

fn clear_expired_optimistic_state(state: &Arc<Mutex<AppState>>) {
    let mut guard = state.lock().unwrap();
    if guard
        .optimistic_track_until
        .is_some_and(|until| Instant::now() >= until)
    {
        guard.optimistic_track_uri = None;
        guard.optimistic_track_until = None;
    }
    if guard
        .optimistic_playback_until
        .is_some_and(|until| Instant::now() >= until)
    {
        guard.optimistic_playback_until = None;
        guard.optimistic_is_playing = None;
    }
    if guard
        .optimistic_shuffle_until
        .is_some_and(|until| Instant::now() >= until)
    {
        guard.optimistic_shuffle_until = None;
        guard.optimistic_shuffle_enabled = None;
    }
    if guard
        .optimistic_repeat_until
        .is_some_and(|until| Instant::now() >= until)
    {
        guard.optimistic_repeat_until = None;
        guard.optimistic_repeat_mode = None;
    }
    if guard
        .fast_playback_refresh_until
        .is_some_and(|until| Instant::now() >= until)
    {
        guard.fast_playback_refresh_until = None;
    }
}

fn should_preserve_missing_playback(state: &Arc<Mutex<AppState>>) -> bool {
    let mut guard = state.lock().unwrap();
    if guard
        .optimistic_track_until
        .is_some_and(|until| Instant::now() < until)
        && guard.optimistic_track_uri.is_some()
    {
        return true;
    }
    if guard.current_track_uri.is_none() {
        return false;
    }
    let missing_since = *guard.missing_playback_since.get_or_insert_with(Instant::now);
    missing_since.elapsed() < MISSING_PLAYBACK_GRACE
}

fn clear_now_playing(ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let had_track = {
        let mut guard = state.lock().unwrap();
        let had_track = guard.current_track_uri.is_some();
        guard.current_track_uri = None;
        guard.now_playing = None;
        guard.parked = None;
        guard.latest_local_track_uri = None;
        guard.missing_playback_since = None;
        guard.playback_clock = PlaybackClock::default();
        guard.optimistic_track_uri = None;
        guard.optimistic_track_until = None;
        guard.current_album_art_url = None;
        guard.current_album_art_path = None;
        guard.last_album_art_ui_path = None;
        had_track
    };
    // Polls keep arriving with nothing playing, and redrawing for each one is wasted.
    if !had_track && !ui.get_is_playing() && ui.get_track_title() == "Nothing playing" && ui.get_progress_ms() == 0 {
        return;
    }
    ui.set_is_playing(false);
    ui.set_current_track_uri("".into());
    ui.set_playing_device_name("".into());
    ui.set_active_device_name("".into());
    ui.set_device_icon(load_icon("device.svg"));
    ui.set_stream_quality("".into());
    ui.set_track_title("Nothing playing".into());
    show_track_artists(ui, &[]);
    ui.set_track_album("".into());
    ui.set_track_release_date("".into());
    ui.set_progress_ms(0);
    ui.set_progress_max_ms(1);
    ui.set_elapsed("0:00".into());
    ui.set_duration("0:00".into());
    reset_album_art_to_logo(ui, state);
}

fn apply_playback_to_ui(
    ui: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    playback: Option<PlaybackState>,
    album_art: Option<std::path::PathBuf>,
) {
    let Some(playback) = playback else {
        // Nothing is playing anywhere, so where the music was left stays up, ready to play.
        if state.lock().unwrap().parked.is_some() {
            ui.set_connection_status("Connected".into());
            return;
        }
        if should_preserve_missing_playback(state) {
            ui.set_connection_status("Connected".into());
            return;
        }
        clear_now_playing(ui, state);
        ui.set_connection_status("Connected".into());
        ui.set_shuffle_enabled(false);
        ui.set_repeat_mode("off".into());

        let should_clear_queue = {
            let mut guard = state.lock().unwrap();
            if !guard.last_queue_display.is_empty()
                || guard.last_queue_summary != "Queue unavailable until playback starts"
            {
                guard.last_queue_display.clear();
                guard.last_queue_summary = "Queue unavailable until playback starts".to_string();
                true
            } else {
                false
            }
        };
        if should_clear_queue {
            ui.set_queue_summary("Queue unavailable until playback starts".into());
            clear_queue_rows(ui);
        }
        return;
    };

    let (suppress_shuffle_state, suppress_repeat_state, suppress_play_state) = {
        let guard = state.lock().unwrap();
        let suppress_shuffle = guard
            .optimistic_shuffle_until
            .is_some_and(|until| Instant::now() < until)
            && guard.optimistic_shuffle_enabled != Some(playback.shuffle_state);
        let suppress_repeat = guard
            .optimistic_repeat_until
            .is_some_and(|until| Instant::now() < until)
            && guard.optimistic_repeat_mode.as_deref() != Some(playback.repeat_state.as_str());
        let optimistic_is_playing = if guard
            .optimistic_playback_until
            .is_some_and(|until| Instant::now() < until)
        {
            guard.optimistic_is_playing
        } else {
            None
        };
        let suppress_play = optimistic_is_playing
            .map(|expected| expected != playback.is_playing)
            .unwrap_or(false);
        (suppress_shuffle, suppress_repeat, suppress_play)
    };

    if !suppress_shuffle_state {
        {
            let mut guard = state.lock().unwrap();
            guard.optimistic_shuffle_enabled = None;
            guard.optimistic_shuffle_until = None;
        }
        ui.set_shuffle_enabled(playback.shuffle_state);
    }
    if !suppress_repeat_state {
        {
            let mut guard = state.lock().unwrap();
            guard.optimistic_repeat_mode = None;
            guard.optimistic_repeat_until = None;
        }
        ui.set_repeat_mode(playback.repeat_state.clone().into());
    }

    if let Some(context_uri) = playback
        .context
        .as_ref()
        .and_then(|context| context.uri.clone())
        .filter(|uri| uri.starts_with("spotify:playlist:"))
    {
        state.lock().unwrap().active_queue_playlist_uri = context_uri;
    }

    let Some(track) = playback.item else {
        if state.lock().unwrap().parked.is_none() && !should_preserve_missing_playback(state) {
            clear_now_playing(ui, state);
        }
        return;
    };
    {
        // Something is playing, which takes over from where the music was left.
        let mut guard = state.lock().unwrap();
        guard.missing_playback_since = None;
        if guard.parked.take().is_some() {
            // The saved Next up belonged to the parked song. What's playing now brings its own,
            // so ask for that next time rather than waiting out the usual gap.
            guard.queue_override_active = false;
            guard.queue_override_tracks.clear();
            if guard.spotify_queue_current_uri.as_deref() != Some(track.uri.as_str()) {
                guard.last_spotify_queue_refresh = None;
            }
        }
    }
    if local_player_disagrees(state, &track.uri) {
        // Not transient, so don't start fast polling over it.
        return;
    }
    if should_ignore_stale_playback_track(state, &track.uri) {
        begin_fast_playback_refresh(state, Duration::from_secs(2));
        return;
    }

    let track_changed = {
        let mut guard = state.lock().unwrap();
        let changed = guard.current_track_uri.as_deref() != Some(track.uri.as_str());
        guard.optimistic_track_uri = None;
        guard.optimistic_track_until = None;
        guard.current_track_uri = Some(track.uri.clone());
        guard.now_playing = Some(track.clone());
        if !guard.local_is_active() {
            // While local, only librespot's own events set this.
            guard.latest_local_track_uri = Some(track.uri.clone());
        }
        if let Some(position) = guard
            .queue_override_tracks
            .iter()
            .position(|queued| queued.uri == track.uri)
        {
            guard.queue_override_tracks.drain(..=position);
        }
        if !suppress_play_state {
            guard
                .playback_clock
                .sync(playback.progress_ms.unwrap_or_default(), track.duration_ms, playback.is_playing);
        }
        changed
    };

    let album_art_path = album_art.or_else(|| {
        track
            .album
            .images
            .first()
            .and_then(|image| cached_album_art_path(&image.url))
    });
    set_album_art_if_changed(ui, state, album_art_path);
    ui.set_current_track_uri(track.uri.clone().into());
    {
        // Show where audio is coming out when it isn't this PC.
        let local_name = state.lock().unwrap().config.device_name.clone();
        let active_name = playback.device.as_ref().map(|device| device.name.clone()).unwrap_or_default();
        let remote_name = if active_name == local_name { String::new() } else { active_name.clone() };
        if !remote_name.is_empty() {
            // Another device's stream isn't ours to report on.
            ui.set_stream_quality("".into());
        }
        // The icon by the device's name shows what kind it is: phone, speaker, TV or computer.
        let kind = playback.device.as_ref().map(|device| device.device_type.as_str()).unwrap_or_default();
        ui.set_device_icon(load_icon(if remote_name.is_empty() { "computer.svg" } else { device_type_icon(kind) }));
        ui.set_playing_device_name(remote_name.into());
        ui.set_active_device_name(active_name.into());
    }
    ui.set_track_title(track.name.clone().into());
    show_track_artists(ui, &track.artists);

    ui.set_track_album(track.album.name.clone().into());
    ui.set_track_release_date(track.album.release_date.clone().unwrap_or_default().into());
    ui.set_progress_max_ms(track.duration_ms);
    ui.set_duration(format_time(track.duration_ms).into());

    if !suppress_play_state {
        show_progress(ui, playback.progress_ms.unwrap_or_default());
        ui.set_is_playing(playback.is_playing);
        ui.set_connection_status(if playback.is_playing {
            "Connected - Playing".into()
        } else {
            "Connected - Paused".into()
        });
    }

    if track_changed {
        begin_fast_playback_refresh(state, Duration::from_secs(3));
    }

    let queue_strings = build_queue_items(state);
    let summary = queue_summary_text(queue_strings.len());
    let should_update_queue = {
        let mut guard = state.lock().unwrap();
        if guard.last_queue_display != queue_strings || guard.last_queue_summary != summary {
            guard.last_queue_display = queue_strings;
            guard.last_queue_summary = summary.clone();
            true
        } else {
            false
        }
    };
    if should_update_queue {
        ui.set_queue_summary(summary.into());
        let reorder_enabled = {
            let guard = state.lock().unwrap();
            guard.local_is_selected()
        };
        apply_queue_rows(ui, state, reorder_enabled);
    }
}

fn set_album_art_if_changed(
    ui: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    album_art_path: Option<PathBuf>,
) {
    let should_update = {
        let mut guard = state.lock().unwrap();
        if guard.last_album_art_ui_path != album_art_path {
            guard.last_album_art_ui_path = album_art_path.clone();
            true
        } else {
            false
        }
    };
    if should_update {
        let image = album_art_path
            .as_deref()
            .map(load_full_size_art)
            .unwrap_or_default();
        let theme = AppTheme::get(ui);
        let ambient = album_art_path
            .as_ref()
            .and_then(|_| ambient_color_from_image(&image, theme.get_is_light()))
            .unwrap_or_else(|| theme.get_window_bg());
        theme.set_ambient(ambient);
        ui.set_album_art(image);
    }
}

/// Learns which account is signed in, once per sign-in, and gives it the plays recorded here
/// before it was known. A failed try is retried a minute later.
async fn learn_account(state: &Arc<Mutex<AppState>>, spotify_api: &spotify::SpotifyApi, token: &TokenStore, session: u64) {
    {
        let mut guard = state.lock().unwrap();
        if guard.config.account_id.is_some() || guard.account_asked_at.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
            return;
        }
        guard.account_asked_at = Some(Instant::now());
    }
    let Ok(account_id) = spotify_api.account_id(token).await else { return };
    {
        let mut guard = state.lock().unwrap();
        if ensure_same_session(&guard, session).is_err() || guard.config.account_id.is_some() {
            return;
        }
        guard.config.account_id = Some(account_id.clone());
        config::save_config_background(&guard.config);
    }
    let _ = tokio::task::spawn_blocking(move || history::adopt(&account_id)).await;
}

async fn refresh_playback_only(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) -> Result<()> {
    let refresh_lock = state.lock().unwrap().playback_refresh_lock.clone();
    let _refresh_guard = refresh_lock.lock().await;
    clear_expired_optimistic_state(&state);
    let session = state.lock().unwrap().session;
    let token = current_token(&state).await?;
    // On its own, so a slow answer doesn't hold up what's playing, under this lock.
    if state.lock().unwrap().config.account_id.is_none() {
        let (state, spotify_api, token) = (Arc::clone(&state), spotify_api.clone(), token.clone());
        tokio::spawn(async move { learn_account(&state, &spotify_api, &token, session).await });
    }

    // Do not collapse a request/parse failure into Spotify's real 204/no-playback
    // state. An error keeps the last confirmed snapshot and the next tick retries.
    let playback = spotify_api.current_playback(&token).await?;
    let remote_volume = {
        let mut guard = state.lock().unwrap();
        // Signed out while Spotify answered: the answer was about the account that left.
        ensure_same_session(&guard, session)?;
        let reported_active = playback
            .as_ref()
            .and_then(|p| p.device.as_ref())
            .filter(|d| d.is_active)
            .and_then(|d| d.id.clone());
        guard.active_device_id = reported_active.clone();
        if let Some(active) = reported_active {
            guard.preferred_device_id = Some(active.clone());
            guard.device_id = Some(active);
        }
        let recently_changed = guard
            .last_volume_change
            .is_some_and(|at| at.elapsed() < Duration::from_secs(3));
        if recently_changed {
            None
        } else if guard.local_volume_is_target() {
            // This PC's volume is the one shown. Spotify's report can still be about the
            // device played on before, like a speaker left at 20%.
            Some(guard.local_volume_percent)
        } else {
            playback
                .as_ref()
                .and_then(|p| p.device.as_ref())
                .filter(|d| d.is_active)
                .and_then(|d| d.volume_percent)
        }
    };
    if let Some(volume) = remote_volume {
        let ui_weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_volume_percent() != volume {
                    ui.set_volume_percent(volume.clamp(0, 100));
                    ui.set_muted(volume == 0);
                }
            }
        });
    }
    if playback
        .as_ref()
        .and_then(|playback| playback.item.as_ref())
        .is_some_and(|track| local_player_disagrees(&state, &track.uri))
    {
        // Spotify is describing a session we aren't playing: ignore its track,
        // queue and artwork. librespot's events keep the UI correct.
        return Ok(());
    }
    let accepted_track_uri = playback
        .as_ref()
        .and_then(|playback| playback.item.as_ref())
        .map(|track| track.uri.clone())
        .filter(|track_uri| !should_ignore_stale_playback_track(&state, track_uri));
    let art_url = playback
        .as_ref()
        .and_then(|playback| playback.item.as_ref())
        .and_then(|track| track.album.images.first())
        .map(|image| image.url.clone());
    let cached_art = art_url.as_deref().and_then(cached_album_art_path);
    if let (Some(url), Some(path)) = (art_url.as_ref(), cached_art.as_ref()) {
        let mut guard = state.lock().unwrap();
        guard.current_album_art_url = Some(url.clone());
        guard.current_album_art_path = Some(path.clone());
    }

    let playback_for_background = playback.clone();
    let _ = slint::invoke_from_event_loop({
        let ui_weak = ui_weak.clone();
        let state = Arc::clone(&state);
        move || {
            // Signing out can come first, while this waits its turn.
            if state.lock().unwrap().session != session {
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                apply_playback_to_ui(&ui, &state, playback, cached_art);
            }
        }
    });
    drop(_refresh_guard);

    if let Some(track_uri) = accepted_track_uri {
        remember_last_track_uri(&state, session, &track_uri);
    }
    if let Some(context_uri) = playback_for_background
        .as_ref()
        .and_then(|playback| playback.context.as_ref())
        .and_then(|context| context.uri.clone())
        .filter(|uri| uri.starts_with("spotify:playlist:"))
    {
        remember_last_playlist_uri(&state, session, &context_uri);
    }

    if reserve_spotify_queue_refresh(&state) {
        let queue_state = Arc::clone(&state);
        let queue_ui = ui_weak.clone();
        let queue_api = spotify_api.clone();
        let queue_token = token.clone();
        let queue_playback = playback_for_background.clone();
        tokio::spawn(async move {
            if let Ok(queue) = queue_api.queue(&queue_token).await {
                update_spotify_queue(&queue_state, session, queue_playback.as_ref(), queue);
                repaint_queue_on_event_loop(queue_ui, queue_state);
            }
        });
    }

    if let (Some(track_uri), Some(url)) = (
        playback_for_background
            .as_ref()
            .and_then(|playback| playback.item.as_ref())
            .map(|track| track.uri.clone()),
        art_url,
    ) {
        let should_download = {
            let mut guard = state.lock().unwrap();
            if guard.current_album_art_url.as_deref() == Some(url.as_str()) {
                false
            } else {
                guard.current_album_art_url = Some(url.clone());
                guard.current_album_art_path = None;
                true
            }
        };
        if should_download {
            let art_state = Arc::clone(&state);
            let art_ui = ui_weak;
            tokio::spawn(async move {
                match load_album_art(&url).await {
                    Ok(path) => {
                        let still_current = {
                            let mut guard = art_state.lock().unwrap();
                            let matches = guard.current_track_uri.as_deref() == Some(track_uri.as_str())
                                && guard.current_album_art_url.as_deref() == Some(url.as_str());
                            if matches {
                                guard.current_album_art_path = Some(path.clone());
                            }
                            matches
                        };
                        if still_current {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = art_ui.upgrade() {
                                    set_album_art_if_changed(&ui, &art_state, Some(path));
                                }
                            });
                        }
                    }
                    Err(_) => {
                        let mut guard = art_state.lock().unwrap();
                        if guard.current_album_art_url.as_deref() == Some(url.as_str())
                            && guard.current_album_art_path.is_none()
                        {
                            guard.current_album_art_url = None;
                        }
                    }
                }
            });
        }
    }

    Ok(())
}

async fn refresh_everything(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    include_library: bool,
    log_refresh: bool,
    resume_on_local_device: bool,
) -> Result<bool> {
    clear_expired_optimistic_state(&state);
    let session = {
        let guard = state.lock().unwrap();
        if guard.token.is_none() {
            return Ok(false);
        }
        guard.session
    };
    let token = current_token(&state).await?;
    learn_account(&state, &spotify_api, &token, session).await;
    // Spotify may have just asked to wait, maybe over the account question above. Asking
    // sooner only makes the wait longer, so the rest waits for the next regular check.
    if spotify::rate_limit_wait().is_some() {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_connection_status("Connected - refresh delayed".into());
            }
        });
        return Err(anyhow!("waiting, as Spotify asked"));
    }

    let devices = match spotify_api.devices(&token).await {
        Ok(devices) => devices,
        Err(error) => {
            let message = error.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_connection_status("Connected - refresh delayed".into());
                    append_log(&ui, &format!("Spotify device refresh failed: {message}"));
                }
            });
            return Err(error);
        }
    };
    let device_name = state.lock().unwrap().config.device_name.clone();
    let preferred_device_id = state.lock().unwrap().preferred_device_id.clone();
    let local_device_id = devices
        .iter()
        .find(|d| d.name == device_name)
        .and_then(|d| d.id.clone());
    let active_device_id = devices.iter().find(|d| d.is_active).and_then(|d| d.id.clone());
    // Control whatever is actually playing. Only when nothing is active do we
    // fall back to the user's pick, then this PC.
    let selected_device_id = active_device_id
        .clone()
        .or_else(|| {
            preferred_device_id.and_then(|preferred| {
                devices
                    .iter()
                    .find(|d| d.id.as_deref() == Some(preferred.as_str()))
                    .and_then(|d| d.id.clone())
            })
        })
        .or_else(|| local_device_id.clone());
    let mut ordered_devices = devices;
    if let Some(selected_id) = selected_device_id.as_deref() {
        ordered_devices.sort_by_key(|device| {
            if device.id.as_deref() == Some(selected_id) {
                0
            } else {
                1
            }
        });
    } else {
        ordered_devices.sort_by_key(|device| if device.is_active { 0 } else { 1 });
    }
    let device_summary = ordered_devices
        .iter()
        .map(|d| format!("{}{}", d.name, if d.is_active { " (active)" } else { "" }))
        .collect::<Vec<_>>()
        .join(", ");
    let device_display_strings = ordered_devices
        .iter()
        .map(|d| {
            let marker = if d.name == device_name {
                "[PC]"
            } else if d.is_active {
                "[ON]"
            } else {
                "[DV]"
            };
            let volume = d
                .volume_percent
                .map(|value| format!(" - {}%", value))
                .unwrap_or_default();
            format!("{marker} {}{}", d.name, volume)
        })
        .collect::<Vec<_>>();
    let device_ids = ordered_devices
        .iter()
        .map(|d| d.id.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    let device_names = ordered_devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
    {
        let mut guard = state.lock().unwrap();
        // These are the devices of the account before, if it signed out meanwhile.
        if ensure_same_session(&guard, session).is_err() {
            return Ok(false);
        }
        guard.local_device_id = local_device_id.clone();
        guard.active_device_id = active_device_id;
        guard.device_id = selected_device_id;
        guard.available_device_ids = device_ids.clone();
        guard.available_device_names = device_names;
    }

    let mut liked_full_tracks: Option<Vec<FullTrack>> = None;
    // How much of Liked Songs and of the playlists there is, as far as this refresh found.
    let mut liked_loaded: Option<Loaded> = None;
    let mut playlists_found: Option<(usize, usize)> = None;
    let mut asked_at_revision = 0;
    let (playlist_uris, playlist_track_hrefs, playlist_track_counts, liked_uris, liked_table_columns, snapshot) = if include_library {
        let now = config::now_epoch();
        let refresh_gate = {
            let mut guard = state.lock().unwrap();
            if guard.library_refresh_in_flight == Some(guard.session) {
                Some(Err::<(), String>("Library refresh already in progress.".to_string()))
            } else if now < guard.next_allowed_library_refresh_at_epoch {
                let wait_secs = guard.next_allowed_library_refresh_at_epoch.saturating_sub(now);
                Some(Err::<(), String>(format!("Library refresh cooling down for another {}s.", wait_secs)))
            } else {
                guard.library_refresh_in_flight = Some(session);
                None
            }
        };

        if let Some(Err(message)) = refresh_gate {
            let _ = slint::invoke_from_event_loop({
                let ui_weak = ui_weak.clone();
                move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        append_log(&ui, &message);
                    }
                }
            });
                (None, None, None, None, None, None)
        } else {
            let (playlists_asked, liked_wanted) = {
                let guard = state.lock().unwrap();
                asked_at_revision = guard.playlists_revision;
                (guard.config.library_snapshot.playlists_wanted.max(spotify::PLAYLISTS_PER_PART), songs_wanted(&guard, LIKED_SONGS_PLAYLIST_URI))
            };
            let playlists = match spotify_api.playlists(&token, 0, playlists_asked).await {
                Ok(part) => {
                    playlists_found = Some((part.total, playlists_asked));
                    Some(part.items)
                }
                Err(error) => {
                    let retry_after = error.downcast_ref::<spotify::RateLimitError>().map(|e| e.retry_after_secs);
                    if let Some(retry_after) = retry_after {
                        let mut guard = state.lock().unwrap();
                        let until = config::now_epoch().saturating_add(retry_after);
                        guard.next_allowed_library_refresh_at_epoch = until;
                        guard.config.library_snapshot.next_allowed_refresh_at_epoch = until;
                        config::save_config_background(&guard.config);
                    }
                    let _ = slint::invoke_from_event_loop({
                        let ui_weak = ui_weak.clone();
                        let message = format!("Playlist refresh failed: {error}");
                        move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                append_log(&ui, &message);
                            }
                        }
                    });
                    None
                }
            };
            // Spotify just asked to wait, so asking it for more now would only stretch the wait.
            let liked = match spotify::rate_limit_wait() {
                Some(_) => Err(anyhow!("waiting, as Spotify asked")),
                None => spotify_api.liked_songs(&token, 0, liked_wanted).await,
            };
            let liked = match liked {
                Ok(part) => {
                    liked_loaded = Some(Loaded { total: part.total, wanted: liked_wanted, more_from: part.more_from, stamp: 0 });
                    Some(part.items)
                }
                Err(error) => {
                    let retry_after = error.downcast_ref::<spotify::RateLimitError>().map(|e| e.retry_after_secs);
                    if let Some(retry_after) = retry_after {
                        let mut guard = state.lock().unwrap();
                        let until = config::now_epoch().saturating_add(retry_after);
                        guard.next_allowed_library_refresh_at_epoch = until;
                        guard.config.library_snapshot.next_allowed_refresh_at_epoch = until;
                        config::save_config_background(&guard.config);
                    }
                    let _ = slint::invoke_from_event_loop({
                        let ui_weak = ui_weak.clone();
                        let message = format!("Liked songs refresh failed: {error}");
                        move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                append_log(&ui, &message);
                            }
                        }
                    });
                    None
                }
            };

            {
                let mut guard = state.lock().unwrap();
                if guard.library_refresh_in_flight == Some(session) {
                    guard.library_refresh_in_flight = None;
                }
            }
            liked_full_tracks = liked.clone();

            let playlist_uris = playlists.as_ref().map(|playlists| {
                playlists.iter().map(|item| item.uri.clone()).collect::<Vec<_>>()
            });
            let playlist_track_hrefs = playlists.as_ref().map(|playlists| {
                playlists.iter().map(|item| item.tracks.href.clone()).collect::<Vec<_>>()
            });
            let playlist_track_counts = playlists.as_ref().map(|playlists| {
                playlists.iter().map(|item| item.tracks.total).collect::<Vec<_>>()
            });
            let liked_uris = liked.as_ref().map(|liked| {
                liked.iter().map(|track| track.uri.clone()).collect::<Vec<_>>()
            });
            let liked_table_columns = liked.clone();
            let snapshot = if let (Some(playlists), Some(liked)) = (playlists.as_ref(), liked.as_ref()) {
                Some((
                    playlists.iter().map(cached_playlist).collect::<Vec<_>>(),
                    liked
                        .iter()
                        // The rest loads again when asked for, which keeps the settings file small.
                        .take(spotify::SONGS_PER_PART)
                        .map(|track| CachedLikedSong {
                            name: track.name.clone(),
                            artists: track.artists.iter().map(|artist| artist.name.clone()).collect::<Vec<_>>(),
                            uri: track.uri.clone(),
                            duration_ms: track.duration_ms,
                            album: track.album.name.clone(),
                            image_url: track.album.images.last().map(|image| image.url.clone()),
                        })
                        .collect::<Vec<_>>(),
                    config::now_epoch(),
                ))
            } else {
                None
            };
            (playlist_uris, playlist_track_hrefs, playlist_track_counts, liked_uris, liked_table_columns, snapshot)
        }
    } else {
        (None, None, None, None, None, None)
    };

    {
        let mut guard = state.lock().unwrap();
        // Signed out while the library was loading: it belongs to the account that left.
        if ensure_same_session(&guard, session).is_err() {
            return Ok(false);
        }
        // What was fetched goes in below, with the sidebar; the wait starts now.
        if let Some((_, _, fetched_at_epoch)) = snapshot.as_ref() {
            guard.config.library_snapshot.fetched_at_epoch = *fetched_at_epoch;
            guard.config.library_snapshot.next_allowed_refresh_at_epoch = fetched_at_epoch.saturating_add(120);
            guard.next_allowed_library_refresh_at_epoch = guard.config.library_snapshot.next_allowed_refresh_at_epoch;
        }
    }

    // Library/device pagination is intentionally outside the playback lock so a
    // manual refresh cannot starve the now-playing poller.
    let refresh_lock = state.lock().unwrap().playback_refresh_lock.clone();
    let refresh_guard = refresh_lock.lock().await;
    // Spotify asked to wait just now, maybe for the library above. The regular check asks
    // again once it may.
    let playback = match spotify::rate_limit_wait() {
        Some(_) => Err(anyhow!("waiting, as Spotify asked")),
        None => spotify_api.current_playback(&token).await,
    };
    let (playback, playback_refresh_error) = match playback {
        Ok(playback) => (playback, None),
        Err(error) => (None, Some(error.to_string())),
    };
    if playback_refresh_error.is_none() && reserve_spotify_queue_refresh(&state) {
        // On its own, as in the quick refresh, so a slow queue doesn't hold up what's playing
        // or the next refresh, which waits for this one's lock.
        let (queue_state, queue_ui, queue_api) = (Arc::clone(&state), ui_weak.clone(), spotify_api.clone());
        let (queue_token, queue_playback) = (token.clone(), playback.clone());
        tokio::spawn(async move {
            if let Ok(queue) = queue_api.queue(&queue_token).await {
                update_spotify_queue(&queue_state, session, queue_playback.as_ref(), queue);
                repaint_queue_on_event_loop(queue_ui, queue_state);
            }
        });
    }
    if ensure_same_session(&state.lock().unwrap(), session).is_err() {
        return Ok(false);
    }
    if playback_refresh_error.is_none() && !resume_on_local_device {
        if let Some(track_uri) = playback
            .as_ref()
            .and_then(|playback| playback.item.as_ref())
            .map(|track| track.uri.clone())
            .filter(|track_uri| !should_ignore_stale_playback_track(&state, track_uri))
        {
            remember_last_track_uri(&state, session, &track_uri);
        }
        if let Some(context_uri) = playback
            .as_ref()
            .and_then(|playback| playback.context.as_ref())
            .and_then(|context| context.uri.clone())
            .filter(|uri| uri.starts_with("spotify:playlist:"))
        {
            remember_last_playlist_uri(&state, session, &context_uri);
        }
    }
    let cached_first_liked_uri = {
        let guard = state.lock().unwrap();
        guard.track_uris.first().cloned()
    };
    let cached_first_playlist_uri = {
        let guard = state.lock().unwrap();
        guard.playlist_uris.first().cloned()
    };
    let remembered_last_track_uri = state.lock().unwrap().config.startup_snapshot.last_track_uri.clone();
    let remembered_last_playlist_uri = state.lock().unwrap().config.startup_snapshot.last_playlist_uri.clone();
    let first_liked_uri = liked_uris
        .as_ref()
        .and_then(|tracks| tracks.first().cloned())
        .or(cached_first_liked_uri);
    let first_playlist_uri = playlist_uris
        .as_ref()
        .and_then(|playlists| playlists.first().cloned())
        .or(cached_first_playlist_uri);
    let should_resume_on_local = resume_on_local_device
        && playback_refresh_error.is_none()
        && playback.as_ref().and_then(|p| p.item.as_ref()).is_some()
        && state.lock().unwrap().local_device_id.is_some();
    let mut resumed_on_local = false;
    if should_resume_on_local {
        let device_id = state.lock().unwrap().local_device_id.clone();
        if let Some(device_id) = device_id {
            if spotify_api.transfer(&token, &device_id, true).await.is_ok() {
                resumed_on_local = true;
            }
        }
    }
    let mut started_fallback_playback = false;
    let mut fallback_started_track = None;
    if resume_on_local_device
        && playback_refresh_error.is_none()
        && !resumed_on_local
        && playback.as_ref().and_then(|p| p.item.as_ref()).is_none()
    {
        let device_id = state.lock().unwrap().local_device_id.clone();
        if let Some(device_id) = device_id {
            let remembered_track_uri = remembered_last_track_uri.or(first_liked_uri);
            let remembered_playlist_uri = remembered_last_playlist_uri.or(first_playlist_uri);
            if let Some(track_uri) = remembered_track_uri {
                let remembered_context_uri = remembered_playlist_uri
                    .as_deref()
                    .filter(|uri| uri.starts_with("spotify:playlist:"));
                let queue_from_playlist = if let Some(playlist_uri) = remembered_context_uri {
                    build_startup_queue_from_playlist(&state, &spotify_api, &token, playlist_uri, &track_uri).await
                } else {
                    None
                };
                {
                    let started = if let Some(context_uri) = remembered_context_uri {
                        if spotify_api
                            .play_context_from_track(&token, &device_id, context_uri, &track_uri)
                            .await
                            .is_ok()
                        {
                            true
                        } else if let Some(track_uris) = queue_from_playlist {
                            spotify_api.play_tracks(&token, &device_id, &track_uris).await.is_ok()
                        } else {
                            spotify_api.play_playlist(&token, &device_id, context_uri).await.is_ok()
                        }
                    } else {
                        spotify_api.play_track(&token, &device_id, &track_uri).await.is_ok()
                    };
                    if started {
                        remember_last_track_uri(&state, session, &track_uri);
                        if let Some(context_uri) = remembered_playlist_uri {
                            remember_last_playlist_uri(&state, session, &context_uri);
                        }
                        if let Some(track_id) = track_id_from_uri(&track_uri) {
                            fallback_started_track = spotify_api.track(&token, &track_id).await.ok();
                        }
                        started_fallback_playback = true;
                    }
                }
            } else if let Some(context_uri) = remembered_playlist_uri {
                if spotify_api.play_playlist(&token, &device_id, &context_uri).await.is_ok() {
                    remember_last_playlist_uri(&state, session, &context_uri);
                    started_fallback_playback = true;
                }
            }
        }
    }
    let album_art = playback
        .as_ref()
        .and_then(|p| p.item.as_ref())
        .or(fallback_started_track.as_ref())
        .and_then(|track| track.album.images.first())
        .and_then(|image| cached_album_art_path(&image.url));
    let warm_ui_weak = ui_weak.clone();
    let warm_state = Arc::clone(&state);
    // Dropped once the library below is in, or isn't going in.
    let (library_in, library_in_wait) = tokio::sync::oneshot::channel::<()>();
    let _ = slint::invoke_from_event_loop(move || {
        let _library_in = library_in;
        // Signing out can come first, while this waits its turn.
        if state.lock().unwrap().session != session {
            return;
        }
        if let Some(ui) = ui_weak.upgrade() {
            // More liked songs loaded meanwhile are kept: this refresh asked for less.
            let liked_superseded = {
                let guard = state.lock().unwrap();
                liked_loaded.is_some_and(|fresh| guard.loaded.get(LIKED_SONGS_PLAYLIST_URI).is_some_and(|now| now.wanted > fresh.wanted))
            };
            let liked_uris = liked_uris.filter(|_| !liked_superseded);
            let liked_table_columns = liked_table_columns.filter(|_| !liked_superseded);
            {
                let mut guard = state.lock().unwrap();
                // Rows index into the track cache, so whenever fresh liked songs are
                // shown the cache must update too, even if the playlist fetch failed.
                if let Some(liked_uris) = liked_uris.as_ref() {
                    if guard.selected_playlist_uri == LIKED_SONGS_PLAYLIST_URI {
                        guard.track_uris = liked_uris.clone();
                    }
                    if let Some(liked_tracks) = liked_full_tracks.clone() {
                        guard
                            .playlist_track_cache
                            .insert(LIKED_SONGS_PLAYLIST_URI.to_string(), liked_tracks);
                    }
                    if let Some(loaded) = liked_loaded {
                        mark_loaded(&mut guard, LIKED_SONGS_PLAYLIST_URI, loaded);
                    }
                }
                // Replaced only when both fetches succeed. The sidebar draws from the snapshot
                // and a click goes by these lists, so they change here together, just before
                // the rows are drawn again, unless a "Load more" or a new playlist changed them
                // since this refresh asked.
                if let Some((playlist_snapshot, liked_snapshot, _)) = snapshot {
                    if let (Some(playlist_uris), Some(playlist_track_hrefs), Some(playlist_track_counts), true) =
                        (playlist_uris, playlist_track_hrefs, playlist_track_counts, guard.playlists_revision == asked_at_revision)
                    {
                        guard.playlists_revision = asked_at_revision.wrapping_add(1);
                        guard.playlist_uris = playlist_uris;
                        guard.playlist_track_hrefs = playlist_track_hrefs;
                        guard.playlist_track_counts = playlist_track_counts;
                        guard.config.library_snapshot.playlists = playlist_snapshot;
                        if let Some((total, asked)) = playlists_found {
                            guard.config.library_snapshot.playlists_total = total;
                            guard.config.library_snapshot.playlists_wanted = asked;
                        }
                    }
                    guard.config.library_snapshot.liked_songs = liked_snapshot;
                    if let Some(loaded) = liked_loaded {
                        guard.config.library_snapshot.liked_songs_total = loaded.total;
                    }
                    config::save_config_background(&guard.config);
                }
            }
            {
                let (playlists, liked_count, artwork_enabled, selected_playlist_uri) = {
                    let guard = state.lock().unwrap();
                    (
                        guard.config.library_snapshot.playlists.clone(),
                        liked_count(&guard.config.library_snapshot),
                        guard.config.artwork_enabled,
                        guard.selected_playlist_uri.clone(),
                    )
                };
                apply_playlist_rows(
                    &ui,
                    &playlists,
                    liked_count,
                    artwork_enabled,
                    Some(selected_playlist_uri.as_str()),
                );
                ui.set_more_playlists(more_playlists(&state.lock().unwrap().config.library_snapshot));
            }
            if state.lock().unwrap().selected_playlist_uri == LIKED_SONGS_PLAYLIST_URI && !ui.get_show_home() {
                if let Some(liked_tracks) = liked_table_columns {
                    refresh_tracks_in_place(&ui, liked_tracks);
                    show_more_to_load(&ui, &state);
                }
            }
            let should_update_devices = {
                let mut guard = state.lock().unwrap();
                if guard.last_device_display != device_display_strings {
                    guard.last_device_display = device_display_strings.clone();
                    true
                } else {
                    false
                }
            };
            if should_update_devices {
                apply_device_rows(&ui, &ordered_devices, &device_name);
            }
            let device_ready = state.lock().unwrap().local_device_id.is_some();
            ui.set_player_status(if device_ready { "Local player ready".into() } else { "Waiting for local player".into() });
            if log_refresh {
                let device_log = if device_summary.is_empty() {
                    "Spotify devices: none".to_string()
                } else {
                    format!("Spotify devices: {device_summary}")
                };
                append_log(&ui, &device_log);
            }

            if let Some(error) = playback_refresh_error {
                ui.set_connection_status("Connected - playback sync delayed".into());
                if log_refresh {
                    append_log(&ui, &format!("Playback refresh failed: {error}"));
                }
            } else if let Some(track) = fallback_started_track {
                set_now_playing_from_track(&ui, &state, &track, NowPlayingUpdate::Optimistic);
                if let Some(album_art) = album_art.as_ref() {
                    set_album_art_if_changed(&ui, &state, Some(album_art.clone()));
                }
                begin_fast_playback_refresh(&state, Duration::from_secs(5));
            } else {
                apply_playback_to_ui(&ui, &state, playback, album_art);
            }
            if log_refresh {
                append_log(&ui, "State refreshed.");
            }
        }
    });
    drop(refresh_guard);

    if include_library {
        // Covers found now go into the fresh list, not one about to be replaced.
        let _ = library_in_wait.await;
        let _ = backfill_missing_playlist_image_urls(
            &warm_state,
            &spotify_api,
            &token,
            PLAYLIST_ARTWORK_WARM_LIMIT,
        )
        .await;
        warm_playlist_art_and_repaint(warm_ui_weak.clone(), Arc::clone(&warm_state)).await;
    }
    if spotlight_window_is_foreground() {
        warm_queue_art_and_repaint(warm_ui_weak, warm_state, spotify_api).await;
    }

    Ok(resumed_on_local || started_fallback_playback)
}

fn album_art_cache_dir() -> Result<PathBuf> {
    if let Some(path) = ALBUM_ART_CACHE_DIR.get() {
        return Ok(path.clone());
    }
    let path = config::config_dir()?.join("album-art");
    let _ = ALBUM_ART_CACHE_DIR.set(path.clone());
    Ok(path)
}

/// The covers saved on disk: when each was fetched, its size and where it is.
fn album_art_files() -> Vec<(std::time::SystemTime, u64, PathBuf)> {
    let Ok(entries) = album_art_cache_dir().and_then(|dir| Ok(std::fs::read_dir(dir)?)) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| (metadata.modified().unwrap_or(std::time::UNIX_EPOCH), metadata.len(), entry.path()))
        })
        .collect()
}

/// Works out, off the UI thread, how much the saved covers take, first deleting them if asked,
/// and shows it in Settings.
fn show_cover_cache_size(ui_weak: slint::Weak<AppWindow>, clear_first: bool) {
    let _ = std::thread::Builder::new().name("spotlight-cover-cache".to_string()).spawn(move || {
        if clear_first {
            // One being written or read just now stays. It's fetched again when it's next needed.
            for (_, _, path) in album_art_files() {
                let _ = std::fs::remove_file(path);
            }
        }
        let bytes: u64 = album_art_files().iter().map(|(_, size, _)| size).sum();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_cover_cache_note(cover_cache_note(bytes).into());
                ui.set_cover_cache_clearable(bytes > 0);
                if clear_first {
                    show_toast(&ui, "Saved covers cleared", false);
                }
            }
        });
    });
}

fn cover_cache_note(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    match bytes {
        0 => "None saved yet. Album covers are kept on this PC, so they show at once.".to_string(),
        _ if bytes < MB => "Less than 1 MB of album covers, kept so they show at once.".to_string(),
        _ => format!("{} MB of album covers, kept so they show at once.", (bytes + MB / 2) / MB),
    }
}

/// Keeps the covers on disk from growing for ever: past 250 MB, the ones fetched longest ago
/// go until it's 200 MB. One still in use is simply fetched again.
fn trim_album_art_cache() {
    const LIMIT: u64 = 250 * 1024 * 1024;
    const TRIMMED_TO: u64 = 200 * 1024 * 1024;
    let mut files = album_art_files();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= LIMIT {
        return;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, size, path) in files {
        if total <= TRIMMED_TO {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
}

async fn load_album_art(url: &str) -> Result<std::path::PathBuf> {
    let cache_dir = tokio::task::spawn_blocking(|| -> Result<PathBuf> {
        let cache_dir = album_art_cache_dir()?;
        std::fs::create_dir_all(&cache_dir)?;
        Ok(cache_dir)
    })
    .await??;
    use sha2::Digest;
    let extension = album_art_extension(url);
    let filename = format!("{:x}.{}", sha2::Sha256::digest(url.as_bytes()), extension);
    let path = cache_dir.join(filename);

    if !path.exists() {
        let bytes = ALBUM_ART_CLIENT
            .get_or_init(|| {
                reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(4))
                    .timeout(Duration::from_secs(10))
                    .pool_idle_timeout(Duration::from_secs(30))
                    .build()
                    .expect("valid artwork HTTP client configuration")
            })
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let write_path = path.clone();
        // Written aside first so an interrupted download can't leave a broken cover.
        tokio::task::spawn_blocking(move || {
            let partial = write_path.with_extension("partial");
            std::fs::write(&partial, &bytes)?;
            std::fs::rename(&partial, &write_path)
        })
        .await??;
    }

    Ok(path)
}

fn album_art_extension(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|segments| segments.last().map(|s| s.to_string()))
        })
        .and_then(|name| name.rsplit('.').next().map(|ext| ext.to_ascii_lowercase()))
        .filter(|ext| matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp"))
        .unwrap_or_else(|| "jpg".to_string())
}

fn cached_album_art_path(url: &str) -> Option<std::path::PathBuf> {
    use sha2::Digest;
    let cache_dir = album_art_cache_dir().ok()?;
    let filename = format!("{:x}.{}", sha2::Sha256::digest(url.as_bytes()), album_art_extension(url));
    let path = cache_dir.join(filename);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn load_image_from_path(path: &std::path::PathBuf) -> slint::Image {
    ALBUM_ART_IMAGES.with(|cached| {
        if let Some(image) = cached.borrow().get(path).cloned() {
            return image;
        }
        let image = slint::Image::load_from_path(path)
            .ok()
            .or_else(|| decode_by_content(path))
            .unwrap_or_else(load_spotlight_logo);
        let mut cached = cached.borrow_mut();
        if cached.len() >= MAX_DECODED_ALBUM_ART_IMAGES {
            cached.clear();
        }
        cached.insert(path.clone(), image.clone());
        image
    })
}

/// Art that fills a large area. Not kept here once nothing shows it: holding on
/// would keep a megabyte or two for every song played this session.
fn load_full_size_art(path: &std::path::Path) -> slint::Image {
    slint::Image::load_from_path(path)
        .ok()
        .or_else(|| decode_by_content(path))
        .unwrap_or_else(load_spotlight_logo)
}

/// Spotify's playlist covers are usually 640px and every one is on screen at once,
/// so decoding them at the size they're drawn is what keeps the sidebar small.
///
/// Shrinking a 640px cover takes about 7 ms, which for 30 playlists held up the first frame by
/// a fifth of a second. So the small copy is saved beside the cover and read back next time,
/// which takes about half a millisecond.
fn load_playlist_cover(path: &std::path::PathBuf, scale_factor: f32) -> slint::Image {
    PLAYLIST_COVERS.with(|cached| {
        if let Some(image) = cached.borrow().get(path).cloned() {
            return image;
        }
        let size = (PLAYLIST_COVER_PX * scale_factor).round() as u32;
        let small_copy = path.with_file_name(format!(
            "{}.sidebar-{size}.png",
            path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default()
        ));
        let saved = std::fs::read(&small_copy)
            .ok()
            .and_then(|bytes| image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok())
            .map(|decoded| decoded.into_rgba8())
            .filter(|rgba| rgba.width() == size && rgba.height() == size);
        let rgba = saved.or_else(|| {
            let decoded = image::load_from_memory(&std::fs::read(path).ok()?).ok()?;
            // Same crop as the `image-fit: cover` these are drawn with.
            let rgba = decoded.resize_to_fill(size, size, image::imageops::FilterType::Triangle).into_rgba8();
            save_small_cover(small_copy, rgba.clone());
            Some(rgba)
        });
        let image = rgba
            .map(|rgba| {
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height());
                slint::Image::from_rgba8(buffer)
            })
            .unwrap_or_else(|| load_image_from_path(path));
        cached.borrow_mut().insert(path.clone(), image.clone());
        image
    })
}

/// Written off the UI thread, under another name first so a half-written copy is never read.
/// It lives with the covers, so clearing or trimming them takes it too.
fn save_small_cover(path: PathBuf, rgba: image::RgbaImage) {
    let Some(handle) = RUNTIME_HANDLE.get() else { return };
    handle.spawn_blocking(move || {
        let part = path.with_extension("png.part");
        let saved = rgba.save_with_format(&part, image::ImageFormat::Png).is_ok() && std::fs::rename(&part, &path).is_ok();
        if !saved {
            let _ = std::fs::remove_file(&part);
        }
    });
}

/// The cached file's extension comes from its URL, which can be wrong (Spotify
/// sends WebP from extensionless URLs), so go by what the bytes actually are.
fn decode_by_content(path: &std::path::Path) -> Option<slint::Image> {
    let rgba = image::load_from_memory(&std::fs::read(path).ok()?).ok()?.into_rgba8();
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height());
    Some(slint::Image::from_rgba8(buffer))
}

fn load_spotlight_logo() -> slint::Image {
    SPOTLIGHT_LOGO_IMAGE.with(|cached| {
        if let Some(image) = cached.borrow().as_ref().cloned() {
            return image;
        }
        // A vector, so it's drawn sharp at whatever size it's shown. The 1024 px PNG
        // turned jagged at title bar size and held 4 MB once decoded.
        let image = embedded_asset_path("spotlight-mark.svg")
            .and_then(|path| slint::Image::load_from_path(&path).ok())
            .unwrap_or_default();
        *cached.borrow_mut() = Some(image.clone());
        image
    })
}

fn load_icon(name: &str) -> slint::Image {
    STATIC_ICON_IMAGES.with(|cached| {
        if let Some(image) = cached.borrow().get(name).cloned() {
            return image;
        }
        let image = embedded_asset_path(name)
            .and_then(|path| slint::Image::load_from_path(&path).ok())
            .unwrap_or_default();
        cached.borrow_mut().insert(name.to_string(), image.clone());
        image
    })
}

macro_rules! embedded_icon {
    ($name:literal) => {
        ($name, include_bytes!(concat!("../assets/icons/", $name)).as_slice())
    };
}

/// The installer ships only the exe, so assets live inside it and are unpacked
/// to the app data folder for Slint to load by path.
fn embedded_asset_path(name: &str) -> Option<PathBuf> {
    let assets: [(&str, &[u8]); 28] = [
        ("spotlight-mark.svg", include_bytes!("../assets/spotlight-mark.svg").as_slice()),
        embedded_icon!("computer.svg"),
        embedded_icon!("device.svg"),
        embedded_icon!("liked-songs.svg"),
        embedded_icon!("next.svg"),
        embedded_icon!("pause.svg"),
        embedded_icon!("phone.svg"),
        embedded_icon!("play.svg"),
        embedded_icon!("playlist.svg"),
        embedded_icon!("previous.svg"),
        embedded_icon!("queue.svg"),
        embedded_icon!("refresh.svg"),
        embedded_icon!("repeat.svg"),
        embedded_icon!("search.svg"),
        embedded_icon!("settings.svg"),
        embedded_icon!("shuffle.svg"),
        embedded_icon!("speaker.svg"),
        embedded_icon!("spinner.svg"),
        embedded_icon!("tv.svg"),
        embedded_icon!("volume.svg"),
        embedded_icon!("volume-low.svg"),
        embedded_icon!("volume-mute.svg"),
        embedded_icon!("repeat-one.svg"),
        embedded_icon!("fullscreen.svg"),
        embedded_icon!("plus.svg"),
        embedded_icon!("lyrics.svg"),
        embedded_icon!("mini-player.svg"),
        embedded_icon!("recently-played.svg"),
    ];
    let bytes = assets.iter().find(|(asset, _)| *asset == name)?.1;
    let dir = config::config_dir().ok()?.join("assets");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    // Rewrite when the build's copy differs, so updated icons take effect.
    if std::fs::read(&path).ok().as_deref() != Some(bytes) {
        std::fs::write(&path, bytes).ok()?;
    }
    Some(path)
}

async fn toggle_playback(
    spotify_api: &spotify::SpotifyApi,
    token: &TokenStore,
    device_id: Option<&str>,
) -> Result<()> {
    let playback = spotify_api.current_playback(token).await?;
    let is_playing = playback.as_ref().map(|p| p.is_playing).unwrap_or(false);
    // Always address the device that actually holds the session.
    let session_device = playback
        .as_ref()
        .and_then(|p| p.device.as_ref())
        .and_then(|d| d.id.clone());
    let has_context = playback.and_then(|p| p.item).is_some();
    let target = session_device.as_deref().or(device_id);

    if is_playing {
        spotify_api.pause(token, target).await
    } else if has_context {
        spotify_api.resume(token, target).await
    } else if let Some(device_id) = device_id {
        // No active session anywhere: ask Spotify to resume the last one here.
        spotify_api
            .transfer(token, device_id, true)
            .await
            .map_err(|_| anyhow::anyhow!("nothing is currently queued to resume; start a playlist or liked song first"))
    } else {
        Err(anyhow::anyhow!("nothing is currently queued to resume; start a playlist or liked song first"))
    }
}

async fn start_current_visible_tracks(
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) -> Result<()> {
    let (token, device_id, selected_playlist_uri, queue_tracks, first_track) = {
        let guard = state.lock().unwrap();
        let queue_tracks = guard
            .playlist_track_cache
            .get(&guard.selected_playlist_uri)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|track| track.uri)
            .collect::<Vec<_>>();
        let first_track = guard.track_uris.first().cloned();
        (
            guard.token.clone(),
            guard.device_id.clone(),
            guard.selected_playlist_uri.clone(),
            queue_tracks,
            first_track,
        )
    };

    let token = token.ok_or_else(|| anyhow!("not connected"))?;
    let device_id = device_id.ok_or_else(|| anyhow!("no local Spotify device ready"))?;

    if !queue_tracks.is_empty() {
        if selected_playlist_uri != LIKED_SONGS_PLAYLIST_URI {
            remember_last_playlist_uri(&state, current_session(&state), &selected_playlist_uri);
        }
        if let Some(track_uri) = queue_tracks.first() {
            remember_last_track_uri(&state, current_session(&state), track_uri);
        }
        spotify_api.play_tracks(&token, &device_id, &queue_tracks).await
    } else if let Some(track_uri) = first_track {
        remember_last_track_uri(&state, current_session(&state), &track_uri);
        spotify_api.play_track(&token, &device_id, &track_uri).await
    } else {
        Err(anyhow!("nothing is currently queued to resume; start a playlist or liked song first"))
    }
}

async fn switch_playback_device(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    token: TokenStore,
    mut device_id: String,
    _device_name: String,
    is_local_device: bool,
) -> Result<String> {
    let playback_before = spotify_api.current_playback(&token).await?;
    let was_playing = playback_before.as_ref().map(|p| p.is_playing).unwrap_or(false);
    let current_track_uri = playback_before
        .as_ref()
        .and_then(|p| p.item.as_ref())
        .map(|track| track.uri.clone());
    let current_progress_ms = playback_before
        .as_ref()
        .and_then(|p| p.progress_ms)
        .unwrap_or_default()
        .max(0);

    if is_local_device {
        if !local_player_is_running(&state) {
            clear_local_playback_engine(&state);
            ensure_local_playback(Arc::clone(&state), ui_weak.clone()).await?;
        }
        wait_for_local_device_refresh(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await;
        if let Some(local_device_id) = state.lock().unwrap().local_device_id.clone() {
            device_id = local_device_id;
        }
    }

    spotify_api.transfer(&token, &device_id, false).await?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    if was_playing {
        let resume_result = spotify_api.resume(&token, Some(device_id.as_str())).await;
        let needs_explicit_start = match resume_result {
            Ok(()) => {
                tokio::time::sleep(Duration::from_millis(600)).await;
                match spotify_api.current_playback(&token).await? {
                    Some(playback) => !playback.is_playing,
                    None => true,
                }
            }
            Err(_) => true,
        };
        if needs_explicit_start {
            if let Some(track_uri) = current_track_uri.clone() {
                let queue_uris = queue_uris_from_current_track(&state, &track_uri);
                if queue_uris.len() > 1 {
                    spotify_api.play_tracks(&token, &device_id, &queue_uris).await?;
                } else {
                    spotify_api.play_track(&token, &device_id, &track_uri).await?;
                }
                if current_progress_ms > 0 {
                    let _ = spotify_api.seek(&token, Some(device_id.as_str()), current_progress_ms).await;
                }
            } else {
                start_current_visible_tracks(Arc::clone(&state), spotify_api.clone()).await?;
            }
        }
    }

    begin_fast_playback_refresh(&state, Duration::from_secs(4));
    Ok(device_id)
}

fn queue_uris_from_current_track(state: &Arc<Mutex<AppState>>, current_track_uri: &str) -> Vec<String> {
    let guard = state.lock().unwrap();
    let Some(tracks) = guard.playlist_track_cache.get(&guard.active_queue_playlist_uri) else {
        return vec![current_track_uri.to_string()];
    };
    let Some(position) = tracks.iter().position(|track| track.uri == current_track_uri) else {
        return vec![current_track_uri.to_string()];
    };
    tracks
        .iter()
        .skip(position)
        .map(|track| track.uri.clone())
        .collect()
}

async fn apply_volume_change(
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    volume_percent: i32,
) -> Result<()> {
    let volume_percent = volume_percent.clamp(0, 100);
    let (use_local, target_device_id, has_token) = {
        let guard = state.lock().unwrap();
        (
            guard.local_volume_is_target(),
            guard.active_device_id.clone().or_else(|| guard.device_id.clone()),
            guard.token.is_some(),
        )
    };
    if use_local {
        return run_local_player_command(state, |engine| engine.set_volume(volume_percent as u16));
    }
    // Another device is playing: only its volume changes. This PC keeps its own, so
    // turning a speaker down never turns this PC down with it.
    match target_device_id {
        Some(device_id) if has_token => {
            let token = current_token(&state).await?;
            spotify_api.volume(&token, Some(device_id.as_str()), volume_percent).await
        }
        _ => Ok(()),
    }
}

async fn apply_seek_change(
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
    position_ms: i32,
) -> Result<()> {
    let position_ms = position_ms.max(0);
    let (use_local, target_device_id) = {
        let mut guard = state.lock().unwrap();
        // Not playing yet: it starts from there.
        if let Some(parked) = guard.parked.as_mut() {
            parked.position_ms = position_ms;
            return Ok(());
        }
        (
            guard.local_is_active(),
            guard.active_device_id.clone().or_else(|| guard.device_id.clone()),
        )
    };
    if use_local {
        audio_output::begin_skip();
        let result = run_local_player_command(state, |engine| engine.seek(position_ms as u32));
        if result.is_err() {
            audio_output::end_skip();
        }
        return result;
    }
    let Some(device_id) = target_device_id else {
        return Err(anyhow!("nothing is playing"));
    };
    let token = current_token(&state).await?;
    spotify_api.seek(&token, Some(device_id.as_str()), position_ms).await
}

fn run_local_player_command<F>(state: Arc<Mutex<AppState>>, f: F) -> Result<()>
where
    F: FnOnce(&playback::PlaybackEngine) -> Result<()>,
{
    let guard = state.lock().unwrap();
    let engine = guard
        .playback_engine
        .as_ref()
        .ok_or_else(|| anyhow!("local player is not running"))?;
    f(engine)
}

async fn run_local_player_action_with_recovery(
    state: Arc<Mutex<AppState>>,
    ui_weak: slint::Weak<AppWindow>,
    spotify_api: spotify::SpotifyApi,
    action: LocalPlayerAction,
) -> Result<()> {
    if !local_player_is_running(&state) {
        clear_local_playback_engine(&state);
        ensure_local_playback(Arc::clone(&state), ui_weak.clone()).await?;
        wait_for_local_device_refresh(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await;
    }

    let result = run_local_player_command(Arc::clone(&state), |engine| action.run(engine));
    if result.is_ok() || !is_stale_local_player_error(&result) {
        return result;
    }

    clear_local_playback_engine(&state);
    ensure_local_playback(Arc::clone(&state), ui_weak.clone()).await?;
    wait_for_local_device_refresh(ui_weak, Arc::clone(&state), spotify_api).await;
    run_local_player_command(state, |engine| action.run(engine))
}

fn local_player_is_running(state: &Arc<Mutex<AppState>>) -> bool {
    state
        .lock()
        .unwrap()
        .playback_engine
        .as_ref()
        .map(|engine| !engine.is_finished())
        .unwrap_or(false)
}

/// The format librespot will pick for a track, in its own preference order.
/// A track without a 320 kbps file silently plays lower, so this reports what
/// is really being played rather than what the setting asks for.
fn stream_quality_label(available: &[String], audio_quality: &str) -> String {
    let order: &[&str] = match audio_quality {
        "High" => &["OGG_VORBIS_320", "MP3_320", "MP3_256", "OGG_VORBIS_160", "MP3_160", "OGG_VORBIS_96", "MP3_96"],
        "Low" => &["OGG_VORBIS_96", "MP3_96", "OGG_VORBIS_160", "MP3_160", "MP3_256", "OGG_VORBIS_320", "MP3_320"],
        _ => &["OGG_VORBIS_160", "MP3_160", "OGG_VORBIS_96", "MP3_96", "MP3_256", "OGG_VORBIS_320", "MP3_320"],
    };
    order
        .iter()
        .find(|format| available.iter().any(|candidate| candidate == *format))
        .and_then(|format| format.rsplit_once('_'))
        .map(|(codec, kbps)| {
            let codec = if codec.starts_with("OGG") { "Ogg Vorbis" } else { "MP3" };
            format!("{kbps} kbps {codec}")
        })
        .unwrap_or_default()
}

/// Audio settings are fixed when a librespot player is created, so applying a
/// change means replacing the player. If it was playing, music carries on from
/// the same spot and queue.
async fn restart_local_player(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) -> Result<()> {
    let (was_active, current_track, queue_uris, progress_ms, was_playing, old_device_id) = {
        let queue_uris = current_queue_tracks(&state).into_iter().map(|track| track.uri).collect::<Vec<_>>();
        let guard = state.lock().unwrap();
        (
            guard.local_is_active(),
            guard.current_track_uri.clone(),
            queue_uris,
            guard.playback_clock.displayed_progress_ms(),
            guard.playback_clock.is_playing,
            guard.local_device_id.clone(),
        )
    };
    clear_local_playback_engine(&state);
    ensure_local_playback(Arc::clone(&state), ui_weak.clone()).await?;

    // The old player can linger in Spotify's device list under the same name.
    let mut device_id = None;
    for _ in 0..8 {
        wait_for_local_device_refresh(ui_weak.clone(), Arc::clone(&state), spotify_api.clone()).await;
        let local = state.lock().unwrap().local_device_id.clone();
        if local.is_some() && local != old_device_id {
            device_id = local;
            break;
        }
    }
    let (true, Some(track_uri)) = (was_active, current_track) else {
        return Ok(());
    };
    let device_id = device_id.ok_or_else(|| anyhow!("the restarted player did not appear in Spotify"))?;
    let token = current_token(&state).await?;
    let mut uris = vec![track_uri];
    uris.extend(queue_uris);
    spotify_api.play_tracks_at(&token, &device_id, &uris, progress_ms).await?;
    if !was_playing {
        let _ = spotify_api.pause(&token, Some(device_id.as_str())).await;
    }
    Ok(())
}

fn clear_local_playback_engine(state: &Arc<Mutex<AppState>>) {
    let mut guard = state.lock().unwrap();
    let old_local_device_id = guard.local_device_id.clone();
    if let Some(engine) = guard.playback_engine.as_ref() {
        engine.shutdown();
    }
    guard.playback_engine = None;
    guard.playback_engine_generation = guard.playback_engine_generation.wrapping_add(1);
    // The old player's events stop here, its Paused included, so the song stops counting.
    guard.play_watch.stopped();
    guard.latest_local_track_uri = None;
    guard.local_device_id = None;
    if old_local_device_id.is_some() && guard.device_id == old_local_device_id {
        guard.device_id = None;
    }
}

fn is_stale_local_player_error(result: &Result<()>) -> bool {
    result
        .as_ref()
        .err()
        .map(|error| {
            let message = error.to_string();
            message.contains("channel closed") || message.contains("local player is not running")
        })
        .unwrap_or(false)
}

async fn wait_for_local_device_refresh(
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    spotify_api: spotify::SpotifyApi,
) {
    tokio::time::sleep(Duration::from_millis(900)).await;
    let _ = refresh_everything(ui_weak, state, spotify_api, false, true, false).await;
}

/// Errs unless still signed in to the account that was signed in when `session` was read.
fn ensure_same_session(guard: &AppState, session: u64) -> Result<()> {
    if guard.session != session || guard.token.is_none() {
        return Err(anyhow!("not connected to Spotify"));
    }
    Ok(())
}

/// Keeps the player's sign-in, unless the account signed out while it was being fetched.
fn keep_local_playback_token(state: &Arc<Mutex<AppState>>, session: u64, token: TokenStore) -> Result<TokenStore> {
    let mut guard = state.lock().unwrap();
    ensure_same_session(&guard, session)?;
    guard.local_playback_token = Some(token.clone());
    guard.config.local_playback_token = Some(token.clone());
    config::save_config_background(&guard.config);
    Ok(token)
}

async fn ensure_local_playback(
    state: Arc<Mutex<AppState>>,
    ui_weak: slint::Weak<AppWindow>,
) -> Result<()> {
    start_local_playback(state, ui_weak, true).await
}

/// Brings this PC back as a device after its player stopped on its own, as when the
/// connection to Spotify drops while idle or asleep. Tries again with growing waits while
/// there's no network, and gives up only on a change of account or if Spotify wants the
/// browser sign-in, which never pops up unasked.
fn restart_local_player_when_it_stops(
    finished: tokio::sync::oneshot::Receiver<()>,
    state: Arc<Mutex<AppState>>,
    ui_weak: slint::Weak<AppWindow>,
    engine_generation: u64,
    session: u64,
) {
    tokio::spawn(async move {
        // An error only means the player went without saying so; it has stopped all the same.
        let _ = finished.await;
        if state.lock().unwrap().playback_engine_generation != engine_generation {
            return; // Stopped on purpose, by a restart or a sign-out.
        }
        let still_needed = |state: &Arc<Mutex<AppState>>| {
            let guard = state.lock().unwrap();
            ensure_same_session(&guard, session).is_ok()
                && !guard.playback_engine.as_ref().is_some_and(|engine| !engine.is_finished())
        };
        let mut delay = Duration::from_secs(2);
        loop {
            tokio::time::sleep(delay).await;
            if !still_needed(&state) {
                return;
            }
            match start_local_playback(Arc::clone(&state), ui_weak.clone(), false).await {
                Ok(()) => return,
                Err(error) => {
                    let gave_up = error.downcast_ref::<auth::SignInExpired>().is_some();
                    let message = if gave_up {
                        "This PC's player stopped and needs signing in again. Press play here to sign in.".to_string()
                    } else {
                        format!("This PC's player stopped; trying again in a moment ({error}).")
                    };
                    let ui_weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            append_log(&ui, &message);
                        }
                    });
                    if gave_up {
                        return;
                    }
                    delay = (delay * 2).min(Duration::from_secs(300));
                }
            }
        }
    });
}

async fn start_local_playback(
    state: Arc<Mutex<AppState>>,
    ui_weak: slint::Weak<AppWindow>,
    allow_browser_sign_in: bool,
) -> Result<()> {
    let start_lock = state.lock().unwrap().local_playback_start_lock.clone();
    let _start_guard = start_lock.lock().await;
    let session = state.lock().unwrap().session;
    {
        let mut guard = state.lock().unwrap();
        if let Some(engine) = guard.playback_engine.as_ref() {
            if !engine.is_finished() {
                return Ok(());
            }
        }
        let old_local_device_id = guard.local_device_id.clone();
        guard.playback_engine = None;
        guard.playback_engine_generation = guard.playback_engine_generation.wrapping_add(1);
        guard.local_device_id = None;
        if old_local_device_id.is_some() && guard.device_id == old_local_device_id {
            guard.device_id = None;
        }
    }

    let config_snapshot = state.lock().unwrap().config.clone();
    let local_client_id = librespot_core::config::SessionConfig::default().client_id;
    let local_playback_token = {
        let existing = state.lock().unwrap().local_playback_token.clone();
        if let Some(existing) = existing {
            if existing.expires_at_epoch <= config::now_epoch() {
                match auth::refresh_token(&local_client_id, &existing.refresh_token).await {
                    Ok(refreshed) => keep_local_playback_token(&state, session, refreshed)?,
                    // Only a turned-down token needs the browser. No network yet, as just after
                    // the PC wakes, is tried again with the next play.
                    Err(error) if error.downcast_ref::<auth::SignInExpired>().is_none() => return Err(error),
                    Err(error) if !allow_browser_sign_in => return Err(error),
                    Err(_) => {
                        let local_token = auth::login_for_local_playback(&local_client_id).await?;
                        keep_local_playback_token(&state, session, local_token)?
                    }
                }
            } else {
                existing
            }
        } else {
            if !allow_browser_sign_in {
                return Err(auth::SignInExpired::needed().into());
            }
            let local_token = auth::login_for_local_playback(&local_client_id).await?;
            keep_local_playback_token(&state, session, local_token)?
        }
    };

    let startup_volume = if config_snapshot.remember_volume {
        config_snapshot.last_volume_percent.clamp(0, 100)
    } else {
        100
    };
    let mut engine = match playback::PlaybackEngine::start(
        local_playback_token.access_token.clone(),
        local_client_id,
        config_snapshot.device_name.clone(),
        config_snapshot.audio_quality.clone(),
        config_snapshot.normalize_volume,
        startup_volume as u16,
    )
    .await {
        Ok(engine) => engine,
        Err(error) => {
            let message = error.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_connection_status("Connected".into());
                    ui.set_player_status("Local player failed".into());
                    append_log(&ui, &format!("Local playback failed to start: {message}"));
                }
            });
            return Err(error);
        }
    };

    let events = engine.take_player_events();
    let finished = engine.take_finished_signal();
    let engine_generation = {
        let mut guard = state.lock().unwrap();
        // Signed out while the player was starting: it isn't wanted any more.
        if let Err(error) = ensure_same_session(&guard, session) {
            engine.shutdown();
            return Err(error);
        }
        guard.playback_engine_generation = guard.playback_engine_generation.wrapping_add(1);
        let generation = guard.playback_engine_generation;
        guard.playback_engine = Some(engine);
        generation
    };
    if let Some(events) = events {
        tokio::spawn(run_local_player_event_loop(
            events,
            ui_weak.clone(),
            Arc::clone(&state),
            engine_generation,
        ));
    }
    if let Some(finished) = finished {
        restart_local_player_when_it_stops(finished, Arc::clone(&state), ui_weak.clone(), engine_generation, session);
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_connection_status("Connected".into());
            ui.set_player_status("Local player starting".into());
            append_log(&ui, "Local playback engine started. Waiting for Spotify device registration.");
        }
    });

    Ok(())
}

fn full_track_from_audio_item(audio_item: &librespot::metadata::audio::AudioItem) -> FullTrack {
    use librespot::metadata::audio::UniqueFields;

    let (artists, album_name) = match &audio_item.unique_fields {
        UniqueFields::Track { artists, album, .. } => (
            artists
                .iter()
                .map(|artist| crate::models::Artist { id: None, name: artist.name.clone() })
                .collect::<Vec<_>>(),
            album.clone(),
        ),
        UniqueFields::Local { artists, album, .. } => (
            artists
                .iter()
                .filter(|name| !name.is_empty())
                .map(|name| crate::models::Artist { id: None, name: name.clone() })
                .collect::<Vec<_>>(),
            album.clone().unwrap_or_default(),
        ),
        UniqueFields::Episode { show_name, .. } => (
            vec![crate::models::Artist { id: None, name: show_name.clone() }],
            show_name.clone(),
        ),
    };

    FullTrack {
        name: audio_item.name.clone(),
        uri: audio_item.uri.clone(),
        duration_ms: audio_item.duration_ms.min(i32::MAX as u32) as i32,
        album: crate::models::Album {
                id: None,
            name: album_name,
            release_date: None,
            images: audio_item
                .covers
                .iter()
                .map(|cover| crate::models::AlbumImage { url: cover.url.clone() })
                .collect(),
        },
        artists,
    }
}

async fn run_local_player_event_loop(
    mut events: librespot_playback::player::PlayerEventChannel,
    ui_weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    engine_generation: u64,
) {
    use librespot_playback::player::PlayerEvent;

    while let Some(event) = events.recv().await {
        if state.lock().unwrap().playback_engine_generation != engine_generation {
            break;
        }
        // From here on, what the player decodes is from the new place in the music.
        if matches!(event, PlayerEvent::PlayRequestIdChanged { .. } | PlayerEvent::Seeked { .. }) {
            audio_output::end_skip_once_the_player_has_moved();
        }
        match event {
            PlayerEvent::TrackChanged { audio_item } => {
                // AudioItem already contains the metadata needed by the footer.
                // Publishing it directly keeps the event reader independent of
                // Web API and artwork latency.
                let track = full_track_from_audio_item(&audio_item);
                let track_uri = track.uri.clone();
                let available_formats = audio_item.files.0.keys().map(|format| format!("{format:?}")).collect::<Vec<_>>();
                let stream_quality = stream_quality_label(&available_formats, &state.lock().unwrap().config.audio_quality);
                {
                    let mut guard = state.lock().unwrap();
                    // librespot only loads tracks while it is the active device.
                    if guard.local_device_id.is_some() {
                        guard.active_device_id = guard.local_device_id.clone();
                        guard.device_id = guard.local_device_id.clone();
                    }
                    guard.latest_local_track_uri = Some(track_uri.clone());
                    guard.play_watch.song_changed(track.clone());
                    guard.parked = None;
                    guard.missing_playback_since = None;
                    guard.optimistic_track_uri = None;
                    guard.optimistic_track_until = None;
                }
                let art_url = track.album.images.first().map(|image| image.url.clone());
                let ui_state = Arc::clone(&state);
                let ui_weak_track = ui_weak.clone();
                let ui_track_uri = track_uri.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_track.upgrade() {
                        let is_current_event = {
                            let guard = ui_state.lock().unwrap();
                            guard.playback_engine_generation == engine_generation
                                && guard.latest_local_track_uri.as_deref() == Some(ui_track_uri.as_str())
                        };
                        if !is_current_event {
                            return;
                        }
                        set_now_playing_from_track(&ui, &ui_state, &track, NowPlayingUpdate::Authoritative);
                        ui.set_stream_quality(stream_quality.clone().into());
                        repaint_queue(&ui, &ui_state);
                    }
                });
                if let Some(url) = art_url {
                    let ui_weak_art = ui_weak.clone();
                    let ui_state = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Ok(path) = load_album_art(&url).await {
                            let still_current = {
                                let guard = ui_state.lock().unwrap();
                                guard.playback_engine_generation == engine_generation
                                    && guard.current_track_uri.as_deref() == Some(track_uri.as_str())
                            };
                            if !still_current {
                                return;
                            }
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak_art.upgrade() {
                                    let still_current = {
                                        let guard = ui_state.lock().unwrap();
                                        guard.playback_engine_generation == engine_generation
                                            && guard.current_track_uri.as_deref() == Some(track_uri.as_str())
                                    };
                                    if still_current {
                                        set_album_art_if_changed(&ui, &ui_state, Some(path));
                                    }
                                }
                            });
                        }
                    });
                }
            }
            PlayerEvent::Playing { track_id, position_ms, .. } => {
                let track_uri = track_id.to_string();
                watch_play(&state, &track_uri, position_ms);
                apply_local_player_position_event(&ui_weak, &state, track_uri, position_ms as i32, Some(true), engine_generation);
            }
            // Only sent while the song plays.
            PlayerEvent::PositionCorrection { track_id, position_ms, .. }
            | PlayerEvent::PositionChanged { track_id, position_ms, .. } => {
                apply_local_player_position_event(&ui_weak, &state, track_id.to_string(), position_ms as i32, Some(true), engine_generation);
            }
            // Also sent for a seek while paused, which stays paused.
            PlayerEvent::Seeked { track_id, position_ms, .. } => {
                apply_local_player_position_event(&ui_weak, &state, track_id.to_string(), position_ms as i32, None, engine_generation);
            }
            PlayerEvent::Paused { track_id, position_ms, .. } => {
                state.lock().unwrap().play_watch.stopped();
                apply_local_player_position_event(&ui_weak, &state, track_id.to_string(), position_ms as i32, Some(false), engine_generation);
                note_audio_dropouts(&ui_weak);
            }
            PlayerEvent::Stopped { .. } | PlayerEvent::EndOfTrack { .. } | PlayerEvent::Unavailable { .. } => {
                let stopped = matches!(event, PlayerEvent::Stopped { .. });
                let mut guard = state.lock().unwrap();
                guard.play_watch.stopped();
                guard.local_playing = false;
                crash_restart::note_playing_here(false);
                drop(guard);
                if stopped {
                    // Maybe another device took over, which is then shown at once.
                    begin_fast_playback_refresh(&state, Duration::from_secs(4));
                }
                note_audio_dropouts(&ui_weak);
            }
            PlayerEvent::SessionDisconnected { .. } => begin_fast_playback_refresh(&state, Duration::from_secs(4)),
            PlayerEvent::VolumeChanged { volume } => {
                // Set from a phone or another Spotify app, or our own change coming back.
                state.lock().unwrap().local_volume_percent = playback::spotify_volume_to_percent(volume);
                let ui_weak = ui_weak.clone();
                let state = Arc::clone(&state);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        show_local_volume(&ui, &state);
                    }
                });
            }
            _ => {}
        }
    }
}

/// Puts this PC's own volume on the slider while it's the one the slider controls,
/// unless the user moved the slider a moment ago and the player hasn't caught up.
fn show_local_volume(ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let volume = {
        let guard = state.lock().unwrap();
        let recently_changed = guard.last_volume_change.is_some_and(|at| at.elapsed() < Duration::from_secs(3));
        if recently_changed || !guard.local_volume_is_target() {
            return;
        }
        guard.local_volume_percent
    };
    if ui.get_volume_percent() != volume {
        ui.set_volume_percent(volume);
        ui.set_muted(volume == 0);
    }
}

/// Adds the song this PC is playing to Recently played once it has played long enough.
/// Spotify doesn't count what librespot plays, so Spotlight keeps that list itself.
fn watch_play(state: &Arc<Mutex<AppState>>, track_uri: &str, position_ms: u32) {
    let Some((wait, generation)) = state.lock().unwrap().play_watch.playing(track_uri, position_ms) else { return };
    let state = Arc::clone(state);
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        let (track, session) = {
            let mut guard = state.lock().unwrap();
            let Some(track) = guard.play_watch.counts(generation) else { return };
            (track, guard.session)
        };
        let play = history::Play { at_ms: history::now_ms(), track };
        // Asked as it's written, so an account that became known just before has claimed the
        // plays waiting for it. One that signed out since gets nothing.
        let whose = move || {
            let guard = state.lock().unwrap();
            (guard.session == session).then(|| guard.config.account_id.clone())
        };
        let _ = tokio::task::spawn_blocking(move || history::record(whose, play)).await;
    });
}

/// Notes in the log when the audio ran out since the last note, which is heard as a click or a
/// gap. Each time, Windows is kept a little more ahead, so it shouldn't happen again soon.
fn note_audio_dropouts(ui_weak: &slint::Weak<AppWindow>) {
    static NOTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let total = wasapi::UNDERRUNS.load(std::sync::atomic::Ordering::Relaxed);
    let new = total.saturating_sub(NOTED.swap(total, std::sync::atomic::Ordering::Relaxed));
    if new == 0 {
        return;
    }
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let times = if new == 1 { "once".to_string() } else { format!("{new} times") };
            write_log(&ui, &format!("The audio ran out {times} while playing, so Spotlight now keeps a little more ready."));
        }
    });
}

fn apply_local_player_position_event(
    ui_weak: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppState>>,
    track_uri: String,
    position_ms: i32,
    // `None` leaves playing or paused as it was.
    is_playing: Option<bool>,
    engine_generation: u64,
) {
    let is_playing = {
        let mut guard = state.lock().unwrap();
        if guard.playback_engine_generation != engine_generation {
            return;
        }
        let matches_current = guard.current_track_uri.as_deref() == Some(track_uri.as_str())
            || guard.optimistic_track_uri.as_deref() == Some(track_uri.as_str())
            || guard.latest_local_track_uri.as_deref() == Some(track_uri.as_str());
        if !matches_current {
            return;
        }
        let is_playing = is_playing.unwrap_or(guard.local_playing);
        guard.local_playing = is_playing;
        crash_restart::note_playing_here(is_playing);
        if is_playing && guard.local_device_id.is_some() {
            guard.active_device_id = guard.local_device_id.clone();
            guard.device_id = guard.local_device_id.clone();
        }
        let duration = guard.playback_clock.duration_ms;
        guard.playback_clock.sync(position_ms, duration, is_playing);
        is_playing
    };
    let ui_weak = ui_weak.clone();
    let state = Arc::clone(state);
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            if state.lock().unwrap().playback_engine_generation != engine_generation {
                return;
            }
            ui.set_is_playing(is_playing);
            if is_playing {
                // Back from another device, the slider shows this PC's volume again.
                show_local_volume(&ui, &state);
            }
            show_progress(&ui, position_ms);
            ui.set_connection_status(if is_playing {
                "Connected - Playing".into()
            } else {
                "Connected - Paused".into()
            });
            // A seek moves nothing else Windows hears about.
            now_playing_changed(&ui, &state);
        }
    });
}

thread_local! {
    static NOW_PLAYING_TIMER: slint::Timer = slint::Timer::default();
}

/// Tells Windows' media controls and the taskbar's buttons about the song. Changes come in
/// bursts, such as the title, the artists and the cover, so this waits for the last of them.
fn now_playing_changed(ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let ui_weak = ui.as_weak();
    let state = Arc::clone(state);
    NOW_PLAYING_TIMER.with(|timer| {
        timer.start(slint::TimerMode::SingleShot, Duration::from_millis(40), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let has_song = !ui.get_current_track_uri().is_empty() && ui.get_track_title() != "Nothing playing";
            let playing = has_song && ui.get_is_playing();
            let (position, art_url) = {
                let guard = state.lock().unwrap();
                (guard.playback_clock.displayed_progress_ms(), guard.current_album_art_url.clone())
            };
            let song = has_song.then(|| media_controls::Song {
                title: ui.get_track_title().to_string(),
                artist: ui.get_track_artists().iter().filter(|name| !name.is_empty()).map(|name| name.to_string()).collect::<Vec<_>>().join(", "),
                album: ui.get_track_album().to_string(),
                art_url,
                duration_ms: ui.get_progress_max_ms(),
            });
            media_controls::show(song.as_ref(), playing, position);
            taskbar::show_playing(has_song, playing);
            note_left_off(&ui, &state);
        });
    });
}

/// Where the music is now, to pick up from next time. Nothing while nothing is in the player bar,
/// so the last place stays.
fn left_off_now(state: &Arc<Mutex<AppState>>) -> Option<left_off::LeftOff> {
    let upcoming = current_queue_tracks(state);
    let guard = state.lock().ok()?;
    let (shuffle, repeat) = guard.left_off_modes.clone();
    if let Some(parked) = guard.parked.as_ref() {
        // Not played since Spotlight opened, though Next up or the place in the song may have
        // been changed. Its place stays put while it's starting, in case it doesn't.
        return Some(left_off::LeftOff { upcoming, shuffle, repeat, ..parked.clone() });
    }
    let position_ms = guard.playback_clock.displayed_progress_ms().max(0);
    let track = guard.now_playing.clone().filter(|track| guard.current_track_uri.as_deref() == Some(track.uri.as_str()))?;
    let list_uri = guard.active_queue_playlist_uri.clone();
    let rest = match guard.playlist_track_cache.get(&list_uri) {
        // Search results and pages like an album's start one song, not their list.
        Some(list) if list_uri != SEARCH_RESULTS_URI => left_off::rest_of_list(list, &track.uri, &upcoming, shuffle),
        _ => Vec::new(),
    };
    Some(left_off::LeftOff { track, position_ms, upcoming, rest, list_uri, shuffle, repeat })
}

/// Saves where the music is, as the player bar shows it.
fn note_left_off(ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    state.lock().unwrap().left_off_modes = (ui.get_shuffle_enabled(), ui.get_repeat_mode().to_string());
    if let Some(left_off) = left_off_now(state) {
        left_off::save_in_background(left_off);
    }
}

/// Puts the song from when Spotlight last closed back in the player bar, paused where it was,
/// with Next up, shuffle and repeat as they were. Playing it carries on from there.
fn show_left_off(ui: &AppWindow, state: &Arc<Mutex<AppState>>, left_off: left_off::LeftOff) {
    let track = left_off.track.clone();
    let position = left_off.position_ms.clamp(0, track.duration_ms.max(0));
    let repeat = if left_off.repeat.is_empty() { "off".to_string() } else { left_off.repeat.clone() };
    let art_url = track.album.images.first().map(|image| image.url.clone());
    let shuffle = left_off.shuffle;
    {
        let mut guard = state.lock().unwrap();
        guard.now_playing = Some(track.clone());
        guard.current_track_uri = Some(track.uri.clone());
        guard.playback_clock.sync(position, track.duration_ms, false);
        guard.queue_override_tracks = left_off.upcoming.clone();
        guard.queue_override_active = true;
        if !left_off.list_uri.is_empty() {
            guard.active_queue_playlist_uri = left_off.list_uri.clone();
        }
        guard.current_album_art_url = art_url.clone();
        guard.left_off_modes = (shuffle, repeat.clone());
        guard.parked = Some(left_off::LeftOff { position_ms: position, ..left_off });
    }
    ui.set_current_track_uri(track.uri.clone().into());
    ui.set_track_title(track.name.clone().into());
    show_track_artists(ui, &track.artists);
    ui.set_track_album(track.album.name.clone().into());
    ui.set_track_release_date(track.album.release_date.clone().unwrap_or_default().into());
    ui.set_progress_max_ms(track.duration_ms.max(1));
    ui.set_duration(format_time(track.duration_ms).into());
    show_progress(ui, position);
    ui.set_is_playing(false);
    ui.set_shuffle_enabled(shuffle);
    ui.set_repeat_mode(repeat.into());
    set_album_art_if_changed(ui, state, art_url.as_deref().and_then(cached_album_art_path));
    repaint_queue(ui, state);
    now_playing_changed(ui, state);
}

/// Where a song that was left carries on from.
#[derive(Clone, Copy)]
enum PickUp {
    WhereItWas,
    FromTheStart,
    NextSong,
}

/// Plays the music that was left when Spotlight last closed, on this PC.
fn pick_up_left_off(ui_weak: &slint::Weak<AppWindow>, runtime: &Arc<Runtime>, state: &Arc<Mutex<AppState>>, from: PickUp) {
    /// Long enough for the player to start and sign in, short enough that the bar doesn't claim
    /// to play for long when it can't.
    const GIVE_UP_AFTER: Duration = Duration::from_secs(15);
    let Some(ui) = ui_weak.upgrade() else { return };
    let started = Instant::now();
    {
        let mut guard = state.lock().unwrap();
        // It's on its way: another press would only start it twice.
        if guard.picking_up_since.is_some_and(|since| since.elapsed() < GIVE_UP_AFTER) {
            return;
        }
        let Some(parked) = guard.parked.as_ref() else { return };
        let position = match from {
            PickUp::WhereItWas => parked.position_ms,
            PickUp::FromTheStart | PickUp::NextSong => 0,
        };
        guard.picking_up_since = Some(started);
        let duration_ms = guard.playback_clock.duration_ms;
        guard.playback_clock.sync(position, duration_ms, true);
        drop(guard);
        ui.set_is_playing(true);
        ui.set_connection_status("Connected - Playing".into());
        show_progress(&ui, position);
    }

    // The player says when the song starts, which ends this. Should it never say, the bar goes
    // back to how it was rather than claim to play.
    let ui_weak = ui_weak.clone();
    let state = Arc::clone(state);
    {
        let ui_weak = ui_weak.clone();
        let state = Arc::clone(&state);
        runtime.spawn(async move {
            tokio::time::sleep(GIVE_UP_AFTER).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    put_back_left_off(&ui, &state, started);
                }
            });
        });
    }
    runtime.spawn(async move {
        let result = resume_left_off(ui_weak.clone(), Arc::clone(&state), from).await;
        if result.is_ok() {
            begin_fast_playback_refresh(&state, Duration::from_secs(4));
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            put_back_left_off(&ui, &state, started);
            if let Err(error) = result {
                append_log(&ui, &format!("Playback command failed: {error}"));
            }
        });
    });
}

/// Shows the song paused where it was again, after pressing play on it didn't play it.
fn put_back_left_off(ui: &AppWindow, state: &Arc<Mutex<AppState>>, started: Instant) {
    let mut guard = state.lock().unwrap();
    if guard.picking_up_since != Some(started) {
        return;
    }
    guard.picking_up_since = None;
    let Some(position) = guard.parked.as_ref().map(|parked| parked.position_ms) else { return };
    let duration_ms = guard.playback_clock.duration_ms;
    guard.playback_clock.sync(position, duration_ms, false);
    drop(guard);
    ui.set_is_playing(false);
    ui.set_connection_status("Connected - Paused".into());
    show_progress(ui, position);
}

async fn resume_left_off(ui_weak: slint::Weak<AppWindow>, state: Arc<Mutex<AppState>>, from: PickUp) -> Result<()> {
    let upcoming = current_queue_tracks(&state);
    let (left_off, position, (shuffle, repeat)) = {
        let guard = state.lock().unwrap();
        let left_off = guard.parked.clone().ok_or_else(|| anyhow!("nothing to carry on with"))?;
        let position = left_off.position_ms.max(0) as u32;
        (left_off, position, guard.left_off_modes.clone())
    };
    let mut uris = left_off::play_order(&left_off, &upcoming);
    let mut shown = track_uris(&upcoming);
    let position = match from {
        PickUp::WhereItWas => position,
        PickUp::FromTheStart => 0,
        PickUp::NextSong => {
            uris.remove(0);
            if !shown.is_empty() {
                shown.remove(0);
            }
            0
        }
    };
    if uris.is_empty() {
        return Err(anyhow!("nothing comes next"));
    }
    if !local_player_is_running(&state) {
        clear_local_playback_engine(&state);
        ensure_local_playback(Arc::clone(&state), ui_weak).await?;
    }
    audio_output::set_paused(false);
    run_local_player_command(state, |engine| engine.resume_list(uris, position, shuffle, &repeat, shown))
}

/// The lyrics in the panel, for which song, and where the song was when last heard of.
/// Lives on the UI thread, like the model it fills.
#[derive(Default)]
struct LyricsView {
    uri: String,
    lyrics: Option<lyrics::Lyrics>,
    generation: u64,
    /// The position then, when that was, and whether the song was playing.
    heard: Option<(u32, Instant, bool)>,
    /// Goes off as the next line starts. Nothing runs between lines.
    next_line: slint::Timer,
}

thread_local! {
    static LYRICS_VIEW: RefCell<LyricsView> = RefCell::new(LyricsView::default());
}

/// Loads the lyrics for the song playing, unless they're already shown or on the way.
fn show_lyrics(ui: &AppWindow, runtime: &Runtime, state: &Arc<Mutex<AppState>>) {
    let uri = ui.get_current_track_uri().to_string();
    let generation = LYRICS_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        if view.uri == uri && ui.get_lyrics_state() != "error" {
            return None;
        }
        view.uri = uri.clone();
        view.lyrics = None;
        view.next_line.stop();
        view.generation = view.generation.wrapping_add(1);
        Some(view.generation)
    });
    let Some(generation) = generation else {
        sync_lyrics(ui, None);
        return;
    };
    ui.set_lyrics_lines(ModelRc::default());
    ui.set_lyrics_line(-1);
    ui.set_lyrics_provider("".into());
    if !uri.starts_with("spotify:track:") {
        ui.set_lyrics_state("none".into());
        return;
    }
    ui.set_lyrics_state("loading".into());
    let ui_weak = ui.as_weak();
    let state = Arc::clone(state);
    runtime.spawn(async move {
        let result = match spotify_session(&state).await {
            Ok(session) => lyrics::fetch(&session, &uri).await,
            Err(error) => Err(error),
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let current = LYRICS_VIEW.with(|view| view.borrow().generation == generation);
            if !current {
                return;
            }
            match result {
                Ok(Some(found)) => {
                    let lines: Vec<LyricLine> = found
                        .lines
                        .iter()
                        .map(|line| LyricLine { words: line.words.as_str().into(), start_ms: line.start_ms.min(i32::MAX as u32) as i32 })
                        .collect();
                    ui.set_lyrics_lines(ModelRc::from(Rc::new(VecModel::from(lines))));
                    ui.set_lyrics_provider(found.provider.as_str().into());
                    ui.set_lyrics_state(if found.synced { "synced" } else { "unsynced" }.into());
                    LYRICS_VIEW.with(|view| view.borrow_mut().lyrics = Some(found));
                    let position = state.lock().unwrap().playback_clock.displayed_progress_ms();
                    sync_lyrics(&ui, Some((position, ui.get_is_playing())));
                }
                Ok(None) => ui.set_lyrics_state("none".into()),
                Err(error) => {
                    append_log(&ui, &format!("Lyrics failed: {error}"));
                    ui.set_lyrics_state("error".into());
                }
            }
        });
    });
}

/// Lights the line being sung, and sets a timer for when the next one starts. Given where the
/// song is when that's known, or works it out from when it was last heard of.
fn sync_lyrics(ui: &AppWindow, heard: Option<(i32, bool)>) {
    LYRICS_VIEW.with(|cell| {
        let mut view = cell.borrow_mut();
        if let Some((position, playing)) = heard {
            view.heard = Some((position.max(0) as u32, Instant::now(), playing));
        }
        let Some(lyrics) = view.lyrics.as_ref().filter(|lyrics| lyrics.synced) else { return };
        let position = match view.heard {
            Some((at, when, true)) => at.saturating_add(when.elapsed().as_millis().min(u32::MAX as u128) as u32),
            Some((at, _, false)) => at,
            None => 0,
        };
        let line = lyrics.line_at(position).map_or(-1, |line| line as i32);
        if ui.get_lyrics_line() != line {
            ui.set_lyrics_line(line);
        }
        let playing = view.heard.is_some_and(|(_, _, playing)| playing);
        // Nothing to light while it can't be seen or the song is paused.
        let wait = lyrics
            .next_start_after(position)
            .filter(|_| playing && ui.get_lyrics_open() && !ui.get_mini_mode() && !ui.get_window_hidden())
            .map(|start| Duration::from_millis(u64::from(start - position)));
        match wait {
            Some(wait) => {
                let ui_weak = ui.as_weak();
                view.next_line.start(slint::TimerMode::SingleShot, wait, move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        sync_lyrics(&ui, None);
                    }
                });
            }
            None => view.next_line.stop(),
        }
    });
}

/// Moves the progress bar and the time beside it, unless the window is minimized or in the
/// tray. Each frame drawn then would be thrown away, and the bar catches up from the playback
/// clock when the window is shown again.
fn show_progress(ui: &AppWindow, progress_ms: i32) {
    if ui.get_window_hidden() {
        return;
    }
    ui.set_progress_ms(progress_ms);
    ui.set_elapsed(format_time(progress_ms).into());
    sync_lyrics(ui, Some((progress_ms, ui.get_is_playing())));
}

/// What's wrong with a Client ID as typed, for the setup guide to say. Nothing while it's
/// empty, or once it has the shape of one: 32 letters and digits from 0-9 and a-f.
fn client_id_problem(text: &str) -> String {
    let id = text.trim();
    if id.is_empty() || (id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())) {
        return String::new();
    }
    "A Client ID is 32 letters and numbers. It's on your app's Settings page.".to_string()
}

/// An error as a sentence: a capital at the start and a full stop at the end.
fn as_sentence(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return String::new() };
    let mut sentence: String = first.to_uppercase().chain(chars).collect();
    if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    sentence
}

fn format_time(ms: i32) -> String {
    let secs = ms.max(0) / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn cycle_repeat_mode(current: &str) -> String {
    match current {
        "off" => "context".to_string(),
        "context" => "track".to_string(),
        _ => "off".to_string(),
    }
}

fn hotkey_bindings(config: &AppConfig) -> [String; 5] {
    [
        config.play_pause_hotkey.clone(),
        config.next_hotkey.clone(),
        config.previous_hotkey.clone(),
        config.shuffle_hotkey.clone(),
        config.repeat_hotkey.clone(),
    ]
}

/// Each shortcut in Settings: its action, the name of its row, and its keys.
fn shortcut_settings(ui: &AppWindow) -> [(&'static str, &'static str, SharedString); 5] {
    [
        ("play-pause", "Play or pause", ui.get_play_pause_hotkey()),
        ("next", "Next song", ui.get_next_hotkey()),
        ("previous", "Previous song", ui.get_previous_hotkey()),
        ("shuffle", "Shuffle", ui.get_shuffle_hotkey()),
        ("repeat", "Repeat", ui.get_repeat_hotkey()),
    ]
}

/// Ends listening in a shortcut box. `resume` brings back the shortcuts paused for it, unless
/// saving is about to register them all again.
fn stop_listening_for_shortcut(ui: &AppWindow, hotkeys_guard: &Mutex<Option<MediaKeyGuard>>, resume: bool) {
    ui.set_recording_shortcut("".into());
    ui.set_shortcut_keys("".into());
    ui.set_shortcut_refused(false);
    window_chrome::listen_for_shortcut(false);
    if resume {
        if let Some(guard) = hotkeys_guard.lock().unwrap().as_ref() {
            guard.resume_custom();
        }
    }
}

fn cycle_binding(current: &str, primary: &str, alternate: &str) -> String {
    if current == primary { alternate.to_string() } else { primary.to_string() }
}

fn append_log(ui: &AppWindow, message: &str) {
    let lower = message.to_ascii_lowercase();
    let background_noise = lower.contains("refresh failed") || lower.contains("startup");
    if (lower.contains("failed") && !background_noise) || lower.starts_with("no results") {
        // Strip noisy prefixes so the toast stays short.
        let short = message.split(": spotify request failed").next().unwrap_or(message);
        show_toast(ui, short, lower.contains("failed"));
    } else if message.starts_with("Added") || message.starts_with("Playing next") {
        show_toast(ui, message, false);
    }
    write_log(ui, message);
}

/// Adds a line to the log in Settings, with no toast: for failures already shown where they
/// happened, or not worth interrupting anyone for.
fn write_log(ui: &AppWindow, message: &str) {
    if !should_show_log(ui.get_log_level().as_str(), message) {
        return;
    }
    let current = ui
        .get_status_log()
        .lines()
        .take(99)
        .collect::<Vec<_>>()
        .join("\n");
    let now = chrono_like_time();
    let next = format!("[{}] {}\n{}", now, message, current);
    ui.set_status_log(next.into());
}

fn should_show_log(level: &str, message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let is_error = lower.contains("failed") || lower.contains("error") || lower.contains("expired");
    let is_warning = is_error || lower.contains("rate") || lower.contains("retry") || lower.contains("not found");
    match level {
        "Error" => is_error,
        "Warn" => is_warning,
        _ => true,
    }
}

/// The time of day on this PC's clock, for the log.
fn chrono_like_time() -> String {
    let mut now: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut now) };
    format!("{:02}:{:02}:{:02}", now.wHour, now.wMinute, now.wSecond)
}

#[cfg(target_os = "windows")]
fn spotlight_window_is_foreground() -> bool {
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic, IsWindowVisible},
    };

    let hwnd: HWND = window_chrome::handle();
    if hwnd.is_null() {
        return true;
    }
    if unsafe { IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 } {
        return false;
    }
    let foreground = unsafe { GetForegroundWindow() };
    foreground == hwnd
}

#[cfg(not(target_os = "windows"))]
fn spotlight_window_is_foreground() -> bool {
    true
}

static SPOTLIGHT_WINDOW_MAXIMIZED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
fn toggle_spotlight_window_maximized() {
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{ShowWindow, SW_MAXIMIZE, SW_RESTORE},
    };

    let hwnd: HWND = window_chrome::handle();
    if hwnd.is_null() || window_chrome::in_mini() {
        return;
    }

    let was_maximized = SPOTLIGHT_WINDOW_MAXIMIZED.fetch_xor(true, Ordering::Relaxed);
    let command = if was_maximized { SW_RESTORE } else { SW_MAXIMIZE };
    unsafe {
        ShowWindow(hwnd, command);
    }
}

#[cfg(not(target_os = "windows"))]
fn toggle_spotlight_window_maximized() {}

#[cfg(target_os = "windows")]
fn apply_runtime_window_icon() {
    std::thread::spawn(|| {
        use std::{iter, thread, time::Duration};
        use windows_sys::Win32::{
            Foundation::HWND,
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                FindWindowW, LoadIconW, SendMessageW, ICON_BIG, ICON_SMALL, WM_SETICON,
            },
        };

        let title: Vec<u16> = "Spotlight".encode_utf16().chain(iter::once(0)).collect();
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(200));
            let hwnd: HWND = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
            if hwnd.is_null() {
                continue;
            }
            let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
            let hicon = unsafe { LoadIconW(hinstance, 1 as _) };
            if !hicon.is_null() {
                unsafe {
                    SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, hicon as isize);
                    SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, hicon as isize);
                }
                break;
            }
        }
    });
}

#[cfg(test)]
mod queue_edit_tests {
    use super::*;

    fn track(uri: &str) -> FullTrack {
        serde_json::from_value(serde_json::json!({
            "name": uri,
            "uri": uri,
            "duration_ms": 1000,
            "album": { "name": "", "images": [] },
            "artists": [],
        }))
        .unwrap()
    }

    fn tracks(uris: &[&str]) -> Vec<FullTrack> {
        uris.iter().map(|uri| track(uri)).collect()
    }

    fn painted(uris: &[&str]) -> Vec<String> {
        uris.iter().map(|uri| uri.to_string()).collect()
    }

    fn uris(tracks: &[FullTrack]) -> Vec<&str> {
        tracks.iter().map(|track| track.uri.as_str()).collect()
    }

    #[test]
    fn dragging_moves_a_song_both_ways() {
        let rows = painted(&["a", "b", "c", "d"]);
        let mut list = tracks(&["a", "b", "c", "d"]);
        move_dragged_track(&mut list, &rows, 0, 2);
        assert_eq!(uris(&list), ["b", "c", "a", "d"]);
        let mut list = tracks(&["a", "b", "c", "d"]);
        move_dragged_track(&mut list, &rows, 3, 1);
        assert_eq!(uris(&list), ["a", "d", "b", "c"]);
    }

    #[test]
    fn dragging_to_the_last_row_stays_above_songs_not_on_screen() {
        let mut list = tracks(&["a", "b", "c", "later"]);
        move_dragged_track(&mut list, &painted(&["a", "b", "c"]), 0, 2);
        assert_eq!(uris(&list), ["b", "c", "a", "later"]);
    }

    #[test]
    fn dragging_the_right_copy_of_a_song_queued_twice() {
        let mut list = tracks(&["a", "b", "a", "c"]);
        move_dragged_track(&mut list, &painted(&["a", "b", "a", "c"]), 2, 0);
        assert_eq!(uris(&list), ["a", "a", "b", "c"]);
        let mut list = tracks(&["a", "b", "a", "c"]);
        move_dragged_track(&mut list, &painted(&["a", "b", "a", "c"]), 2, 3);
        assert_eq!(uris(&list), ["a", "b", "c", "a"]);
    }

    #[test]
    fn a_song_ending_during_a_drag_still_lands_below_the_same_neighbour() {
        // "a" started playing mid-drag, so it left the list.
        let rows = painted(&["a", "b", "c", "d"]);
        let mut list = tracks(&["b", "c", "d"]);
        move_dragged_track(&mut list, &rows, 3, 2);
        assert_eq!(uris(&list), ["b", "d", "c"]);
        let mut list = tracks(&["b", "c", "d"]);
        move_dragged_track(&mut list, &rows, 3, 0);
        assert_eq!(uris(&list), ["d", "b", "c"]);
    }

    #[test]
    fn a_dragged_song_that_has_gone_changes_nothing() {
        let mut list = tracks(&["b", "c"]);
        move_dragged_track(&mut list, &painted(&["a", "b", "c"]), 0, 2);
        assert_eq!(uris(&list), ["b", "c"]);
    }

    #[test]
    fn removing_takes_the_row_clicked_even_if_the_list_moved_on() {
        let rows = painted(&["a", "b", "c"]);
        let mut list = tracks(&["a", "b", "c"]);
        remove_painted_track(&mut list, &rows, 1);
        assert_eq!(uris(&list), ["a", "c"]);
        let mut list = tracks(&["b", "c"]);
        remove_painted_track(&mut list, &rows, 2);
        assert_eq!(uris(&list), ["b"]);
    }

    #[test]
    fn editing_spotifys_rows_keeps_the_list_behind_them() {
        let shown = tracks(&["b", "c"]);
        assert_eq!(uris(&queue_to_edit(&shown, &tracks(&["b", "c", "d"]), &[], false)), ["b", "c", "d"]);
    }

    #[test]
    fn editing_spotifys_rows_carries_on_with_the_playlist_otherwise() {
        let shown = tracks(&["x", "c"]);
        let playlist = tracks(&["a", "b", "c", "d", "e"]);
        assert_eq!(uris(&queue_to_edit(&shown, &tracks(&["q"]), &playlist, false)), ["x", "c", "d", "e"]);
    }

    #[test]
    fn editing_rows_from_outside_the_playlist_keeps_just_them() {
        let shown = tracks(&["x", "y"]);
        assert_eq!(uris(&queue_to_edit(&shown, &[], &tracks(&["a"]), false)), ["x", "y"]);
    }

    #[test]
    fn spotifys_list_replaces_ours_only_when_they_differ() {
        let ours = tracks(&["a", "b", "c"]);
        assert!(!spotify_queue_replaces_ours(&tracks(&["a", "b"]), &ours, false), "the start of ours");
        assert!(spotify_queue_replaces_ours(&tracks(&["x", "a"]), &ours, false), "someone added a song");
        assert!(spotify_queue_replaces_ours(&tracks(&["a", "b", "c", "d"]), &ours, false), "ours has run out");
        assert!(spotify_queue_replaces_ours(&[], &ours, false), "nothing is coming up");
    }

    #[test]
    fn spotifys_list_waits_until_it_can_have_heard_of_an_edit() {
        assert!(!spotify_queue_replaces_ours(&tracks(&["x"]), &tracks(&["a"]), true));
        assert!(!spotify_queue_replaces_ours(&[], &tracks(&["a"]), true));
    }

    #[test]
    fn the_list_to_edit_never_grows_past_the_limit() {
        let playlist: Vec<FullTrack> = (0..200).map(|n| track(&format!("t{n}"))).collect();
        assert_eq!(queue_to_edit(&playlist[..1], &[], &playlist, false).len(), MAX_LOCAL_QUEUE_TRACKS);
    }

    #[test]
    fn editing_in_shuffle_leaves_the_playlists_order_out_of_it() {
        let shown = tracks(&["d", "b"]);
        let playlist = tracks(&["a", "b", "c", "d", "e"]);
        assert_eq!(uris(&queue_to_edit(&shown, &[], &playlist, true)), ["d", "b"]);
    }

    #[test]
    fn editing_again_in_shuffle_keeps_the_list_already_held() {
        let shown = tracks(&["d", "b"]);
        assert_eq!(uris(&queue_to_edit(&shown, &tracks(&["d", "b", "n"]), &[], true)), ["d", "b", "n"]);
    }
}

#[cfg(test)]
mod device_and_queue_tests {
    use super::{pick_start_device, picker_matches, queue_tail_start, uris_from_queue_click};

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn music_stays_on_the_current_device() {
        let (desktop, local, phone) = ("desktop".to_string(), "local".to_string(), "phone".to_string());
        let available = ids(&["desktop", "local", "phone"]);
        assert_eq!(pick_start_device(Some(&desktop), Some(&phone), &available), Some(desktop.clone()), "what is playing wins");
        assert_eq!(pick_start_device(None, Some(&phone), &available), Some(phone.clone()), "paused long enough for the session to lapse");
        assert_eq!(pick_start_device(None, Some(&local), &available), Some(local));
        assert_eq!(pick_start_device(None, Some(&phone), &ids(&["desktop"])), None, "last device has gone");
        assert_eq!(pick_start_device(None, None, &available), None);
    }

    #[test]
    fn clicking_a_queue_row_plays_on_from_there() {
        let queue = ids(&["b", "c", "d"]);
        let source = ids(&["a", "b", "c", "d", "e", "f"]);
        assert_eq!(uris_from_queue_click(&queue, 1, &source), ids(&["c", "d", "e", "f"]));
        assert_eq!(uris_from_queue_click(&queue, 2, &source), ids(&["d", "e", "f"]));
        assert_eq!(uris_from_queue_click(&queue, 1, &[]), ids(&["c", "d"]), "queue from another app");
        assert!(uris_from_queue_click(&queue, 3, &source).is_empty());
    }

    #[test]
    fn the_queue_carries_on_from_the_new_song() {
        let queue = ["b", "c", "d"];
        assert_eq!(queue_tail_start(queue.into_iter(), "b"), Some(1), "skipped to the next song");
        assert_eq!(queue_tail_start(queue.into_iter(), "c"), Some(2), "skipped two");
        assert_eq!(queue_tail_start(queue.into_iter(), "d"), Some(3), "last one: nothing left");
        assert_eq!(queue_tail_start(queue.into_iter(), "z"), None, "something else started");
    }

    #[test]
    fn the_picker_never_offers_liked_songs_and_ignores_case() {
        let titles = ["Liked Songs", "Deep House", "gym", "House Party"];
        assert_eq!(picker_matches(titles.into_iter(), ""), vec![1, 2, 3]);
        assert_eq!(picker_matches(titles.into_iter(), "  HOUSE "), vec![1, 3]);
        assert_eq!(picker_matches(titles.into_iter(), "liked"), Vec::<i32>::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guide_only_questions_a_client_id_that_cant_be_one() {
        assert_eq!(client_id_problem(""), "");
        assert_eq!(client_id_problem("0123456789abcdef0123456789ABCDEF"), "");
        assert_eq!(client_id_problem("  0123456789abcdef0123456789abcdef\n"), "", "pasted with a line break");
        assert_ne!(client_id_problem("0123456789abcdef"), "", "too short");
        assert_ne!(client_id_problem("http://127.0.0.1:5543/callback/"), "", "the redirect URI by mistake");
        assert_ne!(client_id_problem("0123456789abcdef0123456789abcdeg"), "", "not hex");
    }

    #[test]
    fn album_tints_come_out_evenly_bright() {
        let luminance = |(r, g, b): (f32, f32, f32)| {
            let linear = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        };
        let yellow = hsl_to_rgb(1.0 / 6.0, 0.48, 0.22);
        let blue = hsl_to_rgb(2.0 / 3.0, 0.48, 0.22);
        assert!(luminance(yellow) > 0.07, "the problem this is for");
        assert!(luminance(even_brightness(yellow, false)) <= 0.0401);
        assert_eq!(even_brightness(blue, false), blue, "already dark enough");
        let pale_blue = hsl_to_rgb(2.0 / 3.0, 0.55, 0.86);
        assert!(luminance(even_brightness(pale_blue, true)) >= 0.719);
        let pale_yellow = hsl_to_rgb(1.0 / 6.0, 0.55, 0.86);
        assert_eq!(even_brightness(pale_yellow, true), pale_yellow, "already light enough");
        // The hue stays: yellow is still more red and green than blue.
        let (r, g, b) = even_brightness(yellow, false);
        assert!(r > b * 1.5 && g > b * 1.5);
    }

    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(as_sentence("you didn't agree in the browser"), "You didn't agree in the browser.");
        assert_eq!(as_sentence("Already one."), "Already one.");
        assert_eq!(as_sentence(""), "");
    }

    #[test]
    fn crash_log_times_are_calendar_dates_in_utc() {
        let at = |seconds: u64| utc_timestamp(std::time::UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(at(0), "1970-01-01 00:00:00");
        // The crash this log was added for.
        assert_eq!(at(1_794_251_288), "2026-11-09 19:08:08");
        assert_eq!(at(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(at(4_107_542_399), "2100-02-28 23:59:59");
    }

    #[test]
    fn each_artist_link_opens_that_artist_not_the_first() {
        let artist = |id: &str, name: &str| models::Artist { id: Some(id.to_string()), name: name.to_string() };
        let song = || vec![artist("leon", "Leon Thomas"), artist("masego", "Masego")];
        assert_eq!(clicked_artist(song(), "Masego").id.as_deref(), Some("masego"));
        assert_eq!(clicked_artist(song(), "Leon Thomas").id.as_deref(), Some("leon"));
        // Spelled differently in Spotify's details: still the name that was clicked.
        let unknown = clicked_artist(song(), "MASEGO");
        assert_eq!((unknown.id, unknown.name.as_str()), (None, "MASEGO"));
    }

    #[test]
    fn pending_track_blocks_different_observation_only_until_deadline() {
        let now = Instant::now();
        assert!(pending_track_blocks_observation(
            Some("spotify:track:expected"),
            Some(now + Duration::from_secs(1)),
            "spotify:track:old",
            now,
        ));
        assert!(!pending_track_blocks_observation(
            Some("spotify:track:expected"),
            Some(now),
            "spotify:track:old",
            now,
        ));
    }

    #[test]
    fn stream_quality_reports_the_format_actually_used() {
        let all = ["OGG_VORBIS_96", "OGG_VORBIS_160", "OGG_VORBIS_320"].map(String::from);
        assert_eq!(stream_quality_label(&all, "High"), "320 kbps Ogg Vorbis");
        assert_eq!(stream_quality_label(&all, "Normal"), "160 kbps Ogg Vorbis");
        // No 320 file for this track: the badge must not claim High.
        let no_320 = ["OGG_VORBIS_96", "OGG_VORBIS_160"].map(String::from);
        assert_eq!(stream_quality_label(&no_320, "High"), "160 kbps Ogg Vorbis");
        assert_eq!(stream_quality_label(&["MP3_320".to_string()], "High"), "320 kbps MP3");
        assert_eq!(stream_quality_label(&[], "High"), "");
    }

    #[test]
    fn local_player_overrides_stale_spotify_report_only_while_active() {
        // Hearing Price Tag while Spotify still reports the old session.
        assert!(local_track_conflict(true, Some("spotify:track:price-tag"), "spotify:track:old-session"));
        assert!(!local_track_conflict(true, Some("spotify:track:price-tag"), "spotify:track:price-tag"));
        // Another device took over, so Spotify is right again.
        assert!(!local_track_conflict(false, Some("spotify:track:price-tag"), "spotify:track:other"));
        // Nothing has loaded locally yet.
        assert!(!local_track_conflict(true, None, "spotify:track:other"));
    }

    #[test]
    fn pending_track_never_blocks_matching_confirmation() {
        let now = Instant::now();
        assert!(!pending_track_blocks_observation(
            Some("spotify:track:expected"),
            Some(now + Duration::from_secs(30)),
            "spotify:track:expected",
            now,
        ));
    }
}
