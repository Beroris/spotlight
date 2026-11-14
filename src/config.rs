use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};

use crate::models::AppConfig;

/// Whether this copy is the portable one: its file name says so, as in
/// `Spotlight-1.0.0-Portable.exe`.
pub fn is_portable() -> bool {
    std::env::current_exe().ok().is_some_and(|exe| is_portable_exe(&exe))
}

fn is_portable_exe(exe: &std::path::Path) -> bool {
    exe.file_stem().is_some_and(|stem| stem.to_string_lossy().to_ascii_lowercase().contains("portable"))
}

/// The folder's name in the user's app data. A Mac has a Spotlight of its own, so there it goes
/// by the app's full name, the one `scripts/package-macos.sh` gives the app. Linux keeps such
/// folders in small letters, under `~/.local/share`.
const DATA_FOLDER: &str = if cfg!(target_os = "macos") {
    "com.beroris.spotlight"
} else if cfg!(target_os = "linux") {
    "spotlight"
} else {
    "Spotlight"
};

/// Where settings, the login and the cover cache live: in "Spotlight settings" beside the
/// portable copy, and in the user's local app data otherwise.
pub fn config_dir() -> Result<PathBuf> {
    let portable = std::env::current_exe().ok().filter(|exe| is_portable_exe(exe));
    let path = match portable.as_deref().and_then(std::path::Path::parent) {
        Some(beside) => beside.join("Spotlight settings"),
        None => dirs::data_local_dir().context("LocalAppData directory not found")?.join(DATA_FOLDER),
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
        return Ok(new_config());
    }

    let json = fs::read(&path)?;
    let mut cfg: AppConfig = match serde_json::from_slice(&json) {
        Ok(cfg) => cfg,
        // A damaged file, say from a power cut, mustn't stop Spotlight from opening. It's
        // kept aside and Spotlight starts as new.
        Err(_) => {
            let _ = fs::rename(&path, path.with_extension("json.broken"));
            return Ok(new_config());
        }
    };
    tidy_config(&mut cfg);
    Ok(cfg)
}

/// What this computer is called among Spotify's devices, until it's given another name.
const DEVICE_NAME: &str = if cfg!(target_os = "macos") { "Spotlight Mac" } else { "Spotlight PC" };

/// On Linux the name carries the computer's own, as in "Spotlight Mia", so that it can't be
/// taken for the "Spotlight PC" in the same account's list.
fn device_name() -> String {
    if cfg!(target_os = "linux") {
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        return device_name_for(&host);
    }
    DEVICE_NAME.into()
}

/// "Spotlight" and the computer's name, without anything after its first dot.
fn device_name_for(host: &str) -> String {
    match host.trim().split('.').next().unwrap_or_default() {
        "" | "localhost" => "Spotlight Linux".into(),
        name => format!("Spotlight {name}"),
    }
}

/// The settings Spotlight starts with, and goes back to when its data is reset.
pub fn new_config() -> AppConfig {
    AppConfig {
        redirect_uri: "http://127.0.0.1:5543/callback/".into(),
        device_name: device_name(),
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
        tray_at_sign_in: true,
        low_resource_mode: true,
        reduce_animations: false,
        disable_background_effects: true,
        album_colors: true,
        log_level: "Info".into(),
        check_for_updates: true,
        library_snapshot: Default::default(),
        startup_snapshot: Default::default(),
        ..Default::default()
    }
}

/// Puts right what older builds, or hand edits, could leave in a saved file.
fn tidy_config(cfg: &mut AppConfig) {
    if cfg.redirect_uri.is_empty() {
        cfg.redirect_uri = "http://127.0.0.1:5543/callback/".into();
    }
    // "Spotlight PC" is what the first Linux builds saved, before the name carried the computer's.
    if cfg.device_name.is_empty() || (cfg!(target_os = "linux") && cfg.device_name == DEVICE_NAME) {
        cfg.device_name = device_name();
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
}

/// Writes a file from its own thread, so the caller never waits for the disk. Only the newest
/// content is written: a save that comes while an older one waits replaces it.
pub struct BackgroundWriter<T: Send + 'static> {
    name: &'static str,
    write: fn(&T) -> Result<()>,
    /// Held for each write, so two never cross, and while quitting, so none starts after.
    writing: Mutex<()>,
    waiting: Mutex<Option<T>>,
    wake: OnceLock<mpsc::Sender<()>>,
}

impl<T: Send + 'static> BackgroundWriter<T> {
    pub const fn new(name: &'static str, write: fn(&T) -> Result<()>) -> Self {
        Self { name, write, writing: Mutex::new(()), waiting: Mutex::new(None), wake: OnceLock::new() }
    }

    fn writing(&self) -> MutexGuard<'_, ()> {
        self.writing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take_waiting(&self) -> Option<T> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Saves in a moment, on the writer's thread.
    pub fn save_soon(&'static self, value: T) {
        *self.waiting.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
        let wake = self.wake.get_or_init(|| {
            let (wake, woken) = mpsc::channel::<()>();
            std::thread::Builder::new()
                .name(self.name.to_string())
                .spawn(move || {
                    while woken.recv().is_ok() {
                        while woken.try_recv().is_ok() {}
                        let _writing = self.writing();
                        if let Some(value) = self.take_waiting() {
                            let _ = (self.write)(&value);
                        }
                    }
                })
                .expect("start a Spotlight file writer");
            wake
        });
        let _ = wake.send(());
    }

    /// Saves now. What's still waiting is older, so it's dropped.
    pub fn save_now(&self, value: &T) -> Result<()> {
        let _writing = self.writing();
        self.take_waiting();
        (self.write)(value)
    }

    /// Drops what's waiting and does `work`, such as deleting the file, with no write in
    /// between.
    pub fn discard_then(&self, work: impl FnOnce()) {
        let _writing = self.writing();
        self.take_waiting();
        work();
    }

    /// Writes what's still waiting, then keeps any other write from starting while the
    /// returned guard lives.
    pub fn finish(&self) -> MutexGuard<'_, ()> {
        let writing = self.writing();
        if let Some(value) = self.take_waiting() {
            let _ = (self.write)(&value);
        }
        writing
    }
}

static CONFIG_WRITER: BackgroundWriter<AppConfig> = BackgroundWriter::new("spotlight-config-writer", write_config);

/// Writes a file and waits until it's really on the disk. Otherwise a power cut just after a
/// save can leave the file empty, even once it has been moved into place.
pub fn write_durably(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_config(config: &AppConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(config)?;
    // Written beside it and then moved over it, so a crash or a quit part way through leaves
    // the old file whole rather than half of the new one.
    let path = config_path()?;
    let temporary = path.with_extension("json.tmp");
    write_durably(&temporary, json.as_bytes())?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

static BEFORE_EXIT: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Something to do as the app quits, such as saving where the music was.
pub fn before_exit(work: impl Fn() + Send + Sync + 'static) {
    let _ = BEFORE_EXIT.set(Box::new(work));
}

/// Does what was asked for before quitting, and writes settings still waiting to be saved. Also
/// for Windows shutting down, which ends the app without it quitting.
pub fn run_before_exit() {
    crate::crash_restart::quitting();
    if let Some(work) = BEFORE_EXIT.get() {
        // A failure here mustn't stop the app from closing.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    }
    drop(CONFIG_WRITER.finish());
}

/// Quits the app, once every save has been written.
pub fn exit_after_saving() -> ! {
    run_before_exit();
    let _no_more_saves = CONFIG_WRITER.finish();
    std::process::exit(0)
}

/// Saves the settings in a moment. Call it while still holding the app's state lock, as the
/// borrow asks, so saves queue in the order their changes were made and an older copy can't
/// land on top of a newer one.
pub fn save_config_background(config: &AppConfig) {
    CONFIG_WRITER.save_soon(config.clone());
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
    fn a_linux_device_is_named_after_its_computer() {
        assert_eq!(device_name_for("Mia\n"), "Spotlight Mia");
        assert_eq!(device_name_for("desk.home.example"), "Spotlight desk");
        assert_eq!(device_name_for(""), "Spotlight Linux");
        assert_eq!(device_name_for("localhost"), "Spotlight Linux");
    }

    #[test]
    fn only_a_copy_named_portable_keeps_its_settings_beside_itself() {
        assert!(is_portable_exe(Path::new(r"D:\Apps\Spotlight-1.0.0-Portable.exe")));
        assert!(is_portable_exe(Path::new(r"D:\Apps\spotlight portable.exe")));
        assert!(!is_portable_exe(Path::new(r"C:\Users\Me\AppData\Local\Programs\Spotlight\Spotlight.exe")));
        #[cfg(target_os = "windows")]
        assert!(!is_portable_exe(Path::new(r"D:\Portable apps\Spotlight.exe")), "only the file's own name counts");
        #[cfg(not(target_os = "windows"))]
        assert!(!is_portable_exe(Path::new("/Portable apps/Spotlight.app/Contents/MacOS/Spotlight")), "only the file's own name counts");
    }
}
