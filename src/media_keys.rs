use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Once, PoisonError,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use tokio::sync::mpsc;

/// Whether the shortcuts from Settings are on offer. Not on a Mac or on Linux yet, where a box
/// in Settings has no way to hear a new one being pressed. Their media keys work through Now
/// Playing and MPRIS.
pub const OFFERED_HERE: bool = cfg!(target_os = "windows");

#[derive(Clone, Copy, Debug)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
    Shuffle,
    Repeat,
    /// From Windows' media controls: play from this many milliseconds in.
    SeekTo(i32),
}

/// Where shortcut presses go: the shortcuts registered now, and who hears about them. Presses
/// arrive on the thread that registered the shortcuts, as messages there, so no thread of its
/// own has to sit waiting for them.
struct Route {
    owner: u64,
    tx: mpsc::UnboundedSender<MediaCommand>,
    registered: Vec<(HotKey, MediaCommand)>,
    last_seen: HashMap<u32, Instant>,
}

static ROUTE: Mutex<Option<Route>> = Mutex::new(None);
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static HANDLER: Once = Once::new();

fn route(event: GlobalHotKeyEvent) {
    if event.state != HotKeyState::Pressed {
        return;
    }
    let mut route = ROUTE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(route) = route.as_mut() else {
        return;
    };
    let now = Instant::now();
    if route.last_seen.get(&event.id).is_some_and(|previous| now.duration_since(*previous) < Duration::from_millis(90)) {
        return;
    }
    route.last_seen.insert(event.id, now);
    if let Some((_, command)) = route.registered.iter().find(|(key, _)| key.id() == event.id) {
        let _ = route.tx.send(*command);
    }
}

/// The shortcuts from Settings. The keyboard's media keys aren't among them: Windows sends
/// those to its media controls, which pass them to whichever app played last.
pub struct MediaKeyGuard {
    manager: Arc<GlobalHotKeyManager>,
    /// The shortcuts that registered.
    custom: Vec<HotKey>,
    /// Which `Route` is this guard's.
    owner: u64,
    /// Human-readable problems with individual bindings. A bad binding is
    /// skipped instead of disabling every other shortcut.
    pub warnings: Vec<String>,
}

const BINDING_LABELS: [&str; 5] = ["Play/pause", "Next", "Previous", "Shuffle", "Repeat"];

impl MediaKeyGuard {
    pub fn install(tx: mpsc::UnboundedSender<MediaCommand>, bindings: &[String; 5]) -> Result<Self> {
        // Set before anything can be pressed: the library takes the first handler it sees.
        HANDLER.call_once(|| GlobalHotKeyEvent::set_event_handler(Some(route)));
        let manager = Arc::new(GlobalHotKeyManager::new()?);
        let mut candidates = Vec::new();
        let mut warnings = Vec::new();
        let commands = [MediaCommand::PlayPause, MediaCommand::Next, MediaCommand::Previous, MediaCommand::Shuffle, MediaCommand::Repeat];
        for ((binding, command), label) in bindings.iter().zip(commands).zip(BINDING_LABELS) {
            match parse_hotkey(binding) {
                Ok(hotkey) => candidates.push((format!("{label} ({binding})"), hotkey, command)),
                Err(error) => warnings.push(format!("{label} shortcut \"{binding}\" ignored: {error}")),
            }
        }
        let mut registered: Vec<(HotKey, MediaCommand)> = Vec::new();
        for (label, hotkey, command) in candidates {
            if registered.iter().any(|(existing, _)| existing.id() == hotkey.id()) {
                warnings.push(format!("{label} ignored: the same key is already bound."));
                continue;
            }
            match manager.register(hotkey) {
                Ok(()) => registered.push((hotkey, command)),
                Err(error) => warnings.push(format!("{label} could not be registered (another app may own it): {error}")),
            }
        }
        let custom = registered.iter().map(|(hotkey, _)| *hotkey).collect();

        let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
        *ROUTE.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Route { owner, tx, registered, last_seen: HashMap::new() });

        Ok(Self { manager, custom, owner, warnings })
    }

    /// Lets go of the shortcuts from Settings while a new one is being pressed. Windows hands a
    /// registered shortcut to its owner instead of the window, so the box would never see it.
    pub fn pause_custom(&self) {
        let _ = self.manager.unregister_all(&self.custom);
    }

    pub fn resume_custom(&self) {
        for hotkey in &self.custom {
            let _ = self.manager.register(*hotkey);
        }
    }
}

/// A shortcut box in Settings after a key goes down or up while it listens.
#[derive(Debug, PartialEq)]
pub enum Recorded {
    /// Only modifiers so far, such as "Ctrl+Alt+", or "" once they are let go.
    Holding(String),
    /// A whole shortcut, such as "Ctrl+Alt+P".
    Done(String),
    /// Why the key pressed can't be a shortcut, short enough to fit in the box.
    Refused(&'static str),
    Cancelled,
}

/// Reads a key pressed in a listening shortcut box. `key` is a Windows virtual-key code, or 0
/// when a key was let go, and the modifiers are those held after it.
pub fn record_key(key: u16, ctrl: bool, alt: bool, shift: bool, win: bool) -> Recorded {
    const ESCAPE: u16 = 0x1b;
    let held = modifier_names(ctrl, alt, shift, win);
    // Shift, Ctrl and Alt, then their left and right versions, then the two Windows keys.
    if matches!(key, 0 | 0x10..=0x12 | 0xa0..=0xa5 | 0x5b | 0x5c) {
        return Recorded::Holding(if held.is_empty() { held } else { held + "+" });
    }
    if key == ESCAPE {
        return Recorded::Cancelled;
    }
    let Some(name) = key_name(key) else {
        return Recorded::Refused("Can't use that key");
    };
    // Windows keeps most Win+key shortcuts for itself, so Win alone isn't enough.
    if !ctrl && !alt && !shift {
        return Recorded::Refused("Add Ctrl, Alt or Shift");
    }
    // Shift with a letter is typing, which a shortcut in the background would take from every app.
    if shift && !ctrl && !alt && !win && (name.len() == 1 || name == "Space") {
        return Recorded::Refused("Add Ctrl or Alt");
    }
    if alt && !ctrl && !shift && !win && name == "F4" {
        return Recorded::Refused("Alt+F4 closes apps");
    }
    Recorded::Done(format!("{held}+{name}"))
}

/// The modifiers held, as "Ctrl+Alt+Shift+Win" in that order. All four together read "Hyper",
/// which is what a Hyper key sends.
fn modifier_names(ctrl: bool, alt: bool, shift: bool, win: bool) -> String {
    if ctrl && alt && shift && win {
        return "Hyper".into();
    }
    [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift"), (win, "Win")]
        .into_iter()
        .filter_map(|(down, name)| down.then_some(name))
        .collect::<Vec<_>>()
        .join("+")
}

/// The name `parse_hotkey` reads for a Windows virtual-key code, for the keys it takes.
fn key_name(key: u16) -> Option<String> {
    Some(match key {
        0x30..=0x39 | 0x41..=0x5a => char::from(key as u8).to_string(),
        0x20 => "Space".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x70..=0x7b => format!("F{}", key - 0x6f),
        _ => return None,
    })
}

/// Whether two shortcuts name the same keys, such as "ctrl+alt+p" and "Ctrl+Alt+P".
pub fn same_shortcut(first: &str, second: &str) -> bool {
    matches!((parse_hotkey(first), parse_hotkey(second)), (Ok(first), Ok(second)) if first.id() == second.id())
}

/// Checks a custom shortcut string such as "Ctrl+Alt+P".
pub fn validate_hotkey(value: &str) -> Result<()> {
    parse_hotkey(value).map(|_| ())
}

impl Drop for MediaKeyGuard {
    fn drop(&mut self) {
        let mut route = ROUTE.lock().unwrap_or_else(PoisonError::into_inner);
        if route.as_ref().is_some_and(|route| route.owner == self.owner) {
            *route = None;
        }
    }
}

fn parse_hotkey(value: &str) -> Result<HotKey> {
    let parts = value.split('+').map(|part| part.trim().to_ascii_uppercase()).collect::<Vec<_>>();
    let mut modifiers = Modifiers::empty();
    for part in &parts[..parts.len().saturating_sub(1)] {
        match part.as_str() {
            "CTRL" | "CONTROL" => modifiers |= Modifiers::CONTROL,
            "ALT" => modifiers |= Modifiers::ALT,
            "SHIFT" => modifiers |= Modifiers::SHIFT,
            "WIN" | "SUPER" => modifiers |= Modifiers::SUPER,
            "HYPER" => modifiers |= Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::SUPER,
            _ => return Err(anyhow!("unsupported shortcut modifier: {part}")),
        }
    }
    let key = parts.last().ok_or_else(|| anyhow!("shortcut is empty"))?;
    let code = match key.as_str() {
        "A" => Code::KeyA, "B" => Code::KeyB, "C" => Code::KeyC, "D" => Code::KeyD, "E" => Code::KeyE, "F" => Code::KeyF,
        "G" => Code::KeyG, "H" => Code::KeyH, "I" => Code::KeyI, "J" => Code::KeyJ, "K" => Code::KeyK, "L" => Code::KeyL,
        "M" => Code::KeyM, "N" => Code::KeyN, "O" => Code::KeyO, "P" => Code::KeyP, "Q" => Code::KeyQ, "R" => Code::KeyR,
        "S" => Code::KeyS, "T" => Code::KeyT, "U" => Code::KeyU, "V" => Code::KeyV, "W" => Code::KeyW, "X" => Code::KeyX,
        "Y" => Code::KeyY, "Z" => Code::KeyZ, "SPACE" => Code::Space,
        "0" => Code::Digit0, "1" => Code::Digit1, "2" => Code::Digit2, "3" => Code::Digit3, "4" => Code::Digit4,
        "5" => Code::Digit5, "6" => Code::Digit6, "7" => Code::Digit7, "8" => Code::Digit8, "9" => Code::Digit9,
        "LEFT" => Code::ArrowLeft, "RIGHT" => Code::ArrowRight, "UP" => Code::ArrowUp, "DOWN" => Code::ArrowDown,
        "F1" => Code::F1, "F2" => Code::F2, "F3" => Code::F3, "F4" => Code::F4, "F5" => Code::F5, "F6" => Code::F6,
        "F7" => Code::F7, "F8" => Code::F8, "F9" => Code::F9, "F10" => Code::F10, "F11" => Code::F11, "F12" => Code::F12,
        _ => return Err(anyhow!("unsupported shortcut key: {key}")),
    };
    if !modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT) {
        return Err(anyhow!("custom shortcuts need Ctrl, Alt, or Shift"));
    }
    Ok(HotKey::new(Some(modifiers), code))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u16 = 0x11;
    const LEFT_ALT: u16 = 0xa4;

    #[test]
    fn modifiers_show_as_they_are_held_and_let_go() {
        assert_eq!(record_key(CTRL, true, false, false, false), Recorded::Holding("Ctrl+".into()));
        assert_eq!(record_key(LEFT_ALT, true, true, false, false), Recorded::Holding("Ctrl+Alt+".into()));
        assert_eq!(record_key(0, false, true, false, false), Recorded::Holding("Alt+".into()));
        assert_eq!(record_key(0, false, false, false, false), Recorded::Holding("".into()));
    }

    #[test]
    fn a_key_with_modifiers_makes_a_shortcut_the_hotkeys_accept() {
        for (key, ctrl, alt, shift, expected) in [
            (u16::from(b'P'), true, true, false, "Ctrl+Alt+P"),
            (u16::from(b'7'), true, false, true, "Ctrl+Shift+7"),
            (0x20, true, false, true, "Ctrl+Shift+Space"),
            (0x25, false, true, false, "Alt+Left"),
            (0x28, true, false, false, "Ctrl+Down"),
            (0x78, true, false, true, "Ctrl+Shift+F9"),
            (0x7b, false, false, true, "Shift+F12"),
        ] {
            assert_eq!(record_key(key, ctrl, alt, shift, false), Recorded::Done(expected.into()));
            assert!(validate_hotkey(expected).is_ok(), "{expected}");
        }
    }

    #[test]
    fn keys_that_would_get_in_the_way_are_refused() {
        assert_eq!(record_key(u16::from(b'P'), false, false, false, false), Recorded::Refused("Add Ctrl, Alt or Shift"));
        assert_eq!(record_key(u16::from(b'P'), false, false, true, false), Recorded::Refused("Add Ctrl or Alt"));
        assert_eq!(record_key(0x20, false, false, true, false), Recorded::Refused("Add Ctrl or Alt"));
        assert_eq!(record_key(0x73, false, true, false, false), Recorded::Refused("Alt+F4 closes apps"));
        // A semicolon key and Tab aren't keys the shortcuts can use.
        assert_eq!(record_key(0xba, true, true, false, false), Recorded::Refused("Can't use that key"));
        assert_eq!(record_key(0x09, true, false, false, false), Recorded::Refused("Can't use that key"));
    }

    #[test]
    fn escape_cancels_even_with_a_modifier_held() {
        assert_eq!(record_key(0x1b, false, false, false, false), Recorded::Cancelled);
        assert_eq!(record_key(0x1b, true, false, false, false), Recorded::Cancelled);
    }

    #[test]
    fn the_windows_key_counts_and_all_four_modifiers_read_as_hyper() {
        const LEFT_WIN: u16 = 0x5b;
        // A Hyper key holds Ctrl, Alt, Shift and Win before the letter goes down.
        assert_eq!(record_key(LEFT_WIN, true, true, true, true), Recorded::Holding("Hyper+".into()));
        assert_eq!(record_key(u16::from(b'K'), true, true, true, true), Recorded::Done("Hyper+K".into()));
        assert_eq!(record_key(u16::from(b'K'), true, false, false, true), Recorded::Done("Ctrl+Win+K".into()));
        assert_eq!(record_key(u16::from(b'K'), false, false, true, true), Recorded::Done("Shift+Win+K".into()));
        assert_eq!(record_key(u16::from(b'K'), false, false, false, true), Recorded::Refused("Add Ctrl, Alt or Shift"));
        assert!(validate_hotkey("Hyper+K").is_ok());
        assert!(validate_hotkey("Ctrl+Win+K").is_ok());
        assert!(validate_hotkey("Win+K").is_err());
        assert!(same_shortcut("Hyper+K", "Ctrl+Alt+Shift+Win+K"));
        assert!(same_shortcut("hyper+k", "Win+Shift+Alt+Ctrl+K"));
        assert!(!same_shortcut("Hyper+K", "Ctrl+Alt+Shift+K"));
    }

    #[test]
    fn the_same_keys_match_however_they_are_written() {
        assert!(same_shortcut("ctrl+alt+p", "Ctrl+Alt+P"));
        assert!(same_shortcut("Alt+Ctrl+P", "Ctrl+Alt+P"));
        assert!(!same_shortcut("Ctrl+Alt+P", "Ctrl+Shift+P"));
        assert!(!same_shortcut("asdafsadf", "asdafsadf"));
    }
}
