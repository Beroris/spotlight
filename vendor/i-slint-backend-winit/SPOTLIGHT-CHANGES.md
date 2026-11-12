# Why this crate is here

This is i-slint-backend-winit 1.16.1 from crates.io with two changes to its frame pacing, and
one to how a window is first shown. The frame pacing:

- It asked Windows for one more frame after every change, and drew it, though nothing on screen
  had moved. Every update of the progress bar drew two or three frames instead of one.
- While the window was in the tray, a change made it ask for a frame at the monitor's refresh
  rate until the window was shown again, since a hidden window is never painted. While it was
  minimized, which Windows still counts as visible, it drew every change unseen.

The changes, which need carrying over when moving to a newer Slint, unless Slint has fixed them,
are both in `frame_throttle.rs`, in `TimerBasedFrameThrottle::request_throttled_redraw`:

- The timer asks for a frame only while `pending_redraw()` is true, and stops as soon as it isn't.
- It also stops while the window isn't visible or is minimized. Showing or restoring the window
  paints it, which draws the frame that was wanted.

Spotlight can open in the tray, as when it starts with Windows. Its window must still be made, so
the installer's request to quit, a second launch's request to show it and Windows' media controls
can reach it, but not shown. `keep_first_window_hidden` in `lib.rs` asks for that: in
`winitwindowadapter.rs`, `set_visibility` then skips `set_visible(true)` the first time the window
is shown. Slint takes it as shown, as it does a window Spotlight hides in the tray itself.

The version stays 1.16.1, so the other Slint crates, which ask for exactly that version, still
accept it.
