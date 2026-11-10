use std::{
    fs,
    path::PathBuf,
    sync::{mpsc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};

use crate::models::AppConfig;

static CONFIG_SAVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static CONFIG_SAVE_QUEUE: OnceLock<mpsc::Sender<AppConfig>> = OnceLock::new();

/// Whether this copy is the portable one: its file name says so, as in
/// `Spotlight-1.0.0-Portable.exe`.
pub fn is_portable() -> bool {
    std::env::current_exe().ok().is_some_and(|exe| is_portable_exe(&exe))
}

fn is_portable_exe(exe: &std::path::Path) -> bool {
    exe.file_stem().is_some_and(|stem| stem.to_string_lossy().to_ascii_lowercase().contains("portable"))
}

/// Where settings, the login and the cover cache live: in "Spotlight settings" beside the
/// portable copy, and in the user's local app data otherwise.
pub fn config_dir() -> Result<PathBuf> {
    let portable = std::env::current_exe().ok().filter(|exe| is_portable_exe(exe));
    let path = match portable.as_deref().and_then(std::path::Path::parent) {
        Some(beside) => beside.join("Spotlight settings"),
        None => dirs::data_local_dir().context("LocalAppData directory not found")?.join("Spotlight"),
    };
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub fn load_config() -> Result<AppConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(AppConfig {
            redirect_uri: "http://127.0.0.1:5543/callback/".into(),
            device_name: "Spotlight PC".into(),
            artwork_enabled: true,
            show_artist_column: true,
            show_time_column: true,
            compact_sidebars: false,
            mini_on_top: true,
            theme_mode: "Dark".into(),
            accent_color: "Green".into(),
            audio_quality: "Normal".into(),
            normalize_volume: true,
            autoplay_similar: true,
            remember_volume: true,
            last_volume_percent: 100,
            global_hotkeys: true,
            media_keys_enabled: true,
            play_pause_hotkey: "Ctrl+Alt+P".into(),
            next_hotkey: "Ctrl+Alt+N".into(),
            previous_hotkey: "Ctrl+Alt+B".into(),
            shuffle_hotkey: "Ctrl+Alt+S".into(),
            repeat_hotkey: "Ctrl+Alt+R".into(),
            minimize_to_tray: false,
            close_to_tray: false,
            start_minimized: false,
            low_resource_mode: true,
            reduce_animations: false,
            disable_background_effects: true,
            album_colors: true,
            log_level: "Info".into(),
            check_for_updates: true,
            library_snapshot: Default::default(),
            startup_snapshot: Default::default(),
            ..Default::default()
        });
    }

    let json = fs::read_to_string(path)?;
    let mut cfg: AppConfig = serde_json::from_str(&json)?;
    if cfg.redirect_uri.is_empty() {
        cfg.redirect_uri = "http://127.0.0.1:5543/callback/".into();
    }
    if cfg.device_name.is_empty() {
        cfg.device_name = "Spotlight PC".into();
    }
    if !matches!(cfg.theme_mode.as_str(), "Dark" | "Light" | "System") {
        cfg.theme_mode = "Dark".into();
    }
    if !matches!(cfg.accent_color.as_str(), "Green" | "Blue" | "Purple" | "White") {
        cfg.accent_color = "Green".into();
    }
    if cfg.audio_quality == "Very High" {
        // librespot tops out at 320 kbps, which is what "High" already uses.
        cfg.audio_quality = "High".into();
    }
    if !matches!(cfg.audio_quality.as_str(), "Low" | "Normal" | "High") {
        cfg.audio_quality = "Normal".into();
    }
    cfg.last_volume_percent = cfg.last_volume_percent.clamp(0, 100);
    // Older builds could store media-key names here, which are not valid
    // custom shortcuts (media keys have their own toggle).
    if crate::media_keys::validate_hotkey(&cfg.play_pause_hotkey).is_err() { cfg.play_pause_hotkey = "Ctrl+Alt+P".into(); }
    if crate::media_keys::validate_hotkey(&cfg.next_hotkey).is_err() { cfg.next_hotkey = "Ctrl+Alt+N".into(); }
    if crate::media_keys::validate_hotkey(&cfg.previous_hotkey).is_err() { cfg.previous_hotkey = "Ctrl+Alt+B".into(); }
    if crate::media_keys::validate_hotkey(&cfg.shuffle_hotkey).is_err() { cfg.shuffle_hotkey = "Ctrl+Alt+S".into(); }
    if crate::media_keys::validate_hotkey(&cfg.repeat_hotkey).is_err() { cfg.repeat_hotkey = "Ctrl+Alt+R".into(); }
    if cfg.library_snapshot.playlists.is_empty() && cfg.library_snapshot.liked_songs.is_empty() {
        cfg.library_snapshot = Default::default();
    }
    if cfg.library_snapshot.fetched_at_epoch > 0 && cfg.library_snapshot.next_allowed_refresh_at_epoch < cfg.library_snapshot.fetched_at_epoch {
        cfg.library_snapshot.next_allowed_refresh_at_epoch = cfg.library_snapshot.fetched_at_epoch;
    }
    Ok(cfg)
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    let _guard = CONFIG_SAVE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("config save mutex poisoned");
    let json = serde_json::to_string_pretty(config)?;
    // Written beside it and then moved over it, so a crash or a quit part way through leaves
    // the old file whole rather than half of the new one.
    let path = config_path()?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, json)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

static BEFORE_EXIT: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Something to do as the app quits, such as saving where the music was.
pub fn before_exit(work: impl Fn() + Send + Sync + 'static) {
    let _ = BEFORE_EXIT.set(Box::new(work));
}

/// Does what was asked for before quitting. Also for Windows shutting down, which ends the app
/// without it quitting.
pub fn run_before_exit() {
    if let Some(work) = BEFORE_EXIT.get() {
        // A failure here mustn't stop the app from closing.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    }
}

/// Quits the app, after any save in progress has finished.
pub fn exit_after_saving() -> ! {
    run_before_exit();
    let _no_more_saves = CONFIG_SAVE_LOCK.get_or_init(|| Mutex::new(())).lock();
    std::process::exit(0)
}

pub fn save_config_background(config: AppConfig) {
    let sender = CONFIG_SAVE_QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<AppConfig>();
        std::thread::Builder::new()
            .name("spotlight-config-writer".to_string())
            .spawn(move || {
                while let Ok(mut latest) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        latest = newer;
                    }
                    let _ = save_config(&latest);
                }
            })
            .expect("start Spotlight config writer");
        tx
    });
    let _ = sender.send(config);
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn only_a_copy_named_portable_keeps_its_settings_beside_itself() {
        assert!(is_portable_exe(Path::new(r"D:\Apps\Spotlight-1.0.0-Portable.exe")));
        assert!(is_portable_exe(Path::new(r"D:\Apps\spotlight portable.exe")));
        assert!(!is_portable_exe(Path::new(r"C:\Users\Me\AppData\Local\Programs\Spotlight\Spotlight.exe")));
        assert!(!is_portable_exe(Path::new(r"D:\Portable apps\Spotlight.exe")), "only the file's own name counts");
    }
}
