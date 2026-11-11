//! Custom title bar and smooth wheel scrolling.
//!
//! The window keeps its native frame, so Windows still provides edge resizing,
//! snapping, the shadow and rounded corners. Only the caption area is handed to
//! the app. All of this reacts to window messages and nothing polls: a scroll
//! takes one step per frame the monitor shows, and stops when it has settled.

use slint::ComponentHandle;

use crate::AppWindow;

#[cfg(target_os = "windows")]
pub use imp::*;

#[cfg(not(target_os = "windows"))]
pub fn install(_ui: &AppWindow) -> bool { true }
#[cfg(not(target_os = "windows"))]
pub fn set_smooth_scrolling(_enabled: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn set_caption_layout(_search_left: f32, _search_right: f32, _back_visible: bool, _connect_visible: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn minimize() {}
#[cfg(not(target_os = "windows"))]
pub fn close() {}
#[cfg(not(target_os = "windows"))]
pub fn install_frame_driver(_ui: &AppWindow) {}
#[cfg(not(target_os = "windows"))]
pub fn sync_hover_to_pointer(_ui: &AppWindow) {}
#[cfg(not(target_os = "windows"))]
pub fn listen_for_shortcut(_listen: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn center_on_screen(_ui: &AppWindow) {}
#[cfg(not(target_os = "windows"))]
pub fn copy_text(_text: &str) -> bool { false }
#[cfg(not(target_os = "windows"))]
pub fn enter_mini(_on_top: bool, _saved: Option<(i32, i32)>) -> bool { false }
#[cfg(not(target_os = "windows"))]
pub fn leave_mini() -> Option<(i32, i32)> { None }
#[cfg(not(target_os = "windows"))]
pub fn set_mini_on_top(_on_top: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn in_mini() -> bool { false }
#[cfg(not(target_os = "windows"))]
pub fn quit_running() {}
#[cfg(not(target_os = "windows"))]
pub fn handle() -> isize { 0 }

/// The mini player's size in logical px. `ui/app.slint` lays it out the same way.
pub const MINI_WIDTH: f32 = 420.0;
pub const MINI_HEIGHT: f32 = 112.0;

/// Whether a point (logical px, client area) on the mini player is somewhere to drag it by,
/// rather than one of its buttons or the seek strip. Its pin, full size and close buttons
/// sit in the top right corner, previous, play, next and like just right of the cover, and
/// the seek strip runs along the bottom edge.
pub fn is_mini_caption(x: f32, y: f32, width: f32, height: f32) -> bool {
    if x < 0.0 || y < 0.0 || x >= width || y >= height {
        return false;
    }
    let on_window_buttons = (8.0..36.0).contains(&y) && x >= width - 96.0;
    let on_transport = (106.0..256.0).contains(&x) && (58.0..102.0).contains(&y);
    let on_seek = y >= height - 12.0;
    !(on_window_buttons || on_transport || on_seek)
}

/// Where the mini player's window goes, in physical px: where it was left, kept on the
/// screen, or else in the bottom right corner. `work` is the screen's work area as left, top,
/// right and bottom; `size` is the whole window's; `frame` is the invisible resize border at
/// its sides and bottom, which may hang past the edge as the visible card can't.
pub fn place_mini(
    saved: Option<(i32, i32)>,
    work: (i32, i32, i32, i32),
    size: (i32, i32),
    frame: (i32, i32),
    margin: i32,
) -> (i32, i32) {
    let (left, top, right, bottom) = work;
    let (width, height) = size;
    let (side, base) = frame;
    let lowest_x = left - side;
    let highest_x = (right - width + side).max(lowest_x);
    let lowest_y = top;
    let highest_y = (bottom - height + base).max(lowest_y);
    let (x, y) = saved.unwrap_or((highest_x - margin, highest_y - margin));
    (x.clamp(lowest_x, highest_x), y.clamp(lowest_y, highest_y))
}

/// Fraction of the remaining scroll distance to leave after `elapsed_ms`.
/// Frame-rate independent, so it feels the same on any monitor.
pub fn scroll_decay(elapsed_ms: f32) -> f32 {
    const TIME_CONSTANT_MS: f32 = 55.0;
    (-elapsed_ms / TIME_CONSTANT_MS).exp()
}

/// One tick of a smooth scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollStep {
    /// How far to move now, in whole device pixels: rows are cached as textures,
    /// which blur between pixels.
    pub move_by: f32,
    pub remaining: f32,
    /// The part of a pixel still owed, added to the next step.
    pub carry: f32,
    pub finished: bool,
}

pub fn scroll_step(remaining: f32, carry: f32, decay: f32, scale: f32) -> ScrollStep {
    let left = remaining * decay;
    let finished = left.abs() < 0.5;
    let moved = carry + if finished { remaining } else { remaining - left };
    let move_by = (moved * scale).round() / scale;
    ScrollStep {
        move_by,
        remaining: if finished { 0.0 } else { left },
        carry: if finished { 0.0 } else { moved - move_by },
        finished,
    }
}

/// Width of one caption button and height of the caption buttons, in logical px.
/// `ui/app.slint` draws the buttons at the same size.
pub const CAPTION_BUTTON_WIDTH: f32 = 46.0;
pub const CAPTION_BUTTON_HEIGHT: f32 = 40.0;

/// Whether a point (physical px, client area) is on the maximize button, which
/// sits second from the right. Windows needs this answer for Snap Layouts.
pub fn over_maximize_button(x: f32, y: f32, client_width: f32, scale: f32) -> bool {
    let right = client_width - CAPTION_BUTTON_WIDTH * scale;
    let left = right - CAPTION_BUTTON_WIDTH * scale;
    y >= 0.0 && y < CAPTION_BUTTON_HEIGHT * scale && x >= left && x < right
}

/// Top bar geometry in logical px; `ui/app.slint` lays it out the same way.
pub const TOP_BAR_HEIGHT: f32 = 56.0;
const CAPTION_BUTTONS_WIDTH: f32 = CAPTION_BUTTON_WIDTH * 3.0;
const RIGHT_CONTROLS_INSET: f32 = 146.0;
const CONNECT_WIDTH: f32 = 150.0;
/// The back button sits just before the search box.
const BACK_WIDTH: f32 = 36.0;
const BACK_GAP: f32 = 8.0;

#[derive(Clone, Copy)]
pub struct CaptionLayout {
    pub search_left: f32,
    pub search_right: f32,
    pub back_visible: bool,
    pub connect_visible: bool,
}

/// Whether a point (logical px, client area) is empty title bar, which Windows
/// should treat as the real caption, rather than a control.
pub fn is_empty_title_bar(x: f32, y: f32, client_width: f32, layout: CaptionLayout) -> bool {
    if y < 0.0 || y >= TOP_BAR_HEIGHT {
        return false;
    }
    let on_caption_buttons = y < CAPTION_BUTTON_HEIGHT && x >= client_width - CAPTION_BUTTONS_WIDTH;
    let controls_right = client_width - RIGHT_CONTROLS_INSET;
    let on_connect = layout.connect_visible && x >= controls_right - CONNECT_WIDTH && x < controls_right;
    let on_search = x >= layout.search_left && x <= layout.search_right;
    let back_right = layout.search_left - BACK_GAP;
    let on_back = layout.back_visible && (back_right - BACK_WIDTH..back_right).contains(&x);
    !(on_caption_buttons || on_connect || on_search || on_back)
}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        ptr,
        sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        System::Threading::{GetCurrentProcessId, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MonitorFromWindow, ScreenToClient, MONITORINFO,
            MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY,
        },
        UI::{
            HiDpi::{GetDpiForMonitor, GetDpiForWindow, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{
                GetKeyState, TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
                VK_SHIFT,
            },
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::{
                EnumThreadWindows, EnumWindows, GetClassNameW, GetWindowThreadProcessId, RegisterWindowMessageW, GetClientRect, GetCursorPos, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, GetWindowTextW,
                IsIconic, IsWindowVisible, IsZoomed, PostMessageW, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
                ShowWindow, WindowFromPoint, GWL_STYLE, HTBORDER, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT, HTMAXBUTTON, HTTOP,
                HTTOPLEFT, HTTOPRIGHT, HWND_NOTOPMOST, HWND_TOPMOST, MINMAXINFO, NCCALCSIZE_PARAMS, SC_MAXIMIZE,
                SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYCAPTION, SM_CYSIZEFRAME, SWP_FRAMECHANGED, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                SWP_NOZORDER, SWP_SHOWWINDOW, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWNORMAL, WINDOWPLACEMENT, WINDOWPOS,
                WM_CHAR, WM_CLOSE, WM_DEADCHAR, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
                WM_LBUTTONDOWN, WM_MOUSEWHEEL, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP,
                WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SIZE, WM_SYSCHAR, WM_SYSCOMMAND, WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
                WM_WINDOWPOSCHANGED, WS_MAXIMIZEBOX,
            },
        },
    };

    const SUBCLASS_ID: usize = 0x5350_4f54;
    const SIZE_MAXIMIZED: usize = 2;
    /// Logical pixels per wheel notch.
    const PIXELS_PER_NOTCH: f32 = 120.0;

    static SMOOTH_SCROLLING: AtomicBool = AtomicBool::new(true);
    static WINDOW: AtomicIsize = AtomicIsize::new(0);
    /// The window is the mini player, which only moves: Windows can't resize or maximize it.
    static MINI: AtomicBool = AtomicBool::new(false);

    #[derive(Default)]
    struct SmoothScroll {
        remaining: Cell<f32>,
        carry: Cell<f32>,
        position: Cell<(f32, f32)>,
        last_tick: Cell<Option<Instant>>,
        active: Cell<bool>,
    }

    /// Where keys go while a shortcut box in Settings listens for one.
    #[derive(Clone, Copy, PartialEq)]
    enum ShortcutKeys {
        Off,
        /// Every key goes to the box, and none to the app.
        Listening,
        /// The box is done, and the keys still held are let go without reaching the app, so
        /// letting go of Alt doesn't open the window menu.
        Draining,
    }

    thread_local! {
        static UI: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
        static SHORTCUT_KEYS: Cell<ShortcutKeys> = const { Cell::new(ShortcutKeys::Off) };
        static SCROLL: SmoothScroll = SmoothScroll::default();
        static MAX_PRESSED: Cell<bool> = const { Cell::new(false) };
        static CAPTION_LAYOUT: Cell<CaptionLayout> = const {
            Cell::new(CaptionLayout { search_left: 0.0, search_right: 0.0, back_visible: false, connect_visible: false })
        };
        /// Where the full window was, and whether it had a maximize button, while the mini
        /// player is out.
        static FULL_WINDOW: Cell<Option<(WINDOWPLACEMENT, bool)>> = const { Cell::new(None) };
    }

    fn window() -> HWND {
        WINDOW.load(Ordering::Relaxed) as HWND
    }

    /// The main window, or null before it exists.
    pub fn handle() -> HWND {
        window()
    }

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

    unsafe extern "system" fn find_main_window(hwnd: HWND, found: LPARAM) -> i32 {
        let mut title = [0u16; 32];
        let length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        if String::from_utf16_lossy(&title[..length.max(0) as usize]) == "Spotlight" {
            *(found as *mut HWND) = hwnd;
            return 0;
        }
        1
    }

    /// What `Spotlight.exe --quit` sends to every running Spotlight, once it's registered.
    static QUIT_MESSAGE: AtomicU32 = AtomicU32::new(0);

    fn quit_message() -> u32 {
        let name: Vec<u16> = "SpotlightQuit\0".encode_utf16().collect();
        unsafe { RegisterWindowMessageW(name.as_ptr()) }
    }

    unsafe extern "system" fn collect_spotlights(hwnd: HWND, found: LPARAM) -> i32 {
        let mut class = [0u16; 32];
        let class_length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        let mut title = [0u16; 32];
        let title_length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        if String::from_utf16_lossy(&class[..class_length.max(0) as usize]) == "Window Class"
            && String::from_utf16_lossy(&title[..title_length.max(0) as usize]) == "Spotlight"
        {
            (*(found as *mut Vec<HWND>)).push(hwnd);
        }
        1
    }

    /// Asks every other running Spotlight to quit, as the installer does before it replaces or
    /// removes the app, and waits up to five seconds for them to go.
    pub fn quit_running() {
        let message = quit_message();
        let mut windows: Vec<HWND> = Vec::new();
        unsafe {
            EnumWindows(Some(collect_spotlights), &mut windows as *mut Vec<HWND> as LPARAM);
            let own = GetCurrentProcessId();
            let mut processes = Vec::new();
            for hwnd in windows {
                let mut process_id = 0;
                GetWindowThreadProcessId(hwnd, &mut process_id);
                if process_id == 0 || process_id == own {
                    continue;
                }
                let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id);
                let asked = PostMessageW(hwnd, message, 0, 0) != 0;
                if !process.is_null() {
                    if asked {
                        processes.push(process);
                    } else {
                        CloseHandle(process);
                    }
                }
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            for process in processes {
                let left = deadline.saturating_duration_since(Instant::now()).as_millis() as u32;
                WaitForSingleObject(process, left);
                CloseHandle(process);
            }
        }
    }

    /// Call on the UI thread. Returns false if the native window does not exist yet.
    pub fn install(ui: &AppWindow) -> bool {
        UI.with(|slot| *slot.borrow_mut() = Some(ui.as_weak()));
        QUIT_MESSAGE.store(quit_message(), Ordering::Relaxed);
        unsafe {
            let mut hwnd: HWND = ptr::null_mut();
            let thread = windows_sys::Win32::System::Threading::GetCurrentThreadId();
            EnumThreadWindows(thread, Some(find_main_window), &mut hwnd as *mut HWND as LPARAM);
            if hwnd.is_null() {
                return false;
            }
            WINDOW.store(hwnd as isize, Ordering::Relaxed);
            if SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, 0) == 0 {
                return true;
            }
            // Makes Windows recalculate the frame through WM_NCCALCSIZE below.
            SetWindowPos(
                hwnd,
                ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            ui.set_window_maximized(IsZoomed(hwnd) != 0);
            follow_visibility(ui);
            ui.invoke_report_caption_layout();
            crate::media_controls::install(hwnd as isize);
            crate::taskbar::install(ui, hwnd as isize);
            ui.invoke_now_playing_changed();
        }
        true
    }

    /// Opens the window in the middle of the screen the pointer is on. Call before the event
    /// loop runs: the window is then made there, so it never shows anywhere else first.
    pub fn center_on_screen(ui: &AppWindow) {
        unsafe {
            let mut pointer = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pointer);
            let monitor = MonitorFromPoint(pointer, MONITOR_DEFAULTTOPRIMARY);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return;
            }
            let (mut dpi, mut _dpi_y) = (96, 96);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut _dpi_y);
            let scale = dpi as f32 / 96.0;
            // The window starts with a standard frame, which install() then takes over: the
            // frame and caption above the content become part of the window, and the frame
            // at the sides and bottom stays as invisible resize borders.
            let side = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            let top = GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                + GetSystemMetricsForDpi(SM_CYCAPTION, dpi);
            let width = (ui.get_opening_width() * scale).round() as i32;
            let height = (ui.get_opening_height() * scale).round() as i32 + top;
            // A window bigger than the screen keeps its top left corner on it.
            let work = info.rcWork;
            let left = work.left + ((work.right - work.left - width) / 2).max(0);
            let top = work.top + ((work.bottom - work.top - height) / 2).max(0);
            ui.window().set_position(slint::PhysicalPosition::new(left - side, top));
        }
    }

    /// Takes Windows' word on whether the window can be seen, asked once a change has happened,
    /// as coming back from the tray both shows the window and restores it.
    fn follow_visibility(ui: &AppWindow) {
        let hidden = window_is_hidden();
        if ui.get_window_hidden() != hidden {
            ui.set_window_hidden(hidden);
            if !hidden {
                ui.invoke_window_shown();
            }
        }
    }

    /// Sends every key to the shortcut box listening in Settings, through `shortcut_key`, until
    /// called again with false. Call on the UI thread.
    pub fn listen_for_shortcut(listen: bool) {
        let next = if listen {
            ShortcutKeys::Listening
        } else if modifiers_held() != NONE_HELD {
            ShortcutKeys::Draining
        } else {
            ShortcutKeys::Off
        };
        SHORTCUT_KEYS.with(|keys| keys.set(next));
    }

    const NONE_HELD: (bool, bool, bool, bool) = (false, false, false, false);

    /// Ctrl, Alt, Shift and Win, either side of each.
    fn modifiers_held() -> (bool, bool, bool, bool) {
        let held = |key: u16| unsafe { GetKeyState(key as i32) } < 0;
        (held(VK_CONTROL), held(VK_MENU), held(VK_SHIFT), held(VK_LWIN) || held(VK_RWIN))
    }

    /// Takes a key message while a shortcut box listens, or while letting go afterwards.
    /// Returns false for keys that should reach the app as usual.
    fn take_shortcut_key(message: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
        let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
        // Held keys repeat, and only the first press counts.
        let repeat = down && lparam & (1 << 30) != 0;
        match SHORTCUT_KEYS.with(Cell::get) {
            ShortcutKeys::Off => false,
            ShortcutKeys::Listening => {
                let key = wparam as u16;
                let is_modifier = matches!(key, 0x10..=0x12 | 0xa0..=0xa5 | 0x5b | 0x5c);
                if (down && !repeat) || (up && is_modifier) {
                    let (ctrl, alt, shift, win) = modifiers_held();
                    let key = if down { key } else { 0 };
                    update_ui(move |ui| ui.invoke_shortcut_key(i32::from(key), ctrl, alt, shift, win));
                }
                true
            }
            ShortcutKeys::Draining => {
                if down && !repeat {
                    SHORTCUT_KEYS.with(|keys| keys.set(ShortcutKeys::Off));
                    return false;
                }
                if up && modifiers_held() == NONE_HELD {
                    SHORTCUT_KEYS.with(|keys| keys.set(ShortcutKeys::Off));
                }
                true
            }
        }
    }

    /// Puts text on the clipboard. Returns false if another app is holding it.
    pub fn copy_text(text: &str) -> bool {
        use windows_sys::Win32::System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        };
        use windows_sys::Win32::Foundation::GlobalFree;
        const CF_UNICODETEXT: u32 = 13;
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if OpenClipboard(window()) == 0 {
                return false;
            }
            EmptyClipboard();
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            let mut copied = false;
            if !memory.is_null() {
                let target = GlobalLock(memory) as *mut u16;
                if !target.is_null() {
                    ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
                    GlobalUnlock(memory);
                    // The clipboard owns the memory once this works.
                    copied = !SetClipboardData(CF_UNICODETEXT, memory).is_null();
                }
                if !copied {
                    GlobalFree(memory);
                }
            }
            CloseClipboard();
            copied
        }
    }

    pub fn set_smooth_scrolling(enabled: bool) {
        SMOOTH_SCROLLING.store(enabled, Ordering::Relaxed);
    }

    pub fn set_caption_layout(search_left: f32, search_right: f32, back_visible: bool, connect_visible: bool) {
        CAPTION_LAYOUT.with(|layout| layout.set(CaptionLayout { search_left, search_right, back_visible, connect_visible }));
    }

    pub fn minimize() {
        unsafe {
            ShowWindow(window(), SW_MINIMIZE);
        }
    }

    /// Goes through the normal close path, so "close to tray" still applies.
    pub fn close() {
        unsafe {
            PostMessageW(window(), WM_CLOSE, 0, 0);
        }
    }

    pub fn in_mini() -> bool {
        MINI.load(Ordering::Relaxed)
    }

    /// The invisible resize border at the window's sides and at its bottom, in physical px.
    unsafe fn resize_border(hwnd: HWND) -> (i32, i32) {
        let dpi = GetDpiForWindow(hwnd);
        let padded = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        (GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + padded, GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + padded)
    }

    /// The mini player's whole window, border included, in physical px.
    unsafe fn mini_window_size(hwnd: HWND) -> (i32, i32) {
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let (side, bottom) = resize_border(hwnd);
        ((MINI_WIDTH * scale).round() as i32 + side * 2, (MINI_HEIGHT * scale).round() as i32 + bottom)
    }

    unsafe fn set_on_top(hwnd: HWND, on_top: bool) {
        let after = if on_top { HWND_TOPMOST } else { HWND_NOTOPMOST };
        SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }

    /// Shrinks the window to the mini player, where it was last left or else in the bottom
    /// right corner of its screen. Call on the UI thread. Returns false if it didn't.
    pub fn enter_mini(on_top: bool, saved: Option<(i32, i32)>) -> bool {
        let hwnd = window();
        if hwnd.is_null() || in_mini() {
            return false;
        }
        unsafe {
            let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
            placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            if GetWindowPlacement(hwnd, &mut placement) == 0 {
                return false;
            }
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let had_maximize = style & WS_MAXIMIZEBOX as isize != 0;
            FULL_WINDOW.with(|full| full.set(Some((placement, had_maximize))));
            // Set first: the size limits below then let the window shrink.
            MINI.store(true, Ordering::Relaxed);
            if IsZoomed(hwnd) != 0 {
                let mut normal = placement;
                normal.showCmd = SW_SHOWNORMAL as u32;
                SetWindowPlacement(hwnd, &normal);
            }
            // Without the button, snapping it to the top of the screen or Win+Up don't maximize it.
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_MAXIMIZEBOX as isize));
            let (width, height) = mini_window_size(hwnd);
            let found = saved
                .map(|(x, y)| MonitorFromRect(&RECT { left: x, top: y, right: x + width, bottom: y + height }, MONITOR_DEFAULTTONULL))
                .filter(|monitor| !monitor.is_null());
            let monitor = found.unwrap_or_else(|| MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST));
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            let (x, y) = if GetMonitorInfoW(monitor, &mut info) != 0 {
                let work = info.rcWork;
                let margin = (16.0 * GetDpiForWindow(hwnd) as f32 / 96.0).round() as i32;
                let saved = found.and(saved);
                place_mini(saved, (work.left, work.top, work.right, work.bottom), (width, height), resize_border(hwnd), margin)
            } else {
                saved.unwrap_or((0, 0))
            };
            let after = if on_top { HWND_TOPMOST } else { HWND_NOTOPMOST };
            SetWindowPos(hwnd, after, x, y, width, height, SWP_NOACTIVATE | SWP_FRAMECHANGED);
        }
        true
    }

    /// Puts the window back the way it was before the mini player, and returns where the mini
    /// player was. Call on the UI thread.
    pub fn leave_mini() -> Option<(i32, i32)> {
        let hwnd = window();
        if hwnd.is_null() || !MINI.swap(false, Ordering::Relaxed) {
            return None;
        }
        unsafe {
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let left_at = (IsIconic(hwnd) == 0 && GetWindowRect(hwnd, &mut rect) != 0).then_some((rect.left, rect.top));
            let full = FULL_WINDOW.with(Cell::take);
            if full.is_none_or(|(_, had_maximize)| had_maximize) {
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_MAXIMIZEBOX as isize);
            }
            SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED);
            if let Some((placement, _)) = full {
                SetWindowPlacement(hwnd, &placement);
            }
            left_at
        }
    }

    /// Whether the mini player stays above other windows. Does nothing to the full window.
    pub fn set_mini_on_top(on_top: bool) {
        let hwnd = window();
        if !hwnd.is_null() && in_mini() {
            unsafe { set_on_top(hwnd, on_top) };
        }
    }

    /// Windows' answer for a point on the mini player, given its own: the border around it
    /// doesn't resize it, and all but its controls drag it.
    unsafe fn mini_hit_test(hwnd: HWND, lparam: LPARAM, default: LRESULT) -> LRESULT {
        if default != HTCLIENT as LRESULT {
            return if (HTLEFT as LRESULT..=HTBOTTOMRIGHT as LRESULT).contains(&default) { HTBORDER as LRESULT } else { default };
        }
        let mut point = POINT { x: signed_low_word(lparam), y: signed_high_word(lparam) };
        ScreenToClient(hwnd, &mut point);
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd, &mut client);
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let caption = is_mini_caption(
            point.x as f32 / scale,
            point.y as f32 / scale,
            client.right as f32 / scale,
            client.bottom as f32 / scale,
        );
        if caption { HTCAPTION as LRESULT } else { HTCLIENT as LRESULT }
    }

    unsafe fn toggle_maximized(hwnd: HWND) {
        ShowWindow(hwnd, if IsZoomed(hwnd) != 0 { SW_RESTORE } else { SW_MAXIMIZE });
    }

    fn signed_low_word(value: isize) -> i32 {
        (value & 0xffff) as u16 as i16 as i32
    }

    fn signed_high_word(value: isize) -> i32 {
        ((value >> 16) & 0xffff) as u16 as i16 as i32
    }

    unsafe fn frame_thickness(hwnd: HWND) -> i32 {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }

    unsafe fn hit_test(hwnd: HWND, lparam: LPARAM) -> Option<u32> {
        let mut point = POINT { x: signed_low_word(lparam), y: signed_high_word(lparam) };
        ScreenToClient(hwnd, &mut point);
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd, &mut client);
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        if IsZoomed(hwnd) == 0 {
            // The frame's top edge now lies inside the client area.
            let border = frame_thickness(hwnd);
            if point.y < border {
                return Some(if point.x < border * 2 {
                    HTTOPLEFT
                } else if point.x >= client.right - border * 2 {
                    HTTOPRIGHT
                } else {
                    HTTOP
                });
            }
        }
        if over_maximize_button(point.x as f32, point.y as f32, client.right as f32, scale) {
            return Some(HTMAXBUTTON);
        }
        let layout = CAPTION_LAYOUT.with(Cell::get);
        is_empty_title_bar(point.x as f32 / scale, point.y as f32 / scale, client.right as f32 / scale, layout)
            .then_some(HTCAPTION)
    }

    fn set_max_hover(hovered: bool) {
        update_ui(move |ui| ui.set_caption_max_hover(hovered));
    }

    fn set_max_pressed(pressed: bool) {
        MAX_PRESSED.with(|state| state.set(pressed));
        update_ui(move |ui| ui.set_caption_max_pressed(pressed));
    }

    /// Steps a scroll once for every frame the monitor shows. Frames only run
    /// while there is distance left, so an idle window does no work at all.
    pub fn install_frame_driver(ui: &AppWindow) {
        let weak = ui.as_weak();
        let _ = ui.window().set_rendering_notifier(move |state, api| {
            // A window has one notifier, and the stage cover's tilt draws from it too.
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

    /// Minimized or in the tray, so no frames are drawn and none can be seen.
    pub fn window_is_hidden() -> bool {
        let window = window();
        !window.is_null() && unsafe { IsWindowVisible(window) == 0 || IsIconic(window) != 0 }
    }

    /// Spread a wheel notch over several frames.
    fn add_scroll(delta_notches: f32, position: (f32, f32)) {
        SCROLL.with(|scroll| {
            scroll.remaining.set(scroll.remaining.get() + delta_notches * PIXELS_PER_NOTCH);
            scroll.position.set(position);
            // No frames come while minimized, which would strand a running scroll.
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
        let Some(ui) = UI.with(|ui| ui.borrow().as_ref().and_then(|weak| weak.upgrade())) else {
            return;
        };
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

    /// A scroll's last ticks land where the wheel was turned, which can be after
    /// the pointer has moved on, and new rows don't know the pointer is over them.
    /// Either would look wrong until the pointer next crossed the window, so hover
    /// goes back to where the pointer really is.
    pub fn sync_hover_to_pointer(ui: &AppWindow) {
        let hwnd = window();
        let mut point = POINT { x: 0, y: 0 };
        let event = unsafe {
            if GetCursorPos(&mut point) == 0 {
                return;
            }
            if WindowFromPoint(point) == hwnd {
                ScreenToClient(hwnd, &mut point);
                let scale = ui.window().scale_factor();
                slint::platform::WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(point.x as f32 / scale, point.y as f32 / scale),
                }
            } else {
                slint::platform::WindowEvent::PointerExited
            }
        };
        ui.window().dispatch_event(event);
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        let quit = QUIT_MESSAGE.load(Ordering::Relaxed);
        if quit != 0 && message == quit {
            crate::tray::remove_icon();
            crate::config::exit_after_saving();
        }
        if let Some(handled) = crate::taskbar::handle_message(message, wparam) {
            return handled;
        }
        match message {
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR
                if take_shortcut_key(message, wparam, lparam) =>
            {
                0
            }
            // Windows is shutting down or signing out, which ends the app without it quitting.
            windows_sys::Win32::UI::WindowsAndMessaging::WM_ENDSESSION if wparam != 0 => {
                crate::config::run_before_exit();
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_KILLFOCUS => {
                // Keys stop coming once another window has them, so a listening shortcut box gives up.
                if SHORTCUT_KEYS.with(Cell::get) == ShortcutKeys::Listening {
                    update_ui(|ui| ui.invoke_record_shortcut("".into()));
                }
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_NCCALCSIZE if wparam != 0 => {
                // Let Windows lay out the side and bottom frame, then take back
                // the caption it reserved at the top.
                let params = lparam as *mut NCCALCSIZE_PARAMS;
                let original_top = (*params).rgrc[0].top;
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                // A maximized window hangs past the screen edge by one frame.
                let inset = if IsZoomed(hwnd) != 0 { frame_thickness(hwnd) } else { 0 };
                (*params).rgrc[0].top = original_top + inset;
                result
            }
            WM_NCHITTEST if in_mini() => {
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                mini_hit_test(hwnd, lparam, result)
            }
            WM_GETMINMAXINFO if in_mini() => {
                // The mini player is one size, whatever the window's own limits say.
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                let info = lparam as *mut MINMAXINFO;
                let (width, height) = mini_window_size(hwnd);
                let size = POINT { x: width, y: height };
                (*info).ptMinTrackSize = size;
                (*info).ptMaxTrackSize = size;
                (*info).ptMaxSize = size;
                result
            }
            WM_SYSCOMMAND if in_mini() && wparam & 0xfff0 == SC_MAXIMIZE as usize => 0,
            WM_NCLBUTTONDBLCLK if in_mini() && wparam == HTCAPTION as usize => {
                // As a double click on a title bar makes a window big, this brings back the full one.
                update_ui(|ui| ui.invoke_toggle_mini_player());
                0
            }
            WM_EXITSIZEMOVE if in_mini() => {
                let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                if GetWindowRect(hwnd, &mut rect) != 0 {
                    let (x, y) = (rect.left, rect.top);
                    update_ui(move |ui| ui.invoke_mini_moved(x, y));
                }
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_NCHITTEST => {
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                if result != HTCLIENT as LRESULT {
                    return result;
                }
                hit_test(hwnd, lparam).map_or(result, |hit| hit as LRESULT)
            }
            WM_NCMOUSEMOVE if wparam == HTMAXBUTTON as usize => {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
                set_max_hover(true);
                0
            }
            WM_NCMOUSEMOVE | WM_NCMOUSELEAVE => {
                set_max_hover(false);
                if message == WM_NCMOUSELEAVE && MAX_PRESSED.with(Cell::get) {
                    set_max_pressed(false);
                }
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam == HTMAXBUTTON as usize => {
                set_max_pressed(true);
                0
            }
            WM_NCLBUTTONUP if wparam == HTMAXBUTTON as usize => {
                if MAX_PRESSED.with(Cell::get) {
                    set_max_pressed(false);
                    toggle_maximized(hwnd);
                }
                0
            }
            WM_NCLBUTTONDOWN if wparam == HTCAPTION as usize => {
                // Pressing the title bar is still a click outside a text box.
                let mut point = POINT { x: signed_low_word(lparam), y: signed_high_word(lparam) };
                ScreenToClient(hwnd, &mut point);
                let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
                let (x, y) = (point.x as f32 / scale, point.y as f32 / scale);
                update_ui(move |ui| ui.invoke_pointer_pressed(x, y));
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_LBUTTONDOWN => {
                // Slint doesn't take focus away from a text box on outside clicks.
                let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
                let (x, y) = (signed_low_word(lparam) as f32 / scale, signed_high_word(lparam) as f32 / scale);
                update_ui(move |ui| ui.invoke_pointer_pressed(x, y));
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_SIZE => {
                let maximized = wparam == SIZE_MAXIMIZED;
                update_ui(move |ui| {
                    ui.set_window_maximized(maximized);
                    follow_visibility(ui);
                });
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            // Sent once the window has been shown or hidden. WM_SHOWWINDOW comes before, and when the
            // tray shows the window from its own thread, the UI could read it as still hidden.
            WM_WINDOWPOSCHANGED
                if lparam != 0 && (*(lparam as *const WINDOWPOS)).flags & (SWP_SHOWWINDOW | SWP_HIDEWINDOW) != 0 =>
            {
                update_ui(follow_visibility);
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_MOUSEWHEEL if SMOOTH_SCROLLING.load(Ordering::Relaxed) => {
                let notches = signed_high_word(wparam as isize) as f32 / 120.0;
                let mut point = POINT { x: signed_low_word(lparam), y: signed_high_word(lparam) };
                ScreenToClient(hwnd, &mut point);
                let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
                add_scroll(notches, (point.x as f32 / scale, point.y as f32 / scale));
                0
            }
            _ => DefSubclassProc(hwnd, message, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_settles_at_the_same_speed_at_any_frame_rate() {
        let total_after = |frame_ms: f32, frames: u32| (0..frames).fold(1.0, |left, _| left * scroll_decay(frame_ms));
        let at_60hz = total_after(1000.0 / 60.0, 12);
        let at_240hz = total_after(1000.0 / 240.0, 48);
        assert!((at_60hz - at_240hz).abs() < 0.001, "{at_60hz} vs {at_240hz}");
        // Mostly done within a fifth of a second, so it never feels laggy.
        assert!(at_60hz < 0.05, "still {at_60hz} left after 200 ms");
    }

    #[test]
    fn a_scroll_always_finishes_covers_its_distance_and_lands_on_whole_pixels() {
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            for frame_ms in [4.0_f32, 10.0, 16.7, 33.0] {
                for total in [-360.0_f32, -100.0, -3.0, 1.0, 2.5, 100.0, 120.0, 1200.0] {
                    let (mut remaining, mut carry, mut moved, mut ticks) = (total, 0.0, 0.0, 0);
                    loop {
                        let step = scroll_step(remaining, carry, scroll_decay(frame_ms), scale);
                        moved += step.move_by;
                        let device_pixels = step.move_by * scale;
                        assert!((device_pixels - device_pixels.round()).abs() < 0.001, "{} at scale {scale}", step.move_by);
                        (remaining, carry) = (step.remaining, step.carry);
                        ticks += 1;
                        assert!(ticks < 2000, "never finished: {total} px at {frame_ms} ms, scale {scale}");
                        if step.finished {
                            break;
                        }
                    }
                    // Within a device pixel of where it should end up.
                    assert!((moved - total).abs() <= 1.0 / scale + 0.001, "{moved} of {total} at scale {scale}");
                }
            }
        }
    }

    #[test]
    fn only_empty_top_bar_counts_as_title_bar() {
        let layout = CaptionLayout { search_left: 540.0, search_right: 1060.0, back_visible: false, connect_visible: false };
        let width = 1600.0;
        assert!(is_empty_title_bar(60.0, 28.0, width, layout), "logo");
        assert!(is_empty_title_bar(300.0, 28.0, width, layout), "gap before search");
        assert!(is_empty_title_bar(1200.0, 28.0, width, layout), "gap after search");
        assert!(!is_empty_title_bar(800.0, 28.0, width, layout), "search box");
        assert!(!is_empty_title_bar(1040.0, 28.0, width, layout), "settings button, at the end of the search area");
        assert!(is_empty_title_bar(width - 164.0, 28.0, width, layout), "where the settings button used to be");
        assert!(!is_empty_title_bar(width - 23.0, 20.0, width, layout), "close");
        assert!(is_empty_title_bar(width - 23.0, 50.0, width, layout), "below the close button");
        assert!(!is_empty_title_bar(300.0, 57.0, width, layout), "page content");
        assert!(is_empty_title_bar(514.0, 28.0, width, layout), "where the hidden back button goes");
        let with_back = CaptionLayout { back_visible: true, ..layout };
        assert!(!is_empty_title_bar(514.0, 28.0, width, with_back), "visible back button");
        assert!(is_empty_title_bar(150.0, 28.0, width, with_back), "where the back button used to be");
        assert!(is_empty_title_bar(535.0, 28.0, width, with_back), "the gap between it and the search box");
        let signed_out = CaptionLayout { connect_visible: true, ..layout };
        assert!(!is_empty_title_bar(width - 220.0, 28.0, width, signed_out), "connect button");
        assert!(is_empty_title_bar(width - 300.0, 28.0, width, signed_out), "left of the connect button");
        assert!(is_empty_title_bar(width - 220.0, 28.0, width, layout), "where the hidden connect button goes");
    }

    #[test]
    fn the_mini_player_drags_from_all_but_its_controls() {
        let (width, height) = (MINI_WIDTH, MINI_HEIGHT);
        assert!(is_mini_caption(50.0, 50.0, width, height), "cover");
        assert!(is_mini_caption(150.0, 24.0, width, height), "title");
        assert!(is_mini_caption(200.0, 44.0, width, height), "artist");
        assert!(is_mini_caption(340.0, 80.0, width, height), "time");
        assert!(is_mini_caption(width - 50.0, 4.0, width, height), "above the window buttons");
        assert!(!is_mini_caption(width - 82.0, 20.0, width, height), "pin");
        assert!(!is_mini_caption(width - 52.0, 20.0, width, height), "full size");
        assert!(!is_mini_caption(width - 22.0, 20.0, width, height), "close");
        assert!(!is_mini_caption(124.0, 80.0, width, height), "previous");
        assert!(!is_mini_caption(164.0, 80.0, width, height), "play");
        assert!(!is_mini_caption(204.0, 80.0, width, height), "next");
        assert!(!is_mini_caption(238.0, 80.0, width, height), "like");
        assert!(!is_mini_caption(50.0, height - 4.0, width, height), "seek strip");
        assert!(!is_mini_caption(-1.0, 50.0, width, height), "outside");
        // A window a pixel off at an odd scale keeps its buttons where they're drawn.
        assert!(!is_mini_caption(width + 1.0 - 22.0, 20.0, width + 1.0, height), "close, a pixel wider");
    }

    #[test]
    fn the_mini_player_stays_on_its_screen() {
        let work = (0, 0, 1920, 1040);
        let (size, frame, margin) = ((440, 120), (8, 8), 16);
        let corner = place_mini(None, work, size, frame, margin);
        assert_eq!(corner, (1920 - 440 + 8 - 16, 1040 - 120 + 8 - 16), "bottom right, clear of the edge");
        assert_eq!(place_mini(Some((300, 200)), work, size, frame, margin), (300, 200), "where it was left");
        assert_eq!(place_mini(Some((1800, 1000)), work, size, frame, margin), (1920 - 440 + 8, 1040 - 120 + 8), "pulled back on");
        assert_eq!(place_mini(Some((-50, -50)), work, size, frame, margin), (-8, 0), "its top edge stays on the screen");
        // A screen to the left, with the taskbar at the top.
        let left_screen = (-1280, 40, 0, 1024);
        assert_eq!(place_mini(Some((-900, 0)), left_screen, size, frame, margin), (-900, 40));
    }

    #[test]
    fn maximize_hit_area_matches_the_drawn_button_at_any_dpi() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let width = 1600.0 * scale;
            let button = 46.0 * scale;
            assert!(over_maximize_button(width - button * 1.5, 10.0, width, scale));
            assert!(!over_maximize_button(width - button * 0.5, 10.0, width, scale), "close button");
            assert!(!over_maximize_button(width - button * 2.5, 10.0, width, scale), "minimize button");
            assert!(!over_maximize_button(width - button * 1.5, 41.0 * scale, width, scale), "below the buttons");
        }
    }
}
