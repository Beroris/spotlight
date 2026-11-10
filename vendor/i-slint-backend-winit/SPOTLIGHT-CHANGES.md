# Why this crate is here

This is i-slint-backend-winit 1.16.1 from crates.io with two changes to its frame pacing:

- It asked Windows for one more frame after every change, and drew it, though nothing on screen
  had moved. Every update of the progress bar drew two or three frames instead of one.
- While the window was in the tray, a change made it ask for a frame at the monitor's refresh
  rate until the window was shown again, since a hidden window is never painted.

The changes, which need carrying over when moving to a newer Slint, unless Slint has fixed them,
are both in `frame_throttle.rs`, in `TimerBasedFrameThrottle::request_throttled_redraw`:

- The timer asks for a frame only while `pending_redraw()` is true, and stops as soon as it isn't.
- It also stops while the window isn't visible. Showing the window paints it, which draws the
  frame that was wanted.

The version stays 1.16.1, so the other Slint crates, which ask for exactly that version, still
accept it.
