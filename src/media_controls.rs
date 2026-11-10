//! Windows' own media controls: the song, its cover and play, pause, next and previous in the
//! box Windows shows by the volume, on the lock screen and in quick settings. Windows draws
//! all of it and only hears from Spotlight when something changes, so it costs nothing while
//! a song plays.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};

use tokio::sync::mpsc::UnboundedSender;

use crate::media_keys::MediaCommand;

/// What Windows shows for the song playing.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
    pub duration_ms: i32,
}

static PLAYING: AtomicBool = AtomicBool::new(false);
static COMMANDS: OnceLock<UnboundedSender<MediaCommand>> = OnceLock::new();

/// Windows' play and pause buttons say which they mean, and only the one that changes
/// something is passed on, so a press can never be undone by the toggle it turns into.
fn wanted(play: bool) -> Option<MediaCommand> {
    (play != PLAYING.load(Ordering::Relaxed)).then_some(MediaCommand::PlayPause)
}

fn send(command: MediaCommand) {
    if let Some(commands) = COMMANDS.get() {
        let _ = commands.send(command);
    }
}

#[cfg(target_os = "windows")]
pub use imp::{install, show};

/// Where presses on Windows' controls go. Call before the window exists.
pub fn set_commands(commands: UnboundedSender<MediaCommand>) {
    let _ = COMMANDS.set(commands);
}

#[cfg(not(target_os = "windows"))]
pub fn install(_window: isize) -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
pub fn show(_song: Option<&Song>, _playing: bool, _position_ms: i32) {}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::cell::RefCell;

    use windows::{
        core::{factory, HSTRING},
        Foundation::{TimeSpan, TypedEventHandler, Uri},
        Media::{
            MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs, SystemMediaTransportControls,
            SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
            SystemMediaTransportControlsTimelineProperties,
        },
        Storage::Streams::RandomAccessStreamReference,
        Win32::{Foundation::HWND, System::WinRT::ISystemMediaTransportControlsInterop},
    };

    struct Controls {
        smtc: SystemMediaTransportControls,
        /// What was last sent, so the same again isn't.
        song: Option<Song>,
        playing: bool,
    }

    thread_local! {
        static CONTROLS: RefCell<Option<Controls>> = const { RefCell::new(None) };
    }

    fn span(ms: i32) -> TimeSpan {
        TimeSpan { Duration: i64::from(ms.max(0)) * 10_000 }
    }

    /// Call on the UI thread once the window exists. Returns false if Windows has no media
    /// controls to offer, which leaves everything else working as before.
    pub fn install(window: isize) -> bool {
        let made = (|| -> windows::core::Result<SystemMediaTransportControls> {
            let interop = factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
            let smtc: SystemMediaTransportControls = unsafe { interop.GetForWindow(HWND(window)) }?;
            // Hidden until there's a song to show.
            smtc.SetIsEnabled(false)?;
            smtc.SetIsPlayEnabled(true)?;
            smtc.SetIsPauseEnabled(true)?;
            smtc.SetIsNextEnabled(true)?;
            smtc.SetIsPreviousEnabled(true)?;
            // These come on one of Windows' threads, so they only pass the press on.
            smtc.ButtonPressed(&TypedEventHandler::new(
                |_, args: &Option<SystemMediaTransportControlsButtonPressedEventArgs>| {
                    let Some(args) = args else { return Ok(()) };
                    let command = match args.Button()? {
                        SystemMediaTransportControlsButton::Play => wanted(true),
                        SystemMediaTransportControlsButton::Pause => wanted(false),
                        SystemMediaTransportControlsButton::Next => Some(MediaCommand::Next),
                        SystemMediaTransportControlsButton::Previous => Some(MediaCommand::Previous),
                        _ => None,
                    };
                    if let Some(command) = command {
                        send(command);
                    }
                    Ok(())
                },
            ))?;
            smtc.PlaybackPositionChangeRequested(&TypedEventHandler::new(
                |_, args: &Option<PlaybackPositionChangeRequestedEventArgs>| {
                    if let Some(args) = args {
                        let ms = args.RequestedPlaybackPosition()?.Duration / 10_000;
                        send(MediaCommand::SeekTo(ms.clamp(0, i64::from(i32::MAX)) as i32));
                    }
                    Ok(())
                },
            ))?;
            Ok(smtc)
        })();
        match made {
            Ok(smtc) => {
                CONTROLS.with(|controls| *controls.borrow_mut() = Some(Controls { smtc, song: None, playing: false }));
                true
            }
            Err(_) => false,
        }
    }

    /// The song playing, or `None` for nothing, whether it's playing and where it is.
    pub fn show(song: Option<&Song>, playing: bool, position_ms: i32) {
        PLAYING.store(playing, Ordering::Relaxed);
        CONTROLS.with(|cell| {
            let mut cell = cell.borrow_mut();
            let Some(controls) = cell.as_mut() else { return };
            let _ = (|| -> windows::core::Result<()> {
                let smtc = &controls.smtc;
                let Some(song) = song else {
                    if controls.song.take().is_some() {
                        smtc.DisplayUpdater()?.ClearAll()?;
                        smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed)?;
                        smtc.SetIsEnabled(false)?;
                    }
                    return Ok(());
                };
                if controls.song.as_ref() != Some(song) {
                    let updater = smtc.DisplayUpdater()?;
                    updater.ClearAll()?;
                    updater.SetType(MediaPlaybackType::Music)?;
                    let music = updater.MusicProperties()?;
                    music.SetTitle(&HSTRING::from(song.title.as_str()))?;
                    music.SetArtist(&HSTRING::from(song.artist.as_str()))?;
                    music.SetAlbumTitle(&HSTRING::from(song.album.as_str()))?;
                    // Windows fetches the cover itself.
                    if let Some(url) = song.art_url.as_deref() {
                        if let Ok(uri) = Uri::CreateUri(&HSTRING::from(url)) {
                            updater.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(&uri)?)?;
                        }
                    }
                    updater.Update()?;
                    if controls.song.is_none() {
                        smtc.SetIsEnabled(true)?;
                    }
                    controls.song = Some(song.clone());
                }
                if controls.playing != playing || smtc.PlaybackStatus()? == MediaPlaybackStatus::Closed {
                    smtc.SetPlaybackStatus(if playing { MediaPlaybackStatus::Playing } else { MediaPlaybackStatus::Paused })?;
                    controls.playing = playing;
                }
                let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
                timeline.SetStartTime(span(0))?;
                timeline.SetEndTime(span(song.duration_ms))?;
                timeline.SetMinSeekTime(span(0))?;
                timeline.SetMaxSeekTime(span(song.duration_ms))?;
                timeline.SetPosition(span(position_ms.min(song.duration_ms)))?;
                smtc.UpdateTimelineProperties(&timeline)?;
                Ok(())
            })();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_and_pause_only_pass_on_a_change() {
        PLAYING.store(false, Ordering::Relaxed);
        assert!(matches!(wanted(true), Some(MediaCommand::PlayPause)));
        assert!(wanted(false).is_none(), "pause while paused");
        PLAYING.store(true, Ordering::Relaxed);
        assert!(wanted(true).is_none(), "play while playing");
        assert!(matches!(wanted(false), Some(MediaCommand::PlayPause)));
        PLAYING.store(false, Ordering::Relaxed);
    }
}
