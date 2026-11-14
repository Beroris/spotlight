//! The Mac's Now Playing: the song and its cover in Control Centre and on the lock screen, and
//! play, pause, next, previous and seeking from there, the keyboard's media keys and headphones.

use std::{cell::RefCell, ptr::NonNull};

use block2::RcBlock;
use objc2::{rc::Retained, runtime::AnyObject, AnyThread};
use objc2_app_kit::NSImage;
use objc2_foundation::{NSDictionary, NSMutableDictionary, NSNumber, NSSize, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState,
    MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};

use super::*;

struct Controls {
    /// What was last sent, so its cover isn't read from the disk again.
    song: Option<Song>,
    /// That song's cover, once Spotlight has it.
    artwork: Option<Retained<MPMediaItemArtwork>>,
}

/// Any of the Mac's objects, as its lists take them.
fn object<T: AsRef<AnyObject> + ?Sized>(of: &T) -> &AnyObject {
    of.as_ref()
}

thread_local! {
    static CONTROLS: RefCell<Option<Controls>> = const { RefCell::new(None) };
}

/// Passes a press on to the app, unless Spotlight is keeping out of the Mac's controls.
fn on_press(command: &MPRemoteCommand, pressed: impl Fn(&MPRemoteCommandEvent) -> Option<MediaCommand> + 'static) {
    let handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        if !ENABLED.load(Ordering::Relaxed) {
            return MPRemoteCommandHandlerStatus::CommandFailed;
        }
        // The Mac keeps the event alive for the length of this call.
        if let Some(command) = pressed(unsafe { event.as_ref() }) {
            send(command);
        }
        MPRemoteCommandHandlerStatus::Success
    });
    unsafe {
        command.setEnabled(true);
        // The Mac keeps the handler, so what it gives back to remove it by isn't needed.
        let _ = command.addTargetWithHandler(&handler);
    }
}

/// Call on the UI thread once the window exists.
pub fn install(_window: isize) -> bool {
    let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    unsafe {
        on_press(&center.playCommand(), |_| wanted(true));
        on_press(&center.pauseCommand(), |_| wanted(false));
        on_press(&center.togglePlayPauseCommand(), |_| Some(MediaCommand::PlayPause));
        on_press(&center.nextTrackCommand(), |_| Some(MediaCommand::Next));
        on_press(&center.previousTrackCommand(), |_| Some(MediaCommand::Previous));
        on_press(&center.changePlaybackPositionCommand(), |event| {
            let event = object(event).downcast_ref::<MPChangePlaybackPositionCommandEvent>()?;
            let ms = (event.positionTime() * 1000.0).clamp(0.0, f64::from(i32::MAX));
            Some(MediaCommand::SeekTo(ms as i32))
        });
    }
    CONTROLS.with(|controls| *controls.borrow_mut() = Some(Controls { song: None, artwork: None }));
    true
}

/// The cover, from the copy Spotlight keeps on the disk once it has fetched it.
fn artwork(song: &Song) -> Option<Retained<MPMediaItemArtwork>> {
    let path = crate::cached_album_art_path(song.art_url.as_deref()?)?;
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(&path.to_string_lossy()))?;
    let size = image.size();
    // The Mac asks for the cover at the sizes it shows it, and scales this one itself.
    let give = RcBlock::new(move |_: NSSize| NonNull::from(&*image));
    Some(unsafe { MPMediaItemArtwork::initWithBoundsSize_requestHandler(MPMediaItemArtwork::alloc(), size, &give) })
}

/// The song playing, or `None` for nothing, whether it's playing and where it is.
pub fn show(song: Option<&Song>, playing: bool, position_ms: i32) {
    PLAYING.store(playing, Ordering::Relaxed);
    let song = song.filter(|_| ENABLED.load(Ordering::Relaxed));
    CONTROLS.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(controls) = cell.as_mut() else { return };
        let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        let Some(song) = song else {
            if controls.song.take().is_some() {
                controls.artwork = None;
                unsafe {
                    center.setNowPlayingInfo(None);
                    center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                }
            }
            return;
        };
        // A new song's cover can still be on its way. This is called again once it's there.
        if controls.song.as_ref() != Some(song) || controls.artwork.is_none() {
            controls.artwork = artwork(song);
            controls.song = Some(song.clone());
        }
        let info = NSMutableDictionary::<NSString, AnyObject>::new();
        let text = |key: &NSString, value: &str| info.insert(key, object(&*NSString::from_str(value)));
        let seconds = |key: &NSString, ms: i32| info.insert(key, object(&*NSNumber::new_f64(f64::from(ms.max(0)) / 1000.0)));
        unsafe {
            text(MPMediaItemPropertyTitle, &song.title);
            text(MPMediaItemPropertyArtist, &song.artist);
            text(MPMediaItemPropertyAlbumTitle, &song.album);
            seconds(MPMediaItemPropertyPlaybackDuration, song.duration_ms);
            // The Mac moves the bar on from here by itself, at the rate it's told.
            seconds(MPNowPlayingInfoPropertyElapsedPlaybackTime, position_ms.min(song.duration_ms));
            info.insert(MPNowPlayingInfoPropertyPlaybackRate, object(&*NSNumber::new_f64(if playing { 1.0 } else { 0.0 })));
            if let Some(artwork) = &controls.artwork {
                info.insert(MPMediaItemPropertyArtwork, object(&**artwork));
            }
            let info: &NSDictionary<NSString, AnyObject> = &info;
            center.setNowPlayingInfo(Some(info));
            center.setPlaybackState(if playing { MPNowPlayingPlaybackState::Playing } else { MPNowPlayingPlaybackState::Paused });
        }
    });
}
