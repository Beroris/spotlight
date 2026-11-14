#[cfg(target_os = "windows")]
mod windows_tray {
    use std::{
        iter, mem, ptr,
        sync::{
            atomic::{AtomicIsize, AtomicU32, Ordering},
            Mutex, Once, OnceLock,
        },
    };

    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
                NIF_SHOWTIP, NIM_MODIFY, NOTIFYICONDATAW, NIN_SELECT,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, LoadIconW, LoadImageW,
                PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, ShowWindow,
                TrackPopupMenu, TranslateMessage, HMENU, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE,
                MF_STRING, MSG, SM_CXSMICON, SW_HIDE,
                SW_RESTORE, SW_SHOW, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_COMMAND, WM_CONTEXTMENU,
                WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_USER, WNDCLASSW,
            },
        },
    };

    static START: Once = Once::new();
    /// The tray icon's own window, once it has one.
    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
    /// The message Explorer sends every window when the taskbar comes back after a restart.
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
    static ICON: OnceLock<isize> = OnceLock::new();
    static RESTORE_HANDLER: OnceLock<Mutex<Option<Box<dyn Fn() + Send + 'static>>>> =
        OnceLock::new();

    const TRAY_UID: u32 = 1;
    const TRAY_CALLBACK: u32 = WM_USER + 42;
    const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;
    const MENU_OPEN: usize = 1001;
    const MENU_EXIT: usize = 1002;

    pub fn ensure_started() {
        START.call_once(|| {
            std::thread::Builder::new()
                .name("spotlight-tray".to_string())
                .spawn(tray_thread)
                .ok();
        });
    }

    pub fn set_restore_handler<F>(handler: F)
    where
        F: Fn() + Send + 'static,
    {
        let handlers = RESTORE_HANDLER.get_or_init(|| Mutex::new(None));
        *handlers.lock().unwrap() = Some(Box::new(handler));
    }

    /// Takes the icon out of the tray, as the app is about to quit.
    pub fn remove_icon() {
        let hwnd = TRAY_HWND.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe { delete_tray_icon(hwnd as HWND) };
        }
    }

    pub fn hide_spotlight_window() {
        unsafe {
            let hwnd = find_spotlight_window();
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    fn tray_thread() {
        unsafe {
            let hinstance = GetModuleHandleW(ptr::null());
            let class_name = wide("SpotlightTrayWindow");
            let wnd_class = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance,
                lpszClassName: class_name.as_ptr(),
                ..mem::zeroed()
            };
            RegisterClassW(&wnd_class);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                hinstance,
                ptr::null(),
            );
            if hwnd.is_null() {
                return;
            }
            TRAY_HWND.store(hwnd as isize, Ordering::Relaxed);
            let taskbar_created = wide("TaskbarCreated");
            TASKBAR_CREATED.store(RegisterWindowMessageW(taskbar_created.as_ptr()), Ordering::Relaxed);

            add_tray_icon(hwnd);

            let mut msg: MSG = mem::zeroed();
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            delete_tray_icon(hwnd);
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed);
        if taskbar_created != 0 && msg == taskbar_created {
            // Explorer restarted and forgot every tray icon. Without this one, a window closed
            // to the tray would have no way back.
            add_tray_icon(hwnd);
            return 0;
        }
        match msg {
            TRAY_CALLBACK => match low_word(lparam) {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT | NIN_KEYSELECT => {
                    restore_spotlight_window();
                    0
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    show_tray_menu(hwnd);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_COMMAND => {
                match wparam & 0xffff {
                    MENU_OPEN => restore_spotlight_window(),
                    MENU_EXIT => {
                        delete_tray_icon(hwnd);
                        crate::config::exit_after_saving();
                    }
                    _ => {}
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    fn low_word(value: LPARAM) -> u32 {
        (value as u32) & 0xffff
    }

    unsafe fn add_tray_icon(hwnd: HWND) {
        let mut data = notify_data(hwnd);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = TRAY_CALLBACK;
        data.hIcon = *ICON.get_or_init(|| load_spotlight_icon() as isize) as _;
        write_tip(&mut data, "Spotlight");
        Shell_NotifyIconW(NIM_ADD, &data);
        Shell_NotifyIconW(NIM_MODIFY, &data);
    }

    unsafe fn load_spotlight_icon() -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
        let hinstance = GetModuleHandleW(ptr::null());
        // The size the tray draws at, which is more than 16 pixels on a high-DPI screen.
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let icon = LoadImageW(hinstance, 1 as _, IMAGE_ICON, size, size, 0);
        if !icon.is_null() {
            return icon as _;
        }

        let icon = LoadImageW(hinstance, 1 as _, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE);
        if !icon.is_null() {
            return icon as _;
        }

        let icon = LoadIconW(hinstance, 1 as _);
        if icon.is_null() {
            LoadIconW(ptr::null_mut(), IDI_APPLICATION)
        } else {
            icon
        }
    }

    unsafe fn delete_tray_icon(hwnd: HWND) {
        let data = notify_data(hwnd);
        Shell_NotifyIconW(NIM_DELETE, &data);
    }

    unsafe fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = mem::zeroed();
        data.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_UID;
        data
    }

    fn write_tip(data: &mut NOTIFYICONDATAW, tip: &str) {
        for (slot, ch) in data.szTip.iter_mut().zip(tip.encode_utf16().chain(iter::once(0))) {
            *slot = ch;
        }
    }

    unsafe fn show_tray_menu(hwnd: HWND) {
        let menu: HMENU = CreatePopupMenu();
        if menu.is_null() {
            return;
        }

        let open = wide("Open Spotlight");
        let exit = wide("Exit");
        AppendMenuW(menu, MF_STRING, MENU_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, exit.as_ptr());

        let mut point: POINT = mem::zeroed();
        GetCursorPos(&mut point);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }

    fn restore_spotlight_window() {
        if let Some(handlers) = RESTORE_HANDLER.get() {
            if let Some(handler) = handlers.lock().unwrap().as_ref() {
                handler();
            }
        }

        unsafe {
            show_spotlight_window();
        }
    }

    unsafe fn show_spotlight_window() {
        let hwnd = find_spotlight_window();
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }

    /// The app's own window. By title it could be another, such as a folder open in Explorer.
    unsafe fn find_spotlight_window() -> HWND {
        crate::window_chrome::handle()
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(iter::once(0)).collect()
    }
}

#[cfg(target_os = "windows")]
pub fn ensure_started() {
    windows_tray::ensure_started();
}

#[cfg(target_os = "windows")]
pub fn set_restore_handler<F>(handler: F)
where
    F: Fn() + Send + 'static,
{
    windows_tray::set_restore_handler(handler);
}

#[cfg(target_os = "windows")]
pub fn hide_spotlight_window() {
    windows_tray::hide_spotlight_window();
}

#[cfg(not(target_os = "windows"))]
pub fn ensure_started() {}

#[cfg(not(target_os = "windows"))]
pub fn set_restore_handler<F>(_handler: F)
where
    F: Fn() + Send + 'static,
{
}

/// A Mac has no tray to go to: the app hides, and its place in the Dock brings it back.
#[cfg(target_os = "macos")]
pub fn hide_spotlight_window() {
    crate::window_chrome::hide_app();
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn hide_spotlight_window() {}

#[cfg(target_os = "windows")]
pub fn remove_icon() {
    windows_tray::remove_icon();
}

#[cfg(not(target_os = "windows"))]
pub fn remove_icon() {}
