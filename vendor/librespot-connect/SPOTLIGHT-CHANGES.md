# Why this crate is here

This is librespot-connect 0.8.0 from crates.io with one addition. Spotify's public API can't
reorder a queue, only start a new list, which restarts the song. So this adds a command that
reorders the player's own list directly and leaves the song alone.

The addition, which needs carrying over when moving to a newer librespot:

- `src/spirc.rs`: `SpircCommand::SetUpcoming`, `Spirc::set_upcoming`, and its branch in `handle_command`.
- `src/state/tracks.rs`: `ConnectState::set_upcoming`, `ConnectState::return_to_context` and the `set_upcoming_tests` module.

Run its tests with `cargo test -p librespot-connect set_upcoming`.
