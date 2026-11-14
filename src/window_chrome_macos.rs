//! The window on a Mac: the page runs up under the title bar, the red, yellow and green buttons
//! sit in the top bar, and the mini player is the same window made small.
//!
//! The Mac tells Spotlight when the window moves, changes size or goes out of sight, so nothing
//! here polls. Call everything on the UI thread.

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use block2::RcBlock;
use i_slint_backend_winit::{
    winit::{
        platform::macos::WindowAttributesExtMacOS,
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
    },
    WinitWindowAccessor,
};
use objc2::{rc::Retained, MainThreadMarker, Message};
use objc2_app_kit::{
    NSApplication, NSApplicationWillTerminateNotification, NSFloatingWindowLevel, NSNormalWindowLevel, NSPasteboard,
    NSPasteboardTypeString, NSScreen, NSView, NSWindow, NSWindowButton, NSWindowDidChangeOcclusionStateNotification,
    NSWindowDidEnterFullScreenNotification, NSWindowDidExitFullScreenNotification, NSWindowDidMoveNotification,
    NSWindowDidResizeNotification, NSWindowOcclusionState, NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSNotificationName, NSPoint, NSRect,
    NSSize, NSString, NSUserDefaults,
};
use slint::ComponentHandle;

use super::{MINI_HEIGHT, MINI_WIDTH, TOP_BAR_HEIGHT};
use crate::AppWindow;

/// How far the red button sits from the window's left edge, in points. `ui/app.slint` leaves
/// room for the three of them.
const WINDOW_BUTTONS_LEFT: f64 = 20.0;
/// How far the mini player opens from the corner of the screen.
const MINI_MARGIN: f64 = 16.0;

static MINI: AtomicBool = AtomicBool::new(false);

/// The full window while the mini player is out: where it was, and the sizes it may take.
#[derive(Clone, Copy)]
struct FullWindow {
    frame: NSRect,
    smallest: NSSize,
    largest: NSSize,
}

thread_local! {
    static UI: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static FULL_WINDOW: Cell<Option<FullWindow>> = const { Cell::new(None) };
}

fn window() -> Option<Retained<NSWindow>> {
    WINDOW.with(|window| window.borrow().clone())
}

/// Does something with the app once the Mac is done with what it's telling it, so nothing
/// changes under a window that's part way through moving or resizing.
fn update_ui(change: impl FnOnce(&AppWindow) + Send + 'static) {
    let weak = UI.with(|ui| ui.borrow().clone());
    if let Some(weak) = weak {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                change(&ui);
            }
        });
    }
}

/// Asks for a window whose page runs up under its title bar, with only the three buttons left
/// of the bar itself. Call before the window is made.
pub fn prepare() -> Result<(), slint::PlatformError> {
    let backend = i_slint_backend_winit::Backend::builder()
        .with_window_attributes_hook(|attributes| {
            attributes.with_titlebar_transparent(true).with_title_hidden(true).with_fullsize_content_view(true)
        })
        .build()?;
    slint::platform::set_platform(Box::new(backend)).map_err(|error| error.to_string().into())
}

/// Call on the UI thread. Returns false if the native window does not exist yet.
pub fn install(ui: &AppWindow) -> bool {
    UI.with(|slot| *slot.borrow_mut() = Some(ui.as_weak()));
    let found = ui.window().with_winit_window(|window| {
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
        // The handle is this window's own view, which lives as long as the window does.
        let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        view.window()
    });
    let Some(window) = found.flatten() else { return false };
    place_window_buttons(&window);
    watch(&window);
    WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
    crate::media_controls::install(0);
    ui.invoke_now_playing_changed();
    true
}

/// Moves the red, yellow and green buttons down to the middle of the top bar. The Mac puts
/// them back where a plain title bar has them whenever it lays the window out again, so this
/// is called after every change of size too.
fn place_window_buttons(window: &NSWindow) {
    let buttons = [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton]
        .map(|button| window.standardWindowButton(button));
    let [Some(close), Some(minimize), Some(zoom)] = buttons else { return };
    // The buttons sit in the title bar, which fills the view the window keeps it in.
    let Some(container) = (unsafe { close.superview().and_then(|bar| bar.superview()) }) else { return };
    let height = f64::from(TOP_BAR_HEIGHT);
    let mut frame = container.frame();
    frame.size.height = height;
    frame.origin.y = window.frame().size.height - height;
    container.setFrame(frame);
    let spacing = minimize.frame().origin.x - close.frame().origin.x;
    let y = (height - close.frame().size.height) / 2.0;
    for (place, button) in [close, minimize, zoom].iter().enumerate() {
        button.setFrameOrigin(NSPoint::new(WINDOW_BUTTONS_LEFT + spacing * place as f64, y));
    }
}

fn show_window_buttons(window: &NSWindow, shown: bool) {
    for button in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
        if let Some(button) = window.standardWindowButton(button) {
            button.setHidden(!shown);
        }
    }
}

/// Hears from the Mac about `window`, or about anything if there's none, for as long as the
/// app runs.
fn on_notice(name: &NSNotificationName, window: Option<&NSWindow>, then: impl Fn() + 'static) {
    let block = RcBlock::new(move |_: NonNull<NSNotification>| then());
    let observer = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(name),
            window.map(|window| -> &objc2::runtime::AnyObject { window.as_ref() }),
            None,
            &block,
        )
    };
    // Kept for good: the window is there for as long as the app is.
    std::mem::forget(observer);
}

fn watch(window: &NSWindow) {
    let resized = window.retain();
    on_notice(unsafe { NSWindowDidResizeNotification }, Some(window), move || {
        if !in_mini() {
            place_window_buttons(&resized);
        }
    });

    // Full screen has no buttons, so the top bar takes their room back.
    let entered = || update_ui(|ui| ui.set_mac_full_screen(true));
    on_notice(unsafe { NSWindowDidEnterFullScreenNotification }, Some(window), entered);
    let left = window.retain();
    on_notice(unsafe { NSWindowDidExitFullScreenNotification }, Some(window), move || {
        place_window_buttons(&left);
        update_ui(|ui| ui.set_mac_full_screen(false));
    });

    // Minimized, hidden, on another desktop or wholly behind other windows: nothing of it can
    // be seen, so nothing in it needs to move.
    let seen = window.retain();
    on_notice(unsafe { NSWindowDidChangeOcclusionStateNotification }, Some(window), move || {
        let hidden = !seen.occlusionState().contains(NSWindowOcclusionState::Visible);
        update_ui(move |ui| {
            if ui.get_window_hidden() != hidden {
                ui.set_window_hidden(hidden);
                if !hidden {
                    ui.invoke_window_shown();
                }
            }
        });
    });

    // The mini player opens where it was last left.
    let moved = window.retain();
    on_notice(unsafe { NSWindowDidMoveNotification }, Some(window), move || {
        if in_mini() && !moved.isMiniaturized() {
            let (x, y) = corner(moved.frame());
            update_ui(move |ui| ui.invoke_mini_moved(x, y));
        }
    });

    // Quit from the menu, the Dock or a shutdown ends the app without the window closing.
    on_notice(unsafe { NSApplicationWillTerminateNotification }, None, crate::config::run_before_exit);

    // The Mac went from light to dark or back. The "System" theme follows it.
    let changed = RcBlock::new(|_: NonNull<NSNotification>| {
        update_ui(|ui| {
            if ui.get_theme_mode() == "System" {
                crate::apply_theme_to_ui(ui);
            }
        });
    });
    let observer = unsafe {
        NSDistributedNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(ns_string!("AppleInterfaceThemeChangedNotification")),
            None,
            None,
            &changed,
        )
    };
    std::mem::forget(observer);
}

/// Whether the Mac is in its light appearance.
pub fn system_is_light() -> bool {
    let style = NSUserDefaults::standardUserDefaults().stringForKey(ns_string!("AppleInterfaceStyle"));
    !style.is_some_and(|style| style.to_string().eq_ignore_ascii_case("dark"))
}

/// Opens the window in the middle of the screen in use, and no bigger than it. Call before
/// the event loop runs: the window is then made there, so it never shows anywhere else first.
pub fn center_on_screen(ui: &AppWindow) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(screen) = NSScreen::mainScreen(mtm) else { return };
    // The Mac measures up from the bottom of its first screen, and Slint down from its top.
    let Some(first) = NSScreen::screens(mtm).firstObject() else { return };
    let room = screen.visibleFrame();
    let width = f64::from(ui.get_opening_width()).min(room.size.width);
    let height = f64::from(ui.get_opening_height()).min(room.size.height);
    let left = room.origin.x + (room.size.width - width) / 2.0;
    let top_edge = room.origin.y + room.size.height - (room.size.height - height) / 2.0;
    let top = first.frame().size.height - top_edge;
    ui.window().set_size(slint::LogicalSize::new(width as f32, height as f32));
    ui.window().set_position(slint::LogicalPosition::new(left as f32, top as f32));
}

/// Draws the stage cover's tilt along with each frame.
pub fn install_frame_driver(ui: &AppWindow) {
    let weak = ui.as_weak();
    let _ = ui.window().set_rendering_notifier(move |state, api| {
        if matches!(state, slint::RenderingState::BeforeRendering | slint::RenderingState::RenderingTeardown) {
            if let Some(ui) = weak.upgrade() {
                crate::art_tilt::rendering(&ui, &state, api);
            }
        }
        if matches!(state, slint::RenderingState::AfterRendering) {
            crate::memory_trim::frame_drawn();
        }
    });
}

/// A press on the empty part of the top bar or on the mini player: the Mac moves the window by
/// it, and a second press straight after does what a double click on a title bar does. Call
/// from that press, which is what the Mac follows.
pub fn start_drag() {
    let Some(ui) = UI.with(|ui| ui.borrow().as_ref().and_then(|weak| weak.upgrade())) else { return };
    let presses = MainThreadMarker::new()
        .and_then(|mtm| NSApplication::sharedApplication(mtm).currentEvent())
        .map_or(1, |press| press.clickCount());
    if presses >= 2 {
        if in_mini() {
            // The mini player goes back to the full window, as it does on Windows.
            update_ui(|ui| ui.invoke_toggle_mini_player());
        } else {
            title_bar_double_click();
        }
        return;
    }
    ui.window().with_winit_window(|window| {
        let _ = window.drag_window();
    });
    // The Mac has the mouse from here, so Slint never hears of the button coming up.
    update_ui(|ui| ui.window().dispatch_event(slint::platform::WindowEvent::PointerExited));
}

/// What a double click on the top bar does, which is the owner's choice in System Settings.
fn title_bar_double_click() {
    let Some(window) = window() else { return };
    let choice = NSUserDefaults::standardUserDefaults().stringForKey(ns_string!("AppleActionOnDoubleClick"));
    match choice.map(|choice| choice.to_string()).as_deref() {
        Some("Minimize") => window.miniaturize(None),
        Some("None") => {}
        _ => window.zoom(None),
    }
}

/// The stage's full screen button: the Mac's own full screen, and back.
pub fn toggle_full_screen() {
    if let Some(window) = window().filter(|_| !in_mini()) {
        window.toggleFullScreen(None);
    }
}

/// Whether the window is the one in use, in front of every other app's.
pub fn is_front() -> bool {
    let Some(window) = window() else { return true };
    window.isKeyWindow() && window.occlusionState().contains(NSWindowOcclusionState::Visible)
}

/// Puts text on the clipboard. Returns false if it couldn't be.
pub fn copy_text(text: &str) -> bool {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
}

pub fn minimize() {
    if let Some(window) = window() {
        window.miniaturize(None);
    }
}

/// Goes the way of the red button: out of sight, with the music still playing.
pub fn close() {
    hide_app();
}

/// Hides Spotlight as Hide in its menu does. Clicking it in the Dock brings it back as it was.
pub fn hide_app() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).hide(None);
    }
}

pub fn in_mini() -> bool {
    MINI.load(Ordering::Relaxed)
}

/// The window's bottom left corner, which is how the Mac says where a window is.
fn corner(frame: NSRect) -> (i32, i32) {
    (frame.origin.x.round() as i32, frame.origin.y.round() as i32)
}

/// Where the mini player goes: where it was left, if a screen still shows that spot, kept
/// wholly on that screen, or else in the bottom right corner of the window's own screen.
fn mini_frame(window: &NSWindow, saved: Option<(i32, i32)>, size: NSSize) -> NSRect {
    let overlaps = |room: NSRect, x: f64, y: f64| {
        x < room.origin.x + room.size.width
            && x + size.width > room.origin.x
            && y < room.origin.y + room.size.height
            && y + size.height > room.origin.y
    };
    let saved_on = saved.and_then(|(x, y)| {
        let mtm = MainThreadMarker::new()?;
        let (x, y) = (f64::from(x), f64::from(y));
        NSScreen::screens(mtm).iter().map(|screen| screen.visibleFrame()).find(|room| overlaps(*room, x, y)).map(|room| (room, x, y))
    });
    let (room, x, y) = match saved_on {
        Some(found) => found,
        None => {
            let room = window.screen().map_or(window.frame(), |screen| screen.visibleFrame());
            (room, room.origin.x + room.size.width - size.width - MINI_MARGIN, room.origin.y + MINI_MARGIN)
        }
    };
    let furthest_x = (room.origin.x + room.size.width - size.width).max(room.origin.x);
    let furthest_y = (room.origin.y + room.size.height - size.height).max(room.origin.y);
    NSRect::new(NSPoint::new(x.clamp(room.origin.x, furthest_x), y.clamp(room.origin.y, furthest_y)), size)
}

/// Shrinks the window to the mini player, where it was last left or else in the bottom right
/// corner of its screen. Returns false if it didn't.
pub fn enter_mini(on_top: bool, saved: Option<(i32, i32)>) -> bool {
    let Some(window) = window() else { return false };
    if in_mini() {
        return false;
    }
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        // Leaving full screen takes the Mac a moment, and the window can't shrink until it has.
        window.toggleFullScreen(None);
        return false;
    }
    FULL_WINDOW.with(|full| {
        full.set(Some(FullWindow { frame: window.frame(), smallest: window.contentMinSize(), largest: window.contentMaxSize() }))
    });
    MINI.store(true, Ordering::Relaxed);
    show_window_buttons(&window, false);
    let size = NSSize::new(f64::from(MINI_WIDTH), f64::from(MINI_HEIGHT));
    // One size only, so its edges don't resize it and nothing zooms it.
    window.setContentMinSize(size);
    window.setContentMaxSize(size);
    window.setFrame_display(mini_frame(&window, saved, size), true);
    window.setLevel(if on_top { NSFloatingWindowLevel } else { NSNormalWindowLevel });
    true
}

/// Puts the window back the way it was before the mini player, and returns where the mini
/// player was.
pub fn leave_mini() -> Option<(i32, i32)> {
    let window = window()?;
    if !MINI.swap(false, Ordering::Relaxed) {
        return None;
    }
    let left_at = (!window.isMiniaturized()).then(|| corner(window.frame()));
    window.setLevel(NSNormalWindowLevel);
    if let Some(full) = FULL_WINDOW.with(Cell::take) {
        window.setContentMinSize(full.smallest);
        window.setContentMaxSize(full.largest);
        window.setFrame_display(full.frame, true);
    }
    show_window_buttons(&window, true);
    place_window_buttons(&window);
    left_at
}

/// Whether the mini player stays above other windows. Does nothing to the full window.
pub fn set_mini_on_top(on_top: bool) {
    if let Some(window) = window().filter(|_| in_mini()) {
        window.setLevel(if on_top { NSFloatingWindowLevel } else { NSNormalWindowLevel });
    }
}

// The Mac scrolls smoothly by itself, takes its own caption, and has no installer asking
// Spotlight to quit or a second copy asking it to show: opening it again brings it forward.
pub fn set_smooth_scrolling(_enabled: bool) {}
pub fn set_caption_layout(_search_left: f32, _search_right: f32, _back_visible: bool, _connect_visible: bool) {}
pub fn sync_hover_to_pointer(_ui: &AppWindow) {}
pub fn listen_for_shortcut(_listen: bool) {}
pub fn quit_running() {}
pub fn show_running() {}
