//! The window on Linux: it has no frame from the desktop, so Spotlight's own top bar is its
//! title bar. The empty parts of the bar move the window, a double click on them makes it
//! big, its edges resize it, and the mini player is the same window made small.
//!
//! The desktop does the moving and resizing itself once asked, on Wayland and on X11 alike.
//! Everything here reacts to what the window is told, so nothing polls. Call everything on the
//! UI thread.

use std::{
    cell::{Cell, RefCell},
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    time::{Duration, Instant},
};

use i_slint_backend_winit::{
    winit::{
        dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
        event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
        platform::wayland::WindowAttributesExtWayland,
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
        window::{CursorIcon, ResizeDirection, UserAttentionType, Window, WindowLevel},
    },
    EventResult, WinitWindowAccessor,
};
use slint::ComponentHandle;

use super::{
    is_empty_title_bar, is_mini_caption, over_maximize_button, place_mini, scroll_decay, scroll_step, CaptionLayout,
    MINI_HEIGHT, MINI_WIDTH,
};
use crate::AppWindow;

/// What the desktop knows Spotlight's window by, which matches it to its launcher and icon.
/// `scripts/package-linux.sh` names the launcher the same.
pub const APP_ID: &str = "com.beroris.spotlight";

/// How far in from an edge a press resizes the window, and how far along from a corner it
/// resizes both ways, in logical px.
const RESIZE_BORDER: f32 = 5.0;
const RESIZE_CORNER: f32 = 14.0;
/// The smallest the full window gets. `ui/app.slint` says the same.
const FULL_MIN_WIDTH: f32 = 1100.0;
const FULL_MIN_HEIGHT: f32 = 640.0;
/// How far the mini player opens from the corner of the screen, in logical px.
const MINI_MARGIN: f32 = 16.0;
/// Two presses this close together, in time and in logical px, are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const DOUBLE_CLICK_REACH: f32 = 4.0;
/// How far the mouse moves with the button held, in logical px, before the window follows.
const DRAG_STARTS_AFTER: f32 = 3.0;
/// Logical pixels per wheel notch.
const PIXELS_PER_NOTCH: f32 = 120.0;

static MINI: AtomicBool = AtomicBool::new(false);
static FRONT: AtomicBool = AtomicBool::new(true);
static SMOOTH_SCROLLING: AtomicBool = AtomicBool::new(true);

/// The full window while the mini player is out.
#[derive(Clone, Copy)]
struct FullWindow {
    size: PhysicalSize<u32>,
    /// Where it was, on desktops that say.
    position: Option<PhysicalPosition<i32>>,
    maximized: bool,
}

#[derive(Default)]
struct SmoothScroll {
    remaining: Cell<f32>,
    carry: Cell<f32>,
    position: Cell<(f32, f32)>,
    last_tick: Cell<Option<Instant>>,
    active: Cell<bool>,
}

thread_local! {
    static UI: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static CAPTION_LAYOUT: Cell<CaptionLayout> = const {
        Cell::new(CaptionLayout { search_left: 0.0, search_right: 0.0, back_visible: false, connect_visible: false })
    };
    /// Where the pointer is, in logical px, while it's over the window.
    static POINTER: Cell<Option<(f32, f32)>> = const { Cell::new(None) };
    /// The edge the pointer is on, which a press would resize by.
    static EDGE: Cell<Option<ResizeDirection>> = const { Cell::new(None) };
    static MAXIMIZED: Cell<bool> = const { Cell::new(false) };
    static MAX_PRESSED: Cell<bool> = const { Cell::new(false) };
    /// The last press on the title bar: when and where.
    static LAST_CAPTION_PRESS: Cell<Option<(Instant, f32, f32)>> = const { Cell::new(None) };
    /// Where the title bar was pressed, while the button is down and the window hasn't moved.
    static CAPTION_HELD: Cell<Option<(f32, f32)>> = const { Cell::new(None) };
    static FULL_WINDOW: Cell<Option<FullWindow>> = const { Cell::new(None) };
    /// Where the mini player was dragged to, until the drag is over.
    static MINI_MOVED_TO: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    static SCROLL: SmoothScroll = SmoothScroll::default();
}

fn ui() -> Option<AppWindow> {
    UI.with(|ui| ui.borrow().as_ref().and_then(|weak| weak.upgrade()))
}

/// Does something with the app once the window is done with what it's being told.
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

fn with_window<T>(work: impl FnOnce(&Window) -> T) -> Option<T> {
    ui()?.window().with_winit_window(work)
}

/// A laptop with two graphics cards has two libraries that can draw, and the system asks
/// NVIDIA's first. Where the screen is on the other card it loads only to say no, which here
/// took about 20 ms of every launch and 29 MB of memory, most of it shared with other apps. So
/// on Wayland, when no connected screen is on an NVIDIA card and nothing has been set to say
/// which card to use, only Mesa is asked.
fn skip_an_unused_nvidia_library() {
    const VENDORS: &str = "/usr/share/glvnd/egl_vendor.d";
    const CHOSEN_ALREADY: [&str; 5] = [
        "__EGL_VENDOR_LIBRARY_FILENAMES",
        "__EGL_VENDOR_LIBRARY_DIRS",
        "__NV_PRIME_RENDER_OFFLOAD",
        "__GLX_VENDOR_LIBRARY_NAME",
        "DRI_PRIME",
    ];
    let mesa = format!("{VENDORS}/50_mesa.json");
    if std::env::var_os("WAYLAND_DISPLAY").is_none()
        || CHOSEN_ALREADY.iter().any(|name| std::env::var_os(name).is_some())
        || !std::path::Path::new(&mesa).exists()
        || !std::path::Path::new(&format!("{VENDORS}/10_nvidia.json")).exists()
    {
        return;
    }
    let Ok(outputs) = std::fs::read_dir("/sys/class/drm") else { return };
    // The driver of every output with a screen on it, such as "amdgpu".
    let drivers: Vec<Option<String>> = outputs
        .flatten()
        .filter(|output| std::fs::read_to_string(output.path().join("status")).is_ok_and(|status| status.trim() == "connected"))
        .map(|output| {
            let driver = std::fs::canonicalize(output.path().join("device/device/driver")).ok()?;
            Some(driver.file_name()?.to_string_lossy().into_owned())
        })
        .collect();
    if only_other_cards_have_screens(&drivers) {
        // Nothing else reads or writes the environment this early.
        std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa);
    }
}

/// Whether every connected screen is on a card that is known and isn't NVIDIA's.
fn only_other_cards_have_screens(drivers: &[Option<String>]) -> bool {
    !drivers.is_empty() && drivers.iter().all(|driver| driver.as_deref().is_some_and(|driver| !driver.starts_with("nvidia")))
}

/// Mesa hands drawing to a second thread for some graphics cards. For a window this light
/// the hand-over costs more than it saves: scrolling took about a tenth more processor time
/// with it. A setting the user made themselves is left alone.
fn draw_on_one_thread() {
    if std::env::var_os("mesa_glthread").is_none() {
        std::env::set_var("mesa_glthread", "false");
    }
}

/// Settles how the graphics libraries will be loaded. Call first thing in `main`, while the
/// app is still one thread: changing the environment while another thread reads it isn't safe.
pub fn choose_graphics_setup() {
    skip_an_unused_nvidia_library();
    draw_on_one_thread();
}

/// Asks for a window with no frame of the desktop's, known to it by Spotlight's name. Call
/// before the window is made.
pub fn prepare() -> Result<(), slint::PlatformError> {
    let backend = i_slint_backend_winit::Backend::builder()
        .with_window_attributes_hook(|attributes| attributes.with_decorations(false).with_name(APP_ID, APP_ID))
        .build()?;
    slint::platform::set_platform(Box::new(backend)).map_err(|error| error.to_string().into())
}

/// Call on the UI thread. Returns false if the native window does not exist yet.
pub fn install(ui: &AppWindow) -> bool {
    UI.with(|slot| *slot.borrow_mut() = Some(ui.as_weak()));
    let Some(wayland) = ui.window().with_winit_window(on_wayland) else { return false };
    // Wayland keeps where windows are, and which is on top, to itself.
    ui.set_can_keep_on_top(!wayland);
    ui.window().on_winit_window_event(window_event);
    follow_window_state(ui);
    ui.invoke_report_caption_layout();
    crate::media_controls::install(0);
    ui.invoke_now_playing_changed();
    true
}

fn on_wayland(window: &Window) -> bool {
    window.window_handle().is_ok_and(|handle| matches!(handle.as_raw(), RawWindowHandle::Wayland(_)))
}

/// Which way a press at a point (logical px) resizes a window of this size, if it's on an edge.
pub fn resize_edge(x: f32, y: f32, width: f32, height: f32) -> Option<ResizeDirection> {
    if x < 0.0 || y < 0.0 || x >= width || y >= height {
        return None;
    }
    let (left, right) = (x < RESIZE_BORDER, x >= width - RESIZE_BORDER);
    let (top, bottom) = (y < RESIZE_BORDER, y >= height - RESIZE_BORDER);
    if !(left || right || top || bottom) {
        return None;
    }
    // Along an edge, the stretch nearest a corner resizes both ways.
    let (near_left, near_right) = (x < RESIZE_CORNER, x >= width - RESIZE_CORNER);
    let (near_top, near_bottom) = (y < RESIZE_CORNER, y >= height - RESIZE_CORNER);
    Some(match (near_left, near_right, near_top, near_bottom) {
        (true, _, true, _) => ResizeDirection::NorthWest,
        (_, true, true, _) => ResizeDirection::NorthEast,
        (true, _, _, true) => ResizeDirection::SouthWest,
        (_, true, _, true) => ResizeDirection::SouthEast,
        _ if left => ResizeDirection::West,
        _ if right => ResizeDirection::East,
        _ if top => ResizeDirection::North,
        _ => ResizeDirection::South,
    })
}

fn cursor_for(edge: ResizeDirection) -> CursorIcon {
    match edge {
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::West => CursorIcon::WResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
    }
}

/// Whether a second press on the title bar, this long after the first and this far from it,
/// makes the two a double click.
pub fn is_double_click(since: Duration, moved_x: f32, moved_y: f32) -> bool {
    since <= DOUBLE_CLICK && moved_x.abs() <= DOUBLE_CLICK_REACH && moved_y.abs() <= DOUBLE_CLICK_REACH
}

fn set_max_hover(ui: &AppWindow, hovered: bool) {
    if ui.get_caption_max_hover() != hovered {
        ui.set_caption_max_hover(hovered);
    }
}

fn set_max_pressed(ui: &AppWindow, pressed: bool) {
    MAX_PRESSED.with(|state| state.set(pressed));
    ui.set_caption_max_pressed(pressed);
}

/// Takes the desktop's word on whether the window fills the screen or is out of sight.
fn follow_window_state(ui: &AppWindow) {
    let Some((maximized, minimized)) =
        ui.window().with_winit_window(|window| (window.is_maximized(), window.is_minimized() == Some(true)))
    else {
        return;
    };
    MAXIMIZED.with(|state| state.set(maximized));
    if ui.get_window_maximized() != maximized {
        ui.set_window_maximized(maximized);
    }
    // Wayland doesn't say when a window is minimized. It stops asking it for frames instead,
    // so nothing is drawn there either way.
    set_hidden(ui, minimized);
}

fn set_hidden(ui: &AppWindow, hidden: bool) {
    if ui.get_window_hidden() != hidden {
        ui.set_window_hidden(hidden);
        if !hidden {
            ui.invoke_window_shown();
        }
    }
}

/// A press on the title bar, or on the mini player: the desktop moves the window once the
/// mouse moves with the button held, and a second press straight after does what a double
/// click on a title bar does.
fn caption_press(window: &Window, x: f32, y: f32) {
    let now = Instant::now();
    let last = LAST_CAPTION_PRESS.with(|press| press.replace(Some((now, x, y))));
    if last.is_some_and(|(at, last_x, last_y)| is_double_click(now - at, x - last_x, y - last_y)) {
        LAST_CAPTION_PRESS.with(|press| press.set(None));
        if in_mini() {
            // As a double click on a title bar makes a window big, this brings back the full one.
            update_ui(|ui| ui.invoke_toggle_mini_player());
        } else {
            window.set_maximized(!window.is_maximized());
        }
        return;
    }
    CAPTION_HELD.with(|held| held.set(Some((x, y))));
}

/// Every event the window gets, before Slint sees it.
fn window_event(slint_window: &slint::Window, event: &WindowEvent) -> EventResult {
    let Some(ui) = ui() else { return EventResult::Propagate };
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            flush_mini_move();
            let scale = slint_window.scale_factor();
            let size = slint_window.size();
            let (x, y) = (position.x as f32 / scale, position.y as f32 / scale);
            let (width, height) = (size.width as f32 / scale, size.height as f32 / scale);
            POINTER.with(|pointer| pointer.set(Some((x, y))));
            if let Some((from_x, from_y)) = CAPTION_HELD.with(Cell::get) {
                if (x - from_x).abs() > DRAG_STARTS_AFTER || (y - from_y).abs() > DRAG_STARTS_AFTER {
                    // The desktop has the mouse from here, and tells nobody when it lets go.
                    CAPTION_HELD.with(|held| held.set(None));
                    slint_window.with_winit_window(|window| window.drag_window());
                }
                return EventResult::PreventDefault;
            }
            let resizable = !in_mini() && !MAXIMIZED.with(Cell::get);
            let edge = if resizable { resize_edge(x, y, width, height) } else { None };
            if edge != EDGE.with(|was| was.replace(edge)) {
                slint_window.with_winit_window(|window| window.set_cursor(edge.map_or(CursorIcon::Default, cursor_for)));
                if edge.is_some() {
                    // What lies under the edge stops being hovered, and Slint's own cursor
                    // starts afresh when the pointer comes back in.
                    slint_window.dispatch_event(slint::platform::WindowEvent::PointerExited);
                }
            }
            if edge.is_some() {
                set_max_hover(&ui, false);
                return EventResult::PreventDefault;
            }
            let over_max = !in_mini() && over_maximize_button(position.x as f32, position.y as f32, size.width as f32, scale);
            set_max_hover(&ui, over_max);
            EventResult::Propagate
        }
        WindowEvent::CursorLeft { .. } => {
            POINTER.with(|pointer| pointer.set(None));
            CAPTION_HELD.with(|held| held.set(None));
            EDGE.with(|edge| edge.set(None));
            set_max_hover(&ui, false);
            if MAX_PRESSED.with(Cell::get) {
                set_max_pressed(&ui, false);
            }
            EventResult::Propagate
        }
        WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => {
            let Some((x, y)) = POINTER.with(Cell::get) else { return EventResult::Propagate };
            let scale = slint_window.scale_factor();
            let size = slint_window.size();
            let (width, height) = (size.width as f32 / scale, size.height as f32 / scale);
            let on_caption = if in_mini() {
                is_mini_caption(x, y, width, height)
            } else {
                is_empty_title_bar(x, y, width, CAPTION_LAYOUT.with(Cell::get))
            };
            match button {
                MouseButton::Left => {
                    if let Some(edge) = EDGE.with(Cell::get) {
                        slint_window.with_winit_window(|window| window.drag_resize_window(edge));
                        return EventResult::PreventDefault;
                    }
                    // Slint doesn't take focus away from a text box on outside clicks.
                    ui.invoke_pointer_pressed(x, y);
                    if !in_mini() && over_maximize_button(x * scale, y * scale, size.width as f32, scale) {
                        set_max_pressed(&ui, true);
                        return EventResult::PreventDefault;
                    }
                    if on_caption {
                        slint_window.with_winit_window(|window| caption_press(window, x, y));
                        return EventResult::PreventDefault;
                    }
                    EventResult::Propagate
                }
                // The desktop's own menu for a window, where it has one to show.
                MouseButton::Right if on_caption => {
                    let at = PhysicalPosition::new(f64::from(x * scale), f64::from(y * scale));
                    slint_window.with_winit_window(|window| window.show_window_menu(at));
                    EventResult::PreventDefault
                }
                _ => EventResult::Propagate,
            }
        }
        WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
            if CAPTION_HELD.with(Cell::take).is_some() {
                return EventResult::PreventDefault; // Slint never heard of the press.
            }
            if !MAX_PRESSED.with(Cell::get) {
                return EventResult::Propagate;
            }
            set_max_pressed(&ui, false);
            if ui.get_caption_max_hover() {
                slint_window.with_winit_window(|window| window.set_maximized(!window.is_maximized()));
            }
            EventResult::PreventDefault
        }
        // A mouse wheel turns in notches, which are spread over several frames. A touchpad
        // already scrolls by the pixel.
        WindowEvent::MouseWheel { delta: MouseScrollDelta::LineDelta(_, notches), .. }
            if *notches != 0.0 && SMOOTH_SCROLLING.load(Ordering::Relaxed) =>
        {
            let Some(position) = POINTER.with(Cell::get) else { return EventResult::Propagate };
            add_scroll(*notches, position);
            EventResult::PreventDefault
        }
        WindowEvent::Focused(focused) => {
            FRONT.store(*focused, Ordering::Relaxed);
            flush_mini_move();
            follow_window_state(&ui);
            EventResult::Propagate
        }
        WindowEvent::Resized(_) => {
            follow_window_state(&ui);
            EventResult::Propagate
        }
        WindowEvent::Occluded(hidden) => {
            set_hidden(&ui, *hidden);
            EventResult::Propagate
        }
        WindowEvent::Moved(position) if in_mini() => {
            MINI_MOVED_TO.with(|moved| moved.set(Some((position.x, position.y))));
            EventResult::Propagate
        }
        _ => EventResult::Propagate,
    }
}

/// Remembers where the mini player was dragged to, once the drag is over.
fn flush_mini_move() {
    if let Some((x, y)) = MINI_MOVED_TO.with(Cell::take) {
        update_ui(move |ui| ui.invoke_mini_moved(x, y));
    }
}

/// Whether the desktop is in its light look. Asked of the desktop once, and kept up from then
/// on by what it says has changed.
pub fn system_is_light() -> bool {
    /// 0 for not asked yet, 1 for dark and 2 for light.
    static LOOK: AtomicU8 = AtomicU8::new(0);
    if LOOK.load(Ordering::Relaxed) == 0 {
        let (first_tx, first_rx) = std::sync::mpsc::channel();
        let mut first_tx = Some(first_tx);
        crate::media_controls::watch_desktop_look(move |light| {
            LOOK.store(if light { 2 } else { 1 }, Ordering::Relaxed);
            match first_tx.take() {
                Some(first) => drop(first.send(())),
                // It went from light to dark or back. The "System" theme follows it.
                None => update_ui(|ui| {
                    if ui.get_theme_mode() == "System" {
                        crate::apply_theme_to_ui(ui);
                    }
                }),
            }
        });
        // A desktop that doesn't answer leaves Spotlight dark.
        let _ = first_rx.recv_timeout(Duration::from_millis(500));
        let _ = LOOK.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed);
    }
    LOOK.load(Ordering::Relaxed) == 2
}

/// The desktop places new windows itself. Call before the event loop runs.
pub fn center_on_screen(_ui: &AppWindow) {}

/// Steps a scroll once for every frame the screen shows, and draws the stage cover's tilt
/// along with each frame.
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
            if SCROLL.with(|scroll| scroll.active.get()) {
                let _ = slint::invoke_from_event_loop(scroll_tick);
            }
        }
    });
}

/// Spread a wheel notch over several frames.
fn add_scroll(delta_notches: f32, position: (f32, f32)) {
    SCROLL.with(|scroll| {
        scroll.remaining.set(scroll.remaining.get() + delta_notches * PIXELS_PER_NOTCH);
        scroll.position.set(position);
        // No frames come while out of sight, which would strand a running scroll.
        let stalled = scroll.last_tick.get().is_none_or(|last| last.elapsed() > Duration::from_millis(100));
        if scroll.active.get() && !stalled {
            return;
        }
        scroll.active.set(true);
        // Moves on the first step instead of a frame later.
        let now = Instant::now();
        scroll.last_tick.set(Some(now.checked_sub(Duration::from_millis(4)).unwrap_or(now)));
        let _ = slint::invoke_from_event_loop(scroll_tick);
    });
}

fn scroll_tick() {
    let Some(ui) = ui() else { return };
    SCROLL.with(|scroll| {
        if !scroll.active.get() {
            return;
        }
        let now = Instant::now();
        let elapsed = scroll.last_tick.replace(Some(now)).map_or(0.0, |last| (now - last).as_secs_f32() * 1000.0);
        let step = scroll_step(scroll.remaining.get(), scroll.carry.get(), scroll_decay(elapsed), ui.window().scale_factor());
        scroll.remaining.set(step.remaining);
        scroll.carry.set(step.carry);
        if step.move_by != 0.0 {
            let (x, y) = scroll.position.get();
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(x, y),
                delta_x: 0.0,
                delta_y: step.move_by,
            });
        }
        if step.finished {
            scroll.active.set(false);
            sync_hover_to_pointer(&ui);
        } else {
            // A step too small to move a pixel still needs the next frame.
            ui.window().request_redraw();
        }
    });
}

/// A scroll's last ticks land where the wheel was turned, which can be after the pointer has
/// moved on, and new rows don't know the pointer is over them. So hover goes back to where
/// the pointer really is.
pub fn sync_hover_to_pointer(ui: &AppWindow) {
    let event = match POINTER.with(Cell::get) {
        Some((x, y)) if EDGE.with(Cell::get).is_none() => {
            slint::platform::WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, y) }
        }
        _ => slint::platform::WindowEvent::PointerExited,
    };
    ui.window().dispatch_event(event);
}

pub fn set_smooth_scrolling(enabled: bool) {
    SMOOTH_SCROLLING.store(enabled, Ordering::Relaxed);
}

pub fn set_caption_layout(search_left: f32, search_right: f32, back_visible: bool, connect_visible: bool) {
    CAPTION_LAYOUT.with(|layout| layout.set(CaptionLayout { search_left, search_right, back_visible, connect_visible }));
}

/// Whether the window is the one in use.
pub fn is_front() -> bool {
    FRONT.load(Ordering::Relaxed)
}

/// Puts text on the clipboard. Returns false if it couldn't be.
pub fn copy_text(text: &str) -> bool {
    i_slint_backend_winit::copy_to_clipboard(text)
}

pub fn minimize() {
    with_window(|window| window.set_minimized(true));
}

/// Goes through the normal close path, as the desktop's own way of closing a window does.
pub fn close() {
    if let Some(ui) = ui() {
        ui.window().dispatch_event(slint::platform::WindowEvent::CloseRequested);
    }
}

/// Makes the window as big as the screen lets it be, or puts it back.
pub fn toggle_maximized() {
    if !in_mini() {
        with_window(|window| window.set_maximized(!window.is_maximized()));
    }
}

/// Brings the window back in front, as when Spotlight is opened again while it runs. Wayland
/// only lets an app ask for attention, which the desktop shows in its own way.
pub fn show() {
    let Some(ui) = ui() else { return };
    let _ = ui.show();
    with_window(|window| {
        window.set_minimized(false);
        window.focus_window();
        if !window.has_focus() {
            window.request_user_attention(Some(UserAttentionType::Informational));
        }
    });
}

pub fn in_mini() -> bool {
    MINI.load(Ordering::Relaxed)
}

/// Shrinks the window to the mini player. Where the desktop lets a window place itself, it
/// goes where it was last left, or else in the bottom right corner of its screen. Returns
/// false if it didn't.
pub fn enter_mini(on_top: bool, saved: Option<(i32, i32)>) -> bool {
    if in_mini() {
        return false;
    }
    let entered = with_window(|window| {
        let maximized = window.is_maximized();
        FULL_WINDOW.with(|full| {
            full.set(Some(FullWindow { size: window.inner_size(), position: window.outer_position().ok(), maximized }))
        });
        MINI.store(true, Ordering::Relaxed);
        if maximized {
            window.set_maximized(false);
        }
        // One size only, so nothing resizes it and nothing makes it big.
        let size = LogicalSize::new(MINI_WIDTH, MINI_HEIGHT);
        window.set_min_inner_size(Some(size));
        window.set_max_inner_size(Some(size));
        window.set_window_level(if on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal });
    });
    EDGE.with(|edge| edge.set(None));
    // Through Slint, which on Wayland hears of the new size no other way.
    if let Some(ui) = ui().filter(|_| entered.is_some()) {
        ui.window().set_size(slint::LogicalSize::new(MINI_WIDTH, MINI_HEIGHT));
        with_window(|window| place_mini_window(window, saved));
    }
    entered.is_some()
}

/// Puts the mini player where it was last, or in a corner of the screen. Wayland places
/// windows itself and says nothing of where, so there this does nothing.
fn place_mini_window(window: &Window, saved: Option<(i32, i32)>) {
    if let (Ok(_), Some(screen)) = (window.outer_position(), window.current_monitor()) {
        let scale = window.scale_factor() as f32;
        let (left, top) = (screen.position().x, screen.position().y);
        let work = (left, top, left + screen.size().width as i32, top + screen.size().height as i32);
        let size = ((MINI_WIDTH * scale).round() as i32, (MINI_HEIGHT * scale).round() as i32);
        let (x, y) = place_mini(saved, work, size, (0, 0), (MINI_MARGIN * scale).round() as i32);
        window.set_outer_position(PhysicalPosition::new(x, y));
    }
}

/// Puts the window back the way it was before the mini player, and returns where the mini
/// player was, on desktops that say.
pub fn leave_mini() -> Option<(i32, i32)> {
    if !MINI.swap(false, Ordering::Relaxed) {
        return None;
    }
    MINI_MOVED_TO.with(|moved| moved.set(None));
    let left_at = with_window(|window| {
        let left_at = window.outer_position().ok().map(|position| (position.x, position.y));
        window.set_window_level(WindowLevel::Normal);
        window.set_min_inner_size(Some(LogicalSize::new(FULL_MIN_WIDTH, FULL_MIN_HEIGHT)));
        window.set_max_inner_size(None::<LogicalSize<f32>>);
        left_at
    })
    .flatten();
    if let (Some(full), Some(ui)) = (FULL_WINDOW.with(Cell::take), ui()) {
        ui.window().set_size(slint::PhysicalSize::new(full.size.width, full.size.height));
        with_window(|window| {
            if let Some(position) = full.position {
                window.set_outer_position(position);
            }
            if full.maximized {
                window.set_maximized(true);
            }
        });
    }
    left_at
}

/// Whether the mini player stays above other windows. Does nothing to the full window.
pub fn set_mini_on_top(on_top: bool) {
    if in_mini() {
        with_window(|window| window.set_window_level(if on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal }));
    }
}

// The shortcut boxes in Settings aren't offered here. Another copy of Spotlight is reached
// over D-Bus, where the media controls already are.
pub fn listen_for_shortcut(_listen: bool) {}
pub fn quit_running() {
    crate::media_controls::ask_running("Quit");
}
pub fn show_running() {
    crate::media_controls::ask_running("Raise");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_rim_of_the_window_resizes_it() {
        let (width, height) = (1600.0, 940.0);
        assert_eq!(resize_edge(800.0, 2.0, width, height), Some(ResizeDirection::North));
        assert_eq!(resize_edge(800.0, height - 1.0, width, height), Some(ResizeDirection::South));
        assert_eq!(resize_edge(0.0, 400.0, width, height), Some(ResizeDirection::West));
        assert_eq!(resize_edge(width - 3.0, 400.0, width, height), Some(ResizeDirection::East));
        assert_eq!(resize_edge(800.0, 6.0, width, height), None, "the top bar, just below the edge");
        assert_eq!(resize_edge(width - 23.0, 20.0, width, height), None, "the close button");
        assert_eq!(resize_edge(-1.0, 400.0, width, height), None, "outside");
        assert_eq!(resize_edge(800.0, height, width, height), None, "outside");
    }

    #[test]
    fn corners_resize_both_ways() {
        let (width, height) = (1600.0, 940.0);
        assert_eq!(resize_edge(2.0, 2.0, width, height), Some(ResizeDirection::NorthWest));
        assert_eq!(resize_edge(10.0, 2.0, width, height), Some(ResizeDirection::NorthWest), "along the top, near the corner");
        assert_eq!(resize_edge(2.0, 10.0, width, height), Some(ResizeDirection::NorthWest), "down the side, near the corner");
        assert_eq!(resize_edge(10.0, 10.0, width, height), None, "inside the corner");
        assert_eq!(resize_edge(width - 2.0, 3.0, width, height), Some(ResizeDirection::NorthEast));
        assert_eq!(resize_edge(3.0, height - 2.0, width, height), Some(ResizeDirection::SouthWest));
        assert_eq!(resize_edge(width - 10.0, height - 2.0, width, height), Some(ResizeDirection::SouthEast));
    }

    #[test]
    fn nvidia_is_only_skipped_when_every_screen_is_on_another_card() {
        let on = |drivers: &[Option<&str>]| only_other_cards_have_screens(&drivers.iter().map(|d| d.map(str::to_string)).collect::<Vec<_>>());
        assert!(on(&[Some("amdgpu")]));
        assert!(on(&[Some("i915"), Some("i915")]));
        assert!(!on(&[Some("amdgpu"), Some("nvidia")]), "an outside screen on the NVIDIA card");
        assert!(!on(&[Some("nvidia-drm")]));
        assert!(!on(&[None]), "a card that doesn't say what it is");
        assert!(!on(&[]), "no screen found, so nothing is known");
    }

    #[test]
    fn a_double_click_is_two_presses_close_together() {
        assert!(is_double_click(Duration::from_millis(150), 1.0, -2.0));
        assert!(!is_double_click(Duration::from_millis(600), 0.0, 0.0), "too slow");
        assert!(!is_double_click(Duration::from_millis(150), 30.0, 0.0), "the pointer moved on");
    }
}
