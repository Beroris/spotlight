//! Opens Spotlight again when it crashes.
//!
//! A crash can come from outside the app: updating the graphics driver throws away the OpenGL
//! context Slint draws with, and its renderer can't carry on without one. The crashing copy
//! saves where the music was, starts a new copy and goes. The new copy waits for it to be gone,
//! comes back the way the window was, and plays on if the music was playing on this PC. A copy
//! opened by hand still waits for play.
//!
//! A crash soon after a restart counts towards the last, and each waits longer before it starts,
//! so a driver that's still installing gets time to finish. After a few in a row it stays closed.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// `--after-crash <old process id> <restarts in a row>`, followed by any of the others.
const AFTER_CRASH_ARG: &str = "--after-crash";
/// The music was playing on this PC.
const PLAYING_ARG: &str = "--was-playing";
/// The window was in the tray.
const IN_TRAY_ARG: &str = "--was-in-tray";
/// The window was minimized.
const MINIMIZED_ARG: &str = "--was-minimized";

/// How long each restart in a row waits before it starts. One more crash after the last stays
/// closed.
const WAITS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// A copy that ran this long before it crashed starts the count again.
const RAN_LONG_ENOUGH: Duration = Duration::from_secs(180);

static STARTED: OnceLock<Instant> = OnceLock::new();
/// Restarts in a row that led to this copy.
static RESTARTS: AtomicU32 = AtomicU32::new(0);
static PLAYING_HERE: AtomicBool = AtomicBool::new(false);
/// Set on the way out, so a crash while quitting doesn't open it again.
static QUITTING: AtomicBool = AtomicBool::new(false);
/// Only one new copy, however many ways the crash is noticed.
static RESTARTED: AtomicBool = AtomicBool::new(false);
static SAVE_PLACE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// How the copy before this one was left, when this one is a restart after a crash.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AfterCrash {
    pub was_playing: bool,
    pub was_in_tray: bool,
    pub was_minimized: bool,
}

/// What a restart is asked to do, read from its command line.
#[derive(Debug, PartialEq)]
struct Restart {
    old_process: Option<u32>,
    restarts: u32,
    left: AfterCrash,
}

fn parse(args: &[String]) -> Option<Restart> {
    let at = args.iter().position(|arg| arg == AFTER_CRASH_ARG)?;
    let number = |offset: usize| args.get(at + offset).and_then(|value| value.parse::<u32>().ok());
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    Some(Restart {
        old_process: number(1),
        restarts: number(2).unwrap_or(1).max(1),
        left: AfterCrash { was_playing: has(PLAYING_ARG), was_in_tray: has(IN_TRAY_ARG), was_minimized: has(MINIMIZED_ARG) },
    })
}

/// The restarts in a row the next copy would be, or none when it's been too many.
fn next_restarts(restarts_so_far: u32, ran_for: Duration) -> Option<u32> {
    let next = if restarts_so_far == 0 || ran_for >= RAN_LONG_ENOUGH { 1 } else { restarts_so_far + 1 };
    (next as usize <= WAITS.len()).then_some(next)
}

/// Call first thing. For a restart, waits for the copy that crashed to be gone and for its
/// turn, then says how that copy was left. Also watches for crashes that aren't panics.
pub fn start(args: &[String]) -> Option<AfterCrash> {
    let _ = STARTED.set(Instant::now());
    imp::catch_native_crashes();
    let restart = parse(args)?;
    RESTARTS.store(restart.restarts, Ordering::Relaxed);
    if let Some(process_id) = restart.old_process {
        imp::wait_for_exit(process_id, Duration::from_secs(15));
    }
    std::thread::sleep(WAITS[(restart.restarts as usize - 1).min(WAITS.len() - 1)]);
    Some(restart.left)
}

/// Saves where the music is when Spotlight crashes. It runs while the app is going down, so it
/// mustn't wait on anything the crash may be holding.
pub fn save_place_with(save: impl Fn() + Send + Sync + 'static) {
    let _ = SAVE_PLACE.set(Box::new(save));
}

/// Whether this PC's player is playing, for a restart to carry on with.
pub fn note_playing_here(playing: bool) {
    PLAYING_HERE.store(playing, Ordering::Relaxed);
}

/// Spotlight is quitting on purpose (or Windows is shutting down), so nothing reopens it.
pub fn quitting() {
    QUITTING.store(true, Ordering::Relaxed);
}

/// Spotlight is about to end because something went wrong: saves where the music was and starts
/// a new copy. Does nothing while quitting or after too many restarts in a row.
pub fn restart() {
    if QUITTING.load(Ordering::Relaxed) || RESTARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let ran_for = STARTED.get().map_or(Duration::ZERO, Instant::elapsed);
    let Some(restarts) = next_restarts(RESTARTS.load(Ordering::Relaxed), ran_for) else { return };
    let was_playing = PLAYING_HERE.load(Ordering::Relaxed);
    save_place();
    let (was_in_tray, was_minimized) = imp::window_left();
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = std::process::Command::new(exe);
    command.arg(AFTER_CRASH_ARG).arg(std::process::id().to_string()).arg(restarts.to_string());
    for (on, flag) in [(was_playing, PLAYING_ARG), (was_in_tray, IN_TRAY_ARG), (was_minimized, MINIMIZED_ARG)] {
        if on {
            command.arg(flag);
        }
    }
    let _ = command.spawn();
}

/// Saves the place on a thread of its own and gives it a second: if the crash left a lock
/// held, the save waits forever, and the restart mustn't wait with it.
fn save_place() {
    let Some(save) = SAVE_PLACE.get() else { return };
    let (done, finished) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("crash save".into()).spawn(move || {
        save();
        let _ = done.send(());
    });
    if spawned.is_ok() {
        let _ = finished.recv_timeout(Duration::from_secs(1));
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::time::Duration;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, EXCEPTION_BREAKPOINT, EXCEPTION_SINGLE_STEP},
        System::{
            Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_POINTERS},
            Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        },
        UI::WindowsAndMessaging::{IsIconic, IsWindow, IsWindowVisible},
    };

    pub fn wait_for_exit(process_id: u32, timeout: Duration) {
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id);
            if process.is_null() {
                return;
            }
            WaitForSingleObject(process, timeout.as_millis().min(u128::from(u32::MAX)) as u32);
            CloseHandle(process);
        }
    }

    /// (in the tray, minimized) for the main window right now.
    pub fn window_left() -> (bool, bool) {
        let hwnd = crate::window_chrome::handle();
        if hwnd.is_null() {
            return (false, false);
        }
        unsafe {
            if IsWindow(hwnd) == 0 {
                return (false, false);
            }
            (IsWindowVisible(hwnd) == 0, IsIconic(hwnd) != 0)
        }
    }

    /// Crashes in native code, a driver's say, skip the panic hook. Windows still writes its
    /// crash dump after this, as it goes on to its own handling.
    pub fn catch_native_crashes() {
        unsafe extern "system" fn crashed(info: *const EXCEPTION_POINTERS) -> i32 {
            const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
            let record = unsafe { info.as_ref().and_then(|info| info.ExceptionRecord.as_ref()) };
            if let Some(record) = record {
                if record.ExceptionCode == EXCEPTION_BREAKPOINT || record.ExceptionCode == EXCEPTION_SINGLE_STEP {
                    return EXCEPTION_CONTINUE_SEARCH;
                }
                crate::append_crash_log(&format!(
                    "exception 0x{:08X} at {:p}",
                    record.ExceptionCode as u32, record.ExceptionAddress
                ));
            }
            super::restart();
            EXCEPTION_CONTINUE_SEARCH
        }
        unsafe {
            SetUnhandledExceptionFilter(Some(crashed));
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use std::time::Duration;

    pub fn wait_for_exit(process_id: u32, timeout: Duration) {
        // Sending no signal at all only asks whether the process is still there.
        let until = std::time::Instant::now() + timeout;
        while unsafe { libc::kill(process_id as libc::pid_t, 0) } == 0 && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    pub fn window_left() -> (bool, bool) {
        (false, false)
    }
    pub fn catch_native_crashes() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn an_ordinary_launch_is_not_a_restart() {
        assert_eq!(parse(&args(&["Spotlight.exe"])), None);
        assert_eq!(parse(&args(&["Spotlight.exe", "--after-update", "12"])), None);
    }

    #[test]
    fn a_restart_reads_how_the_old_copy_was_left() {
        let restart = parse(&args(&["Spotlight.exe", "--after-crash", "4321", "2", "--was-playing", "--was-in-tray"])).unwrap();
        assert_eq!(restart.old_process, Some(4321));
        assert_eq!(restart.restarts, 2);
        assert_eq!(restart.left, AfterCrash { was_playing: true, was_in_tray: true, was_minimized: false });
    }

    #[test]
    fn a_restart_with_missing_numbers_still_counts_as_one() {
        let restart = parse(&args(&["Spotlight.exe", "--after-crash"])).unwrap();
        assert_eq!(restart.old_process, None);
        assert_eq!(restart.restarts, 1);
        assert_eq!(restart.left, AfterCrash::default());
    }

    #[test]
    fn crashes_in_a_row_stop_after_the_last_wait() {
        let soon = Duration::from_secs(10);
        assert_eq!(next_restarts(0, soon), Some(1));
        assert_eq!(next_restarts(1, soon), Some(2));
        assert_eq!(next_restarts(4, soon), Some(5));
        assert_eq!(next_restarts(5, soon), None);
    }

    #[test]
    fn a_copy_that_ran_a_while_starts_the_count_again() {
        assert_eq!(next_restarts(5, RAN_LONG_ENOUGH), Some(1));
        assert_eq!(next_restarts(3, Duration::from_secs(600)), Some(1));
    }
}
