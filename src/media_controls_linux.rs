//! Linux's media controls: MPRIS, which the desktop reads over D-Bus. It shows the song and its
//! cover with play, pause, next and previous, and sends the keyboard's media keys to whichever
//! player it lists. The desktop only hears from Spotlight when something changes, and asks
//! where the song is when it wants to know, so this costs nothing while a song plays.
//!
//! A second copy of Spotlight reaches the first the same way, to bring its window back.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use zbus::{
    interface,
    object_server::SignalEmitter,
    zvariant::{ObjectPath, OwnedValue, Value},
    Connection,
};

use super::*;
use crate::window_chrome::APP_ID;

const PATH: &str = "/org/mpris/MediaPlayer2";
/// The name desktops look for players under. Held only while the media controls are on.
const PLAYER_NAME: &str = "org.mpris.MediaPlayer2.spotlight";
/// Held for as long as Spotlight runs, so another copy can find this one.
const OWN_NAME: &str = APP_ID;
/// A song that is this far from where it would have played to by itself has been moved.
const SEEK_NOTICED_MS: i64 = 1_500;

/// What the desktop was last told.
struct Shown {
    song: Option<Song>,
    playing: bool,
    position_ms: i32,
    /// When the song was there.
    at: Instant,
}

impl Shown {
    /// Where the song is now, in ms: it has played on since it was last heard of.
    fn position_now(&self) -> i64 {
        let Some(song) = &self.song else { return 0 };
        let played = if self.playing { self.at.elapsed().as_millis() as i64 } else { 0 };
        (i64::from(self.position_ms) + played).clamp(0, i64::from(song.duration_ms.max(0)))
    }
}

static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);

/// What to tell the desktop has changed.
enum Change {
    Song,
    Playing,
    /// The song was moved to here, in microseconds.
    Moved(i64),
    /// The media controls were switched on or off in Settings.
    Listed(bool),
}

static CHANGES: OnceLock<UnboundedSender<Change>> = OnceLock::new();

fn shown<T>(read: impl FnOnce(&Shown) -> T) -> Option<T> {
    SHOWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(read)
}

fn has_song() -> bool {
    shown(|shown| shown.song.is_some()).unwrap_or(false)
}

/// A press from the desktop, unless Spotlight is keeping out of its media controls.
fn pressed(command: Option<MediaCommand>) {
    if let Some(command) = command.filter(|_| ENABLED.load(Ordering::Relaxed)) {
        send(command);
    }
}

fn on_ui_thread(work: impl FnOnce() + Send + 'static) {
    let _ = slint::invoke_from_event_loop(work);
}

/// The app itself, as MPRIS has it.
struct App;

#[interface(name = "org.mpris.MediaPlayer2")]
impl App {
    fn raise(&self) {
        on_ui_thread(crate::window_chrome::show);
    }

    fn quit(&self) {
        on_ui_thread(|| crate::config::exit_after_saving());
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        "Spotlight"
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        APP_ID
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The song and its buttons, as MPRIS has them.
struct Player;

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        pressed(Some(MediaCommand::Next));
    }

    fn previous(&self) {
        pressed(Some(MediaCommand::Previous));
    }

    fn pause(&self) {
        pressed(wanted(false));
    }

    fn play_pause(&self) {
        pressed(Some(MediaCommand::PlayPause));
    }

    fn stop(&self) {
        pressed(wanted(false));
    }

    fn play(&self) {
        pressed(wanted(true));
    }

    /// Moves the song on, or back, by this many microseconds.
    fn seek(&self, offset: i64) {
        if let Some(now) = shown(Shown::position_now) {
            pressed(Some(MediaCommand::SeekTo((now + offset / 1_000).clamp(0, i64::from(i32::MAX)) as i32)));
        }
    }

    fn set_position(&self, _track: ObjectPath<'_>, position: i64) {
        pressed(Some(MediaCommand::SeekTo((position / 1_000).clamp(0, i64::from(i32::MAX)) as i32)));
    }

    fn open_uri(&self, _uri: &str) {}

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> &str {
        match shown(|shown| (shown.song.is_some(), shown.playing)) {
            Some((true, true)) => "Playing",
            Some((true, false)) => "Paused",
            _ => "Stopped",
        }
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<&str, OwnedValue> {
        let mut metadata = HashMap::new();
        let mut add = |key, value: Value<'_>| {
            if let Ok(value) = value.try_to_owned() {
                metadata.insert(key, value);
            }
        };
        let no_track = || Value::from(ObjectPath::from_static_str_unchecked("/org/mpris/MediaPlayer2/TrackList/NoTrack"));
        match shown(|shown| shown.song.clone()).flatten() {
            Some(song) => {
                add("mpris:trackid", ObjectPath::from_static_str_unchecked("/com/beroris/spotlight/song").into());
                add("mpris:length", (i64::from(song.duration_ms.max(0)) * 1_000).into());
                add("xesam:title", song.title.into());
                add("xesam:artist", vec![song.artist].into());
                add("xesam:album", song.album.into());
                // The desktop fetches the cover itself.
                if let Some(url) = song.art_url {
                    add("mpris:artUrl", url.into());
                }
            }
            None => add("mpris:trackid", no_track()),
        }
        metadata
    }

    /// Where the song is, in microseconds. The desktop asks when it wants to know.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        shown(Shown::position_now).unwrap_or(0) * 1_000
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        has_song()
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        has_song()
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        has_song()
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        has_song()
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        has_song()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

/// Spotlight's own line to the desktop, made the first time it's wanted.
async fn connection() -> zbus::Result<Connection> {
    static CONNECTION: tokio::sync::OnceCell<Connection> = tokio::sync::OnceCell::const_new();
    let connection = CONNECTION
        .get_or_try_init(|| async {
            // Its work is done on Spotlight's own threads, so it brings none of its own.
            let connection = zbus::connection::Builder::session()?.internal_executor(false).build().await?;
            let ticking = connection.clone();
            tokio::spawn(async move {
                loop {
                    ticking.executor().tick().await;
                }
            });
            Ok::<_, zbus::Error>(connection)
        })
        .await?;
    Ok(connection.clone())
}

async fn serve(mut changes: UnboundedReceiver<Change>) -> zbus::Result<()> {
    let connection = connection().await?;
    connection.object_server().at(PATH, App).await?;
    connection.object_server().at(PATH, Player).await?;
    connection.request_name(OWN_NAME).await?;
    let player = connection.object_server().interface::<_, Player>(PATH).await?;
    let mut listed = false;
    while let Some(change) = changes.recv().await {
        let emitter = player.signal_emitter();
        let player = player.get().await;
        match change {
            Change::Listed(wanted) if wanted != listed => {
                listed = wanted;
                if wanted {
                    connection.request_name(PLAYER_NAME).await?;
                } else {
                    connection.release_name(PLAYER_NAME).await?;
                }
            }
            Change::Listed(_) => {}
            Change::Song => {
                player.metadata_changed(emitter).await?;
                player.playback_status_changed(emitter).await?;
                player.can_go_next_changed(emitter).await?;
                player.can_go_previous_changed(emitter).await?;
                player.can_play_changed(emitter).await?;
                player.can_pause_changed(emitter).await?;
                player.can_seek_changed(emitter).await?;
            }
            Change::Playing => player.playback_status_changed(emitter).await?,
            Change::Moved(position) => Player::seeked(emitter, position).await?,
        }
    }
    Ok(())
}

/// Call on the UI thread once the window exists. Returns false if there's no D-Bus to talk
/// to, which leaves everything else working as before.
pub fn install(_window: isize) -> bool {
    let Some(runtime) = crate::RUNTIME_HANDLE.get() else { return false };
    let (changes_tx, changes_rx) = unbounded_channel();
    if CHANGES.set(changes_tx).is_err() {
        return true;
    }
    runtime.spawn(async move {
        let _ = serve(changes_rx).await;
    });
    true
}

/// The song playing, or `None` for nothing, whether it's playing and where it is.
pub fn show(song: Option<&Song>, playing: bool, position_ms: i32) {
    PLAYING.store(playing, Ordering::Relaxed);
    let Some(changes) = CHANGES.get() else { return };
    let enabled = ENABLED.load(Ordering::Relaxed);
    let song = song.filter(|_| enabled);
    let mut shown = SHOWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (same_song, was_playing, expected) = match shown.as_ref() {
        Some(before) => (before.song.as_ref() == song, before.playing, before.position_now()),
        None => (song.is_none(), false, 0),
    };
    *shown = Some(Shown { song: song.cloned(), playing, position_ms, at: Instant::now() });
    drop(shown);
    let _ = changes.send(Change::Listed(enabled));
    if !same_song {
        let _ = changes.send(Change::Song);
    } else if song.is_some() {
        if was_playing != playing {
            let _ = changes.send(Change::Playing);
        }
        if (i64::from(position_ms) - expected).abs() > SEEK_NOTICED_MS {
            let _ = changes.send(Change::Moved(i64::from(position_ms.max(0)) * 1_000));
        }
    }
}

/// Asks the Spotlight that's already running to come to the front ("Raise") or to quit
/// ("Quit"), and waits a moment for it to hear.
pub fn ask_running(method: &'static str) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    runtime.block_on(async {
        let asked = async {
            let connection = Connection::session().await?;
            connection.call_method(Some(OWN_NAME), PATH, Some("org.mpris.MediaPlayer2"), method, &()).await
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), asked).await;
    });
}

/// Tells `look` whether the desktop is in its light look, now and whenever that changes.
pub fn watch_desktop_look(mut look: impl FnMut(bool) + Send + 'static) {
    let Some(runtime) = crate::RUNTIME_HANDLE.get() else { return };
    runtime.spawn(async move {
        // 1 is the desktop's dark look. A desktop with no say of its own is light.
        let mut tell = |value: OwnedValue| look(value.downcast_ref::<u32>().ok() != Some(1));
        let _ = async {
            let settings = zbus::Proxy::new(
                &connection().await?,
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
            )
            .await?;
            let mut changes = settings.receive_signal_with_args("SettingChanged", &[(0, "org.freedesktop.appearance")]).await?;
            tell(settings.call("ReadOne", &("org.freedesktop.appearance", "color-scheme")).await?);
            while let Some(change) = changes.next().await {
                if let Ok((_, key, value)) = change.body().deserialize::<(String, String, OwnedValue)>() {
                    if key == "color-scheme" {
                        tell(value);
                    }
                }
            }
            Ok::<_, zbus::Error>(())
        }
        .await;
    });
}
