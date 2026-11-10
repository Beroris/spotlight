//! Previous, play or pause, and next under Spotlight's preview when the pointer rests on its
//! taskbar button. The icons are drawn here, at the size Windows asks for, so they are sharp
//! at any scale and nothing extra ships with the app.

/// The shapes, in a 16 by 16 square.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Glyph {
    Previous,
    Play,
    Pause,
    Next,
}

type Point = (f32, f32);

fn triangle(a: Point, b: Point, c: Point, x: f32, y: f32) -> bool {
    let side = |(x1, y1): Point, (x2, y2): Point| (x2 - x1) * (y - y1) - (y2 - y1) * (x - x1);
    let (d1, d2, d3) = (side(a, b), side(b, c), side(c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

fn bar(left: f32, right: f32, x: f32, y: f32) -> bool {
    (left..=right).contains(&x) && (2.5..=13.5).contains(&y)
}

fn covers(glyph: Glyph, x: f32, y: f32) -> bool {
    match glyph {
        Glyph::Play => triangle((4.0, 2.5), (13.5, 8.0), (4.0, 13.5), x, y),
        Glyph::Pause => bar(3.5, 6.5, x, y) || bar(9.5, 12.5, x, y),
        Glyph::Next => triangle((2.5, 2.5), (11.0, 8.0), (2.5, 13.5), x, y) || bar(11.5, 13.5, x, y),
        Glyph::Previous => triangle((13.5, 2.5), (5.0, 8.0), (13.5, 13.5), x, y) || bar(2.5, 4.5, x, y),
    }
}

/// How much of each pixel the shape covers, row by row from the top, at `size` pixels square.
/// Each pixel is sampled four by four, which smooths the edges.
pub fn coverage(glyph: Glyph, size: usize) -> Vec<u8> {
    const SAMPLES: usize = 4;
    let scale = 16.0 / size as f32;
    let mut out = Vec::with_capacity(size * size);
    for row in 0..size {
        for column in 0..size {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (column as f32 + (sx as f32 + 0.5) / SAMPLES as f32) * scale;
                    let y = (row as f32 + (sy as f32 + 0.5) / SAMPLES as f32) * scale;
                    hits += usize::from(covers(glyph, x, y));
                }
            }
            out.push((hits * 255 / (SAMPLES * SAMPLES)) as u8);
        }
    }
    out
}

#[cfg(target_os = "windows")]
pub use imp::{handle_message, install, show_playing};

#[cfg(not(target_os = "windows"))]
pub fn install(_ui: &crate::AppWindow, _window: isize) {}
#[cfg(not(target_os = "windows"))]
pub fn show_playing(_has_song: bool, _playing: bool) {}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::{
        cell::RefCell,
        sync::atomic::{AtomicIsize, AtomicU32, Ordering},
    };

    use slint::ComponentHandle;
    use windows::{
        core::w,
        Win32::{
            Foundation::HWND,
            System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED},
            UI::{
                Shell::{ITaskbarList3, TaskbarList, THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THB_FLAGS, THB_ICON, THB_TOOLTIP, THUMBBUTTON},
                WindowsAndMessaging::{RegisterWindowMessageW, HICON},
            },
        },
    };
    use windows_sys::Win32::{
        Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS},
        System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
        UI::{
            HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
            WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, ICONINFO, SM_CXSMICON, WM_COMMAND},
        },
    };

    const PREVIOUS: u32 = 1;
    const PLAY_PAUSE: u32 = 2;
    const NEXT: u32 = 3;

    /// Sent by Explorer when the taskbar button is made, which is also after Explorer restarts.
    static BUTTON_CREATED: AtomicU32 = AtomicU32::new(0);
    /// The window, readable while the buttons are being sent.
    static WINDOW: AtomicIsize = AtomicIsize::new(0);

    struct Buttons {
        ui: slint::Weak<crate::AppWindow>,
        window: isize,
        list: Option<ITaskbarList3>,
        icons: Vec<(Glyph, isize)>,
        added: bool,
        has_song: bool,
        playing: bool,
    }

    thread_local! {
        static BUTTONS: RefCell<Option<Buttons>> = const { RefCell::new(None) };
    }

    /// The taskbar is light or dark on its own, apart from apps.
    fn taskbar_is_light() -> bool {
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
        let name: Vec<u16> = "SystemUsesLightTheme\0".encode_utf16().collect();
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut value as *mut u32 as *mut _, &mut size)
        };
        status == 0 && value == 1
    }

    fn make_icon(glyph: Glyph, size: usize, light: bool) -> isize {
        let alpha = coverage(glyph, size);
        let ink: u32 = if light { 0x00_1a_1a_1a } else { 0x00_ff_ff_ff };
        unsafe {
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                // Negative: rows from the top.
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let color = CreateDIBSection(std::ptr::null_mut(), &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if color.is_null() || bits.is_null() {
                return 0;
            }
            let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, size * size);
            for (pixel, coverage) in pixels.iter_mut().zip(alpha) {
                *pixel = (u32::from(coverage) << 24) | ink;
            }
            let mask = CreateBitmap(size as i32, size as i32, 1, 1, std::ptr::null());
            let icon_info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
            let icon = CreateIconIndirect(&icon_info);
            DeleteObject(color);
            DeleteObject(mask);
            icon as isize
        }
    }

    fn tip(text: &str) -> [u16; 260] {
        let mut out = [0u16; 260];
        for (slot, unit) in out.iter_mut().zip(text.encode_utf16().take(259)) {
            *slot = unit;
        }
        out
    }

    impl Buttons {
        fn icon(&self, glyph: Glyph) -> HICON {
            HICON(self.icons.iter().find(|(shape, _)| *shape == glyph).map_or(0, |(_, icon)| *icon))
        }

        fn row(&self) -> [THUMBBUTTON; 3] {
            let flags = if self.has_song { THBF_ENABLED } else { THBF_DISABLED };
            let button = |id, glyph, text: &str| THUMBBUTTON {
                dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
                iId: id,
                iBitmap: 0,
                hIcon: self.icon(glyph),
                szTip: tip(text),
                dwFlags: flags,
            };
            [
                button(PREVIOUS, Glyph::Previous, "Previous"),
                if self.playing { button(PLAY_PAUSE, Glyph::Pause, "Pause") } else { button(PLAY_PAUSE, Glyph::Play, "Play") },
                button(NEXT, Glyph::Next, "Next"),
            ]
        }

        /// Puts the buttons on the taskbar's preview, or brings them up to date.
        fn send(&mut self) {
            if self.list.is_none() {
                self.list = unsafe { CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) }
                    .ok()
                    .filter(|list| unsafe { list.HrInit() }.is_ok());
            }
            if self.icons.is_empty() {
                let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(self.window as _)) }.max(16) as usize;
                let light = taskbar_is_light();
                self.icons = [Glyph::Previous, Glyph::Play, Glyph::Pause, Glyph::Next]
                    .into_iter()
                    .map(|glyph| (glyph, make_icon(glyph, size, light)))
                    .collect();
            }
            let Some(list) = self.list.as_ref() else { return };
            let row = self.row();
            let window = HWND(self.window);
            let sent = unsafe {
                if self.added {
                    list.ThumbBarUpdateButtons(window, &row)
                } else {
                    list.ThumbBarAddButtons(window, &row)
                }
            };
            // Before the taskbar button exists this fails, and the message below tries again.
            if sent.is_ok() {
                self.added = true;
            }
        }

        fn forget_icons(&mut self) {
            for (_, icon) in self.icons.drain(..) {
                if icon != 0 {
                    unsafe { DestroyIcon(icon as _) };
                }
            }
        }
    }

    /// Call on the UI thread once the window exists.
    pub fn install(ui: &crate::AppWindow, window: isize) {
        unsafe {
            // Already done by the window's thread in practice. Harmless if so.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            BUTTON_CREATED.store(RegisterWindowMessageW(w!("TaskbarButtonCreated")), Ordering::Relaxed);
        }
        WINDOW.store(window, Ordering::Relaxed);
        BUTTONS.with(|cell| {
            let mut buttons = Buttons { ui: ui.as_weak(), window, list: None, icons: Vec::new(), added: false, has_song: false, playing: false };
            buttons.send();
            *cell.borrow_mut() = Some(buttons);
        });
    }

    /// Shows pause while playing and play otherwise. The buttons can't be pressed without a song.
    pub fn show_playing(has_song: bool, playing: bool) {
        BUTTONS.with(|cell| {
            // Talking to the taskbar lets window messages in while it waits, and one of those
            // can land here while the buttons are still being sent. It tries again shortly.
            let Ok(mut cell) = cell.try_borrow_mut() else {
                slint::Timer::single_shot(std::time::Duration::from_millis(16), move || show_playing(has_song, playing));
                return;
            };
            let Some(buttons) = cell.as_mut() else { return };
            if buttons.has_song == has_song && buttons.playing == playing && buttons.added {
                return;
            }
            buttons.has_song = has_song;
            buttons.playing = playing;
            buttons.send();
        });
    }

    /// From the window's message handler: the buttons' presses, and the taskbar button being
    /// made again. Returns the result for messages that are handled here.
    pub fn handle_message(message: u32, wparam: usize) -> Option<isize> {
        let created = BUTTON_CREATED.load(Ordering::Relaxed);
        if created != 0 && message == created {
            BUTTONS.with(|cell| match cell.try_borrow_mut() {
                Ok(mut cell) => {
                    if let Some(buttons) = cell.as_mut() {
                        // Explorer started again, so it has none of what it was given.
                        buttons.added = false;
                        buttons.list = None;
                        buttons.forget_icons();
                        buttons.send();
                    }
                }
                // It came while the buttons were being sent, so it waits its turn in the queue.
                Err(_) => unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(WINDOW.load(Ordering::Relaxed) as _, message, 0, 0);
                },
            });
            return Some(0);
        }
        if message != WM_COMMAND || (wparam >> 16) as u32 & 0xffff != THBN_CLICKED {
            return None;
        }
        let ui = BUTTONS.with(|cell| cell.try_borrow().ok().and_then(|cell| cell.as_ref().and_then(|buttons| buttons.ui.upgrade())))?;
        match wparam as u32 & 0xffff {
            PREVIOUS => ui.invoke_previous_track(),
            PLAY_PAUSE => ui.invoke_play_pause(),
            NEXT => ui.invoke_next_track(),
            _ => return None,
        }
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_are_solid_inside_clear_outside_and_soft_on_slants() {
        for size in [16, 20, 24, 32] {
            for glyph in [Glyph::Previous, Glyph::Play, Glyph::Pause, Glyph::Next] {
                let pixels = coverage(glyph, size);
                assert_eq!(pixels.len(), size * size);
                assert_eq!(pixels[0], 0, "{glyph:?} at {size}: corner");
                assert!(pixels.iter().any(|&a| a == 255), "{glyph:?} at {size}: nothing solid");
                // The bars of pause can land on whole pixels, and are crisp there; slants can't.
                if glyph != Glyph::Pause {
                    assert!(pixels.iter().any(|&a| a > 0 && a < 255), "{glyph:?} at {size}: hard edges");
                }
            }
        }
        // The pause bars have a gap between them.
        let pause = coverage(Glyph::Pause, 16);
        assert_eq!(pause[8 * 16 + 8], 0);
        assert_eq!(pause[8 * 16 + 5], 255);
    }
}
