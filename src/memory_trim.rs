//! Takes pages nothing is using out of the app's working set.
//!
//! Starting up touches far more memory than running does, mostly in the graphics
//! driver, and those pages stay counted against the app until Windows runs short.
//! Measured on the owner's PC: 89 MB before a trim, 40 MB after, even with the UI
//! being used. A trim frees no allocations and commit stays the same: pages move to
//! Windows' standby and modified lists. Touching one again usually costs a quick soft
//! fault, or a read from disk if Windows has since needed that memory for something else.
//!
//! Drawing builds up pages too. The driver copies each frame's vertices through a 16 MB
//! ring, about 200 KB a frame on Home, and what look like its commands through two 4 MB
//! ones, so a few dozen frames fill them with data that has already been drawn and won't
//! be read again. Those go back to Windows every couple of seconds once animation stops:
//! a frame then pays about 1.5 microseconds for each page it writes, under 0.1 ms in all.
//! While an animation or a scroll runs, only memory nearing 70 MB is trimmed.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Above this, unused pages have built up again.
const LIMIT_BYTES: usize = 60 * 1024 * 1024;
/// Stops a session that truly needs more than the limit from being trimmed nonstop.
const MIN_GAP: Duration = Duration::from_secs(30);
/// This close to 70 MB, as when a very long list loads its artwork, a trim can't wait.
const URGENT_BYTES: usize = 66 * 1024 * 1024;
/// The gap for those, which doubles while trimming isn't keeping memory down.
const FIRST_URGENT_GAP: Duration = Duration::from_secs(5);
/// A slow first launch can still be settling after the close watch ends.
const SETTLING_URGENT_GAP: Duration = Duration::from_secs(1);
/// Starting up can gain memory in a moment, so it is trimmed well short of the limit.
const STARTUP_LIMIT_BYTES: usize = 50 * 1024 * 1024;
/// The graphics driver takes about 40 MB in one go, on top of whatever is in use by then.
/// Held below this until that happens, the total stays clear of 70 MB.
const BEFORE_JUMP_BYTES: usize = 24 * 1024 * 1024;
/// A trim is undone for whatever is still in use, so these ones are spaced out.
const BEFORE_JUMP_GAP: Duration = Duration::from_millis(30);
/// Enough to last until that jump. These fire as memory grows, so a slow start spends
/// them more slowly. Any more would only hold back the first page.
const BEFORE_JUMP_TRIMS: u32 = 10;

/// This many frames this close together are an animation or a scroll, even at 30 fps.
/// The progress bar draws two or three at a time, and the stage's slow drift about four
/// in this long.
const BUSY_WINDOW_MS: u64 = 200;
const BUSY_WINDOW_FRAMES: u32 = 5;
/// How long animation has to have stopped for before anything is released.
const QUIET_AFTER_MS: u64 = 3_000;
/// How often the driver's rings are handed back while frames still trickle in. Any page
/// going back is one the ring would soon reach anyway, so doing this more often adds
/// next to no faults.
const STREAMS_EVERY_MS: u64 = 2_000;
/// How long the list of rings already found is used before looking again. Each use checks
/// they are still as found, and looking takes about 4 ms.
const STREAM_SEARCH_EVERY_MS: u64 = 60_000;
/// After this long without animation, whatever else went cold goes back as well, once.
/// Measured, the next frame takes about 0.4 ms longer for it.
const SETTLED_AFTER_MS: u64 = 10_000;
/// Frames too slow to count as animation warm memory back up as well, so this many since
/// the last time earn it another go.
const PRIVATE_AGAIN_AFTER_FRAMES: u64 = 600;
/// How long the window has to have been out of sight before the rest goes back, once.
const HIDDEN_FOR_MS: u64 = 2_000;
/// A block holding at least this much in one piece, in an allocation it nearly fills,
/// and no bigger than this, is one of the driver's rings. Getting this wrong costs a
/// few soft faults: the pages keep what they hold.
const STREAM_BLOCK_MIN_BYTES: usize = 4 * 1024 * 1024;
const STREAM_BLOCK_MAX_BYTES: usize = 64 * 1024 * 1024;

static FRAMES_DRAWN: AtomicU64 = AtomicU64::new(0);
static LAST_BUSY_MS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static BUSY_WINDOW: Cell<BusyWindow> = const { Cell::new(BusyWindow { started: 0, frames: 0 }) };
}

#[derive(Clone, Copy, Default)]
struct BusyWindow {
    started: u64,
    frames: u32,
}

impl BusyWindow {
    /// Counts a frame. True if it makes this an animation.
    fn frame(&mut self, now: u64) -> bool {
        if now.saturating_sub(self.started) >= BUSY_WINDOW_MS {
            self.started = now;
            self.frames = 0;
        }
        self.frames += 1;
        self.frames >= BUSY_WINDOW_FRAMES
    }
}

fn now_ms() -> u64 {
    static CLOCK: OnceLock<Instant> = OnceLock::new();
    CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// To be called for each frame drawn.
pub fn frame_drawn() {
    FRAMES_DRAWN.fetch_add(1, Ordering::Relaxed);
    let now = now_ms();
    BUSY_WINDOW.with(|window| {
        let mut busy_window = window.get();
        if busy_window.frame(now) {
            LAST_BUSY_MS.store(now, Ordering::Relaxed);
        }
        window.set(busy_window);
    });
}

fn trim_is_due(used: usize, since_last: Duration, urgent_gap: Duration, quiet: bool) -> bool {
    (used > URGENT_BYTES && since_last >= urgent_gap) || (used > LIMIT_BYTES && since_last >= MIN_GAP && quiet)
}

fn startup_trim_is_due(used: usize, since_last: Duration, early_trims_left: u32) -> bool {
    used > STARTUP_LIMIT_BYTES || (early_trims_left > 0 && used > BEFORE_JUMP_BYTES && since_last >= BEFORE_JUMP_GAP)
}

fn is_stream_block(largest_piece: usize, allocation: usize) -> bool {
    largest_piece >= STREAM_BLOCK_MIN_BYTES
        && allocation <= STREAM_BLOCK_MAX_BYTES
        && largest_piece / 9 >= allocation / 10
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Release {
    /// The driver's rings, which look to hold only what has already been drawn.
    Streams,
    /// All private memory, leaving the app's code in place for a quick wake-up.
    Private,
}

#[derive(Default)]
struct Releases {
    streams_at: u64,
    frames_at_streams: u64,
    /// The stretch without animation that has already had its `Private` release.
    settled_after_busy: Option<u64>,
    frames_at_private: u64,
    hidden_released: bool,
}

impl Releases {
    fn private_released(&mut self, now: u64, frames: u64, last_busy: u64) -> Option<Release> {
        self.settled_after_busy = Some(last_busy);
        self.frames_at_private = frames;
        self.streams_at = now;
        self.frames_at_streams = frames;
        Some(Release::Private)
    }

    fn due(&mut self, now: u64, frames: u64, last_busy: u64, hidden_since: Option<u64>) -> Option<Release> {
        if let Some(since) = hidden_since {
            if !self.hidden_released && now.saturating_sub(since) >= HIDDEN_FOR_MS {
                self.hidden_released = true;
                return self.private_released(now, frames, last_busy);
            }
        } else {
            self.hidden_released = false;
        }
        let quiet_for = now.saturating_sub(last_busy);
        if quiet_for < QUIET_AFTER_MS {
            return None;
        }
        let warmed_up = self.settled_after_busy != Some(last_busy)
            || frames.wrapping_sub(self.frames_at_private) >= PRIVATE_AGAIN_AFTER_FRAMES;
        if quiet_for >= SETTLED_AFTER_MS && warmed_up {
            return self.private_released(now, frames, last_busy);
        }
        if frames != self.frames_at_streams && now.saturating_sub(self.streams_at) >= STREAMS_EVERY_MS {
            self.streams_at = now;
            self.frames_at_streams = frames;
            return Some(Release::Streams);
        }
        None
    }
}

#[cfg(target_os = "windows")]
mod pages {
    use super::is_stream_block;
    use std::ffi::c_void;
    use windows_sys::Win32::System::{
        Memory::{VirtualQuery, VirtualUnlock, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_FREE, MEM_PRIVATE, PAGE_READWRITE},
        ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION},
        Threading::GetCurrentProcess,
    };

    const PAGE_BYTES: usize = 4096;

    extern "C" {
        /// mimalloc's: tidies what this thread owns, then gives Windows back memory freed
        /// anywhere in the app. Without `force`, only what has waited out its delay.
        pub(super) fn mi_collect(force: bool);
    }

    /// Unlocking memory that isn't locked takes it out of the working set and leaves what
    /// it holds alone, which Windows documents. It reports an error, as nothing was locked.
    fn release_range(base: *mut c_void, size: usize) {
        unsafe { VirtualUnlock(base, size) };
    }

    /// From what Windows reports about a page: it is in memory, and locked there.
    pub(super) fn is_locked(attributes: usize) -> bool {
        const IN_MEMORY: usize = 1;
        const LOCKED: usize = 1 << 22;
        attributes & IN_MEMORY != 0 && attributes & LOCKED != 0
    }

    /// Windows or a driver locked these on purpose, and unlocking would undo that.
    /// If Windows won't say, the answer is yes so the range is left alone.
    fn holds_locked_page(base: *mut c_void, size: usize) -> bool {
        const ENTRY_BYTES: usize = std::mem::size_of::<PSAPI_WORKING_SET_EX_INFORMATION>();
        let mut entries: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
        let mut page = base as usize;
        let end = page + size;
        while page < end {
            let count = ((end - page) / PAGE_BYTES).min(entries.len());
            if count == 0 {
                return false;
            }
            for (index, entry) in entries[..count].iter_mut().enumerate() {
                entry.VirtualAddress = (page + index * PAGE_BYTES) as *mut c_void;
            }
            let read = unsafe {
                K32QueryWorkingSetEx(GetCurrentProcess(), entries.as_mut_ptr().cast(), (count * ENTRY_BYTES) as u32)
            };
            if read == 0 || entries[..count].iter().any(|entry| is_locked(unsafe { entry.VirtualAttributes.Flags })) {
                return true;
            }
            page += count * PAGE_BYTES;
        }
        false
    }

    fn memory_regions() -> impl Iterator<Item = MEMORY_BASIC_INFORMATION> {
        let mut address = 0usize;
        std::iter::from_fn(move || {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let read = unsafe { VirtualQuery(address as *const _, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
            let next = (info.BaseAddress as usize).wrapping_add(info.RegionSize);
            if read == 0 || next <= address {
                return None;
            }
            address = next;
            Some(info)
        })
    }

    /// Write-combined memory isn't in here, as it holds little.
    fn is_plain_private(info: &MEMORY_BASIC_INFORMATION) -> bool {
        info.State == MEM_COMMIT && info.Type == MEM_PRIVATE && info.Protect == PAGE_READWRITE
    }

    pub(super) fn release_all_private() {
        for region in memory_regions().filter(is_plain_private) {
            if !holds_locked_page(region.BaseAddress, region.RegionSize) {
                release_range(region.BaseAddress, region.RegionSize);
            }
        }
    }

    /// One of the driver's rings, as found.
    pub(super) struct Stream {
        allocation: *mut c_void,
        pub(super) base: *mut c_void,
        size: usize,
    }

    impl Stream {
        /// The driver can replace a ring, or something can lock part of it, and then it is no
        /// longer this one's to hand back.
        pub(super) fn is_as_found(&self) -> bool {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let read = unsafe { VirtualQuery(self.base, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
            read != 0
                && info.BaseAddress == self.base
                && info.AllocationBase == self.allocation
                && info.RegionSize == self.size
                && is_plain_private(&info)
                && !holds_locked_page(self.base, self.size)
        }

        pub(super) fn release(&self) {
            release_range(self.base, self.size);
        }
    }

    pub(super) fn find_streams() -> Vec<Stream> {
        let mut found = Vec::new();
        let mut allocation = (std::ptr::null_mut(), 0usize);
        let mut largest = (std::ptr::null_mut(), 0usize);
        let mut finish = |allocation: (*mut c_void, usize), largest: (*mut c_void, usize)| {
            if is_stream_block(largest.1, allocation.1) && !holds_locked_page(largest.0, largest.1) {
                found.push(Stream { allocation: allocation.0, base: largest.0, size: largest.1 });
            }
        };
        for region in memory_regions() {
            if region.AllocationBase != allocation.0 {
                finish(allocation, largest);
                allocation = (region.AllocationBase, 0);
                largest = (std::ptr::null_mut(), 0);
            }
            if region.State != MEM_FREE {
                allocation.1 += region.RegionSize;
            }
            if is_plain_private(&region) && region.RegionSize > largest.1 {
                largest = (region.BaseAddress, region.RegionSize);
            }
        }
        finish(allocation, largest);
        found
    }
}

#[cfg(target_os = "windows")]
pub fn start() {
    use pages::mi_collect;
    use windows_sys::Win32::System::{
        ProcessStatus::{K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2},
        Threading::GetCurrentProcess,
    };

    const CHECK_EVERY: Duration = Duration::from_secs(1);
    /// Watched very closely, as starting up can gain 40 MB in a moment.
    const STARTUP: Duration = Duration::from_secs(2);
    const STARTUP_CHECK_EVERY: Duration = Duration::from_millis(5);
    /// Counted from launch, as the first page's artwork arrives.
    const SETTLING_TRIMS: [Duration; 2] = [Duration::from_secs(6), Duration::from_secs(16)];
    const SETTLING_ENDS: Duration = SETTLING_TRIMS[SETTLING_TRIMS.len() - 1];
    /// One still waiting for animation to stop this long after the last was due is dropped.
    /// The releases look after memory by then, and they leave the app's code in place.
    const SETTLING_WAITS_AT_MOST: Duration = Duration::from_secs(5);

    fn trim() {
        unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
    }

    fn private_working_set() -> Option<usize> {
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS_EX2 = std::mem::zeroed();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
            counters.cb = size;
            // Windows versions without this figure refuse the size, and only the trims at start run.
            let ok = K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size);
            (ok != 0).then_some(counters.PrivateWorkingSetSize)
        }
    }

    let _ = std::thread::Builder::new()
        .name("memory-trim".into())
        .stack_size(128 * 1024)
        .spawn(|| {
            // mi_collect does nothing on a thread that hasn't allocated yet.
            std::hint::black_box(Box::new(0u8));
            let started = Instant::now();
            let mut last_trim = started;
            let mut early_trims_left = BEFORE_JUMP_TRIMS;
            while started.elapsed() < STARTUP {
                std::thread::sleep(STARTUP_CHECK_EVERY);
                let Some(used) = private_working_set() else { continue };
                if startup_trim_is_due(used, last_trim.elapsed(), early_trims_left) {
                    trim();
                    last_trim = Instant::now();
                    early_trims_left = early_trims_left.saturating_sub(1);
                }
            }
            trim();
            last_trim = Instant::now();
            let mut urgent_gap = FIRST_URGENT_GAP;
            let mut settling = SETTLING_TRIMS.iter();
            let mut next_settling = settling.next();
            let mut releases = Releases::default();
            let mut hidden_since = None;
            let mut streams = Vec::new();
            let mut next_stream_search = 0;
            loop {
                std::thread::sleep(CHECK_EVERY);
                let now = now_ms();
                let frames = FRAMES_DRAWN.load(Ordering::Relaxed);
                let last_busy = LAST_BUSY_MS.load(Ordering::Relaxed);
                let quiet = now.saturating_sub(last_busy) >= QUIET_AFTER_MS;
                if started.elapsed() >= SETTLING_ENDS + SETTLING_WAITS_AT_MOST {
                    next_settling = None;
                }
                if quiet && next_settling.is_some_and(|at| started.elapsed() >= *at) {
                    // One trim covers any others that are due by now as well.
                    while next_settling.is_some_and(|at| started.elapsed() >= *at) {
                        next_settling = settling.next();
                    }
                    trim();
                    last_trim = Instant::now();
                    continue;
                }
                let still_settling = started.elapsed() < SETTLING_ENDS;
                if let Some(used) = private_working_set() {
                    let gap = if still_settling { SETTLING_URGENT_GAP } else { urgent_gap };
                    if trim_is_due(used, last_trim.elapsed(), gap, quiet) {
                        trim();
                        last_trim = Instant::now();
                        if used > URGENT_BYTES && !still_settling {
                            urgent_gap = (urgent_gap * 2).min(MIN_GAP);
                        }
                        continue;
                    } else if used <= LIMIT_BYTES && last_trim.elapsed() >= MIN_GAP {
                        urgent_gap = FIRST_URGENT_GAP;
                    }
                }
                if still_settling {
                    continue;
                }
                let hidden = crate::window_chrome::window_is_hidden();
                hidden_since = if hidden { hidden_since.or(Some(now)) } else { None };
                match releases.due(now, frames, last_busy, hidden_since) {
                    Some(Release::Streams) => {
                        if now >= next_stream_search {
                            streams = pages::find_streams();
                            next_stream_search = now + STREAM_SEARCH_EVERY_MS;
                        }
                        let found = streams.len();
                        streams.retain(pages::Stream::is_as_found);
                        if streams.len() != found {
                            next_stream_search = now;
                        }
                        for stream in &streams {
                            stream.release();
                        }
                    }
                    Some(Release::Private) => {
                        unsafe { mi_collect(true) };
                        pages::release_all_private();
                    }
                    None if now.saturating_sub(last_busy) >= QUIET_AFTER_MS => unsafe { mi_collect(false) },
                    None => {}
                }
            }
        });
}

#[cfg(not(target_os = "windows"))]
pub fn start() {}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: usize = 1024 * 1024;

    #[test]
    fn starting_up_memory_is_held_low_for_a_limited_number_of_trims() {
        assert!(!startup_trim_is_due(24 * MB, Duration::from_secs(1), 1));
        assert!(!startup_trim_is_due(25 * MB, Duration::from_millis(29), 1));
        assert!(startup_trim_is_due(25 * MB, BEFORE_JUMP_GAP, 1));
        assert!(!startup_trim_is_due(25 * MB, Duration::from_secs(1), 0));
    }

    #[test]
    fn starting_up_past_the_limit_a_trim_never_waits() {
        assert!(startup_trim_is_due(51 * MB, Duration::ZERO, 1));
        assert!(startup_trim_is_due(51 * MB, Duration::ZERO, 0));
    }

    #[test]
    fn below_the_limit_nothing_is_trimmed_however_long_it_has_been() {
        assert!(!trim_is_due(60 * MB, Duration::from_secs(3600), FIRST_URGENT_GAP, true));
    }

    #[test]
    fn over_the_limit_a_trim_waits_for_the_long_gap() {
        assert!(!trim_is_due(61 * MB, Duration::from_secs(29), FIRST_URGENT_GAP, true));
        assert!(trim_is_due(61 * MB, MIN_GAP, FIRST_URGENT_GAP, true));
    }

    #[test]
    fn over_the_limit_a_trim_waits_for_animation_to_stop() {
        assert!(!trim_is_due(61 * MB, MIN_GAP, FIRST_URGENT_GAP, false));
    }

    #[test]
    fn near_70_mb_a_trim_only_waits_for_the_short_gap() {
        assert!(!trim_is_due(67 * MB, Duration::from_secs(4), FIRST_URGENT_GAP, true));
        assert!(trim_is_due(67 * MB, FIRST_URGENT_GAP, FIRST_URGENT_GAP, true));
        assert!(!trim_is_due(67 * MB, FIRST_URGENT_GAP, FIRST_URGENT_GAP * 2, true));
    }

    #[test]
    fn near_70_mb_a_trim_goes_ahead_even_during_animation() {
        assert!(trim_is_due(67 * MB, FIRST_URGENT_GAP, FIRST_URGENT_GAP, false));
    }

    #[test]
    fn the_progress_bars_few_frames_at_a_time_are_not_an_animation() {
        let mut window = BusyWindow::default();
        for tick in 0..40u64 {
            for frame in 0..3 {
                assert!(!window.frame(1_000 + tick * 250 + frame));
            }
        }
    }

    #[test]
    fn the_stages_slow_drift_is_not_an_animation() {
        let mut window = BusyWindow::default();
        for frame in 0..200u64 {
            assert!(!window.frame(1_000 + frame * 1_000 / 19));
        }
    }

    #[test]
    fn an_animation_is_noticed_at_any_refresh_rate() {
        for hertz in [30u64, 60, 144, 300] {
            let mut window = BusyWindow::default();
            assert!((0..hertz).any(|frame| window.frame(10_000 + frame * 1_000 / hertz)), "{hertz} Hz");
        }
    }

    #[test]
    fn the_rings_are_told_apart_from_the_heap_and_small_blocks() {
        assert!(is_stream_block(16 * MB, 16 * MB));
        assert!(is_stream_block(4 * MB, 4 * MB + 64 * 1024));
        // The heap's large reservation, of which only parts are committed.
        assert!(!is_stream_block(32 * MB, 1024 * MB));
        assert!(!is_stream_block(4 * MB, 16 * MB));
        assert!(!is_stream_block(MB, MB));
    }

    #[test]
    fn nothing_is_released_during_an_animation() {
        let mut releases = Releases::default();
        assert_eq!(releases.due(100_000, 900, 100_000, None), None);
        assert_eq!(releases.due(100_000, 901, 100_000 - QUIET_AFTER_MS + 1, None), None);
    }

    #[test]
    fn the_rings_go_back_every_couple_of_seconds_while_frames_trickle_in() {
        let mut releases = Releases::default();
        assert_eq!(releases.due(10_000, 100, 5_000, None), Some(Release::Streams));
        assert_eq!(releases.due(11_000, 110, 5_000, None), None);
        assert_eq!(releases.due(12_000, 120, 5_000, None), Some(Release::Streams));
    }

    #[test]
    fn with_no_frames_there_is_nothing_to_hand_back() {
        let mut releases = Releases::default();
        assert_eq!(releases.due(10_000, 100, 5_000, None), Some(Release::Streams));
        assert_eq!(releases.due(14_000, 100, 5_000, None), None);
    }

    #[test]
    fn a_stretch_without_animation_releases_the_rest_only_once() {
        let mut releases = Releases::default();
        let busy = 1_000;
        assert_eq!(releases.due(busy + SETTLED_AFTER_MS, 100, busy, None), Some(Release::Private));
        assert_eq!(releases.due(busy + SETTLED_AFTER_MS + 1_000, 100, busy, None), None);
        let later = busy + SETTLED_AFTER_MS + 100_000;
        assert_eq!(releases.due(later, 200, later, None), None);
        assert_eq!(releases.due(later + SETTLED_AFTER_MS, 200, later, None), Some(Release::Private));
    }

    #[test]
    fn frames_that_keep_coming_earn_the_rest_another_release() {
        let mut releases = Releases::default();
        let busy = 1_000;
        let settled = busy + SETTLED_AFTER_MS;
        assert_eq!(releases.due(settled, 100, busy, None), Some(Release::Private));
        assert_eq!(releases.due(settled + 2_000, 100 + PRIVATE_AGAIN_AFTER_FRAMES - 1, busy, None), Some(Release::Streams));
        assert_eq!(releases.due(settled + 4_000, 100 + PRIVATE_AGAIN_AFTER_FRAMES, busy, None), Some(Release::Private));
        assert_eq!(releases.due(settled + 6_000, 110 + PRIVATE_AGAIN_AFTER_FRAMES, busy, None), Some(Release::Streams));
    }

    #[test]
    fn out_of_sight_the_rest_goes_back_once_per_time_hidden() {
        let mut releases = Releases::default();
        assert_eq!(releases.due(50_000, 100, 49_000, Some(50_000 - HIDDEN_FOR_MS + 1)), None);
        assert_eq!(releases.due(50_000, 100, 49_000, Some(50_000 - HIDDEN_FOR_MS)), Some(Release::Private));
        assert_eq!(releases.due(60_000, 100, 49_000, Some(48_000)), None);
        assert_eq!(releases.due(61_000, 200, 61_000, None), None);
        assert_eq!(releases.due(70_000, 200, 69_000, Some(68_000)), Some(Release::Private));
    }

    #[test]
    fn out_of_sight_the_rings_still_go_back_if_the_progress_bar_is_drawn() {
        let mut releases = Releases::default();
        assert_eq!(releases.due(50_000, 100, 40_000, Some(48_000)), Some(Release::Private));
        assert_eq!(releases.due(51_000, 101, 40_000, Some(48_000)), None);
        assert_eq!(releases.due(52_000, 102, 40_000, Some(48_000)), Some(Release::Streams));
    }

    #[cfg(target_os = "windows")]
    mod on_windows {
        use super::super::pages;
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::System::{
            Memory::{VirtualAlloc, VirtualFree, VirtualLock, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE},
            ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION},
            Threading::GetCurrentProcess,
        };

        const PAGE: usize = 4096;

        /// Handing memory back reaches every test's memory in this process, and can undo a
        /// lock another test has just taken, so these take turns.
        static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn alone() -> std::sync::MutexGuard<'static, ()> {
            ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn attributes(address: *mut u8) -> usize {
            unsafe {
                let mut entry: PSAPI_WORKING_SET_EX_INFORMATION = std::mem::zeroed();
                entry.VirtualAddress = address.cast();
                let size = std::mem::size_of::<PSAPI_WORKING_SET_EX_INFORMATION>() as u32;
                assert_ne!(K32QueryWorkingSetEx(GetCurrentProcess(), &mut entry as *mut _ as *mut c_void, size), 0);
                entry.VirtualAttributes.Flags
            }
        }

        fn in_memory(address: *mut u8) -> bool {
            attributes(address) & 1 != 0
        }

        fn block(size: usize, fill: u8) -> *mut u8 {
            unsafe {
                let block = VirtualAlloc(std::ptr::null(), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as *mut u8;
                assert!(!block.is_null());
                block.write_bytes(fill, size);
                block
            }
        }

        #[test]
        fn memory_handed_back_keeps_what_it_holds_and_locked_pages_stay_locked() {
            let _alone = alone();
            unsafe {
                let plain = block(64 * PAGE, 7);
                let holding = block(64 * PAGE, 9);
                assert_ne!(VirtualLock(holding.cast(), PAGE), 0);
                assert!(pages::is_locked(attributes(holding)));
                pages::release_all_private();
                assert!(!in_memory(plain), "should have gone back");
                assert_eq!(*plain.add(3 * PAGE), 7);
                assert!(pages::is_locked(attributes(holding)), "the lock should have stayed");
                assert!(in_memory(holding.add(10 * PAGE)), "its whole region should be left alone");
                VirtualFree(plain.cast(), 0, MEM_RELEASE);
                VirtualFree(holding.cast(), 0, MEM_RELEASE);
            }
        }

        #[test]
        fn a_ring_is_found_handed_back_and_noticed_once_it_is_gone() {
            let _alone = alone();
            unsafe {
                let size = 8 * 1024 * 1024;
                let ring = block(size, 3);
                let found = pages::find_streams();
                let stream = found.iter().find(|stream| stream.base == ring.cast()).expect("the ring");
                assert!(stream.is_as_found());
                stream.release();
                assert!(!in_memory(ring), "should have gone back");
                assert_eq!(*ring.add(size / 2), 3);
                VirtualFree(ring.cast(), 0, MEM_RELEASE);
                assert!(!stream.is_as_found());
            }
        }

        #[test]
        fn a_ring_locked_after_it_was_found_is_left_alone() {
            let _alone = alone();
            unsafe {
                let size = 8 * 1024 * 1024;
                let ring = block(size, 5);
                let found = pages::find_streams();
                let stream = found.iter().find(|stream| stream.base == ring.cast()).expect("the ring");
                assert!(stream.is_as_found());
                assert_ne!(VirtualLock(ring.add(size / 2).cast(), PAGE), 0);
                assert!(!stream.is_as_found(), "a locked page should keep it from going back");
                VirtualFree(ring.cast(), 0, MEM_RELEASE);
            }
        }

        /// Underruns and the longest wait between audio callbacks in microseconds.
        fn silent_stream_for(seconds: u32, releasing: bool) -> (u64, u64) {
            let device = crate::wasapi::default_device_id().expect("a default output device");
            let longest = Arc::new(AtomicU64::new(0));
            let longest_seen = Arc::clone(&longest);
            let mut stream = crate::wasapi::Stream::open(device, move |_rate| {
                let mut memory = vec![1u8; 8 * 1024 * 1024];
                let mut at = 0;
                let mut last: Option<Instant> = None;
                crate::wasapi::AlwaysPlaying(move |samples: &mut [f32]| {
                    samples.fill(0.0);
                    // About 400 KB, more than decoding a callback's worth of music touches.
                    for _ in 0..100 {
                        memory[at] = memory[at].wrapping_add(1);
                        at = (at + PAGE) % memory.len();
                    }
                    let now = Instant::now();
                    if let Some(last) = last {
                        longest_seen.fetch_max(now.duration_since(last).as_micros() as u64, Ordering::Relaxed);
                    }
                    last = Some(now);
                })
            })
            .expect("the stream opens");
            stream.begin();
            std::thread::sleep(Duration::from_secs(1));
            let before = crate::wasapi::UNDERRUNS.load(Ordering::Relaxed);
            longest.store(0, Ordering::Relaxed);
            for _ in 0..seconds {
                std::thread::sleep(Duration::from_secs(1));
                if releasing {
                    unsafe { pages::mi_collect(true) };
                    pages::release_all_private();
                }
            }
            assert!(!stream.is_broken());
            (crate::wasapi::UNDERRUNS.load(Ordering::Relaxed) - before, longest.load(Ordering::Relaxed))
        }

        /// Whether handing memory back can be heard. Plays silence on the default device, so
        /// nothing is audible and playback is left alone, and hands everything back once a
        /// second, far more often than the app does:
        /// `cargo test --release audio_keeps_up -- --ignored --nocapture`
        #[test]
        #[ignore = "needs a real audio device"]
        fn audio_keeps_up_while_memory_goes_back() {
            let _alone = alone();
            for (label, releasing) in [("nothing going back", false), ("going back each second", true), ("nothing going back", false)] {
                let (underruns, longest) = silent_stream_for(60, releasing);
                println!("{label}: {underruns} underruns in 60 s, longest wait between callbacks {:.1} ms", longest as f64 / 1000.0);
            }
        }
    }
}
