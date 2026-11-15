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

## Keeping the songs coming up when the music moves here

Most music Spotlight starts is a plain list of songs, which librespot plays under the made-up
name `spotify:web-api`. Spotify has no such list, so when the music moved to this player from
another device, asking Spotify for it failed and only the song playing was kept: the songs
before and after it were gone. The same happened for any list Spotify couldn't give at that
moment.

The player now takes the list from what it was handed:

- `src/state/handed_over.rs` (new): `HandedOver` reads the songs played, the song playing, the
  songs coming up and the ones added to the queue, from the other device's last report of what
  it plays (`Cluster.player_state`) and from the handover itself (`TransferState`).
  `HandedOver::of` uses the report when the two agree, as it has more of the list, and
  otherwise the handover's songs coming up after the report's songs played.
  `ConnectState::carry_on_from` sets the player up with them, queued songs where they were
  among the others. Its tests are in the same file.
- `src/state.rs`: declares that module.
- `src/state/tracks.rs`: `return_to_context` is `pub(super)`, for `carry_on_from`.
- `src/spirc.rs`:
  - `playing_elsewhere` keeps the other device's last report, from `handle_cluster_update` and
    `handle_connection_id_update`. A handover to the device that already has the music reads
    this player's own state instead.
  - `handle_transfer` uses the handed-over list at once for a list Spotify can't be asked for
    (`HandedOver::can_be_asked_for`) or couldn't give before (`ContextResolver::gave_up_on`),
    and keeps it in `handed_over` as a fallback otherwise. If the list can't be carried on
    from, the song still plays, set up as before.
  - `handle_next_context` falls back to it, through `carry_on_with_what_came`, when a named
    list can't be had or applied during a handover, or when the last of its later pages
    fails and nothing is left that would finish the handover. Before, `transfer_state` stayed
    set after such a failure and was applied to whatever list came next.
  - `handle_load`, `handle_transfer` and `handle_disconnect` forget a handover that never
    finished. The first two also stop waiting for its playlist, which would otherwise
    replace the new music when it arrived.
  - `WEB_API_URI` is now one constant for the file.
- `src/context_resolver.rs`: `next_is_the_list_itself`, which tells a failure to get the list
  from one to get more pages of it or the similar songs after it, and `gave_up_on`. Its test
  is in the same file.

All are marked with a `Spotlight:` comment. A shuffled list keeps the order it arrived in
rather than being dealt again. Spotify passes on at most about 10 songs played and 80 coming
up (about 50 in the handover alone), so a longer plain list arrives without its far end.

Run its tests with `cargo test -p librespot-connect handed_over` and
`cargo test -p librespot-connect context_resolver`.
