# Why this crate is here

This is librespot-connect 0.8.0 from crates.io with one addition and one fix (at the end).
Spotify's public API can't reorder a queue, only start a new list, which restarts the song. So
this adds a command that reorders the player's own list directly and leaves the song alone.

The addition, which needs carrying over when moving to a newer librespot:

- `src/spirc.rs`: `SpircCommand::SetUpcoming`, `Spirc::set_upcoming`, and its branch in `handle_command`.
- `src/state/tracks.rs`: `ConnectState::set_upcoming`, `ConnectState::return_to_context` and the `set_upcoming_tests` module.

Run its tests with `cargo test -p librespot-connect set_upcoming`.

## Shutting down when Spotify can't be reached

Two changes in `src/spirc.rs` make sure a player that is told to stop really stops:

- `SpircCommand::Shutdown` sets `shutdown` even when telling Spotify fails, as it does without a
  network. Before, a failed disconnect left the task running.
- When the command channel closes, because the `Spirc` handle was dropped, the task shuts down.
  Before, `recv()` returned `None` straight away on every pass, so the loop spun a whole core
  until the session ended.

Both are marked with a `Spotlight:` comment.

## Saying when it stops being the active device

`handle_cluster_update` in `src/spirc.rs` has two ways of going inactive. One went through
`handle_disconnect`, which sends `SessionDisconnected`. The other, for an update with no cluster
in it, said nothing. It now sends the same event, so the app always knows whether this player is
the active device. Commands sent to it while it isn't are dropped without an error.
