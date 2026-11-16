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

## Playing the songs a play command brings when its list can't be had

A play command names a list, brings songs along, or both. A list that was named and isn't
the one already here was asked for from Spotify, also when the name was one Spotify has no
list for. Asking failed, the command failed with "context is not available", and nothing
started: Spotify answered the app with "Restriction violated". This was seen once in the
app's log, for a start with a plain list of songs. The log doesn't show which name came; the
`-` Spotify uses for a plain list is the likely one.

In `src/spirc.rs`:

- `named_or_brought` decides what a play command plays. A name Spotify can't be asked for
  plays the songs brought along, as a command with no name always did, and the log says
  which name came. A search plays as before, and so does a song picked in the list this
  player already has. A command that names the list here but brings songs that aren't all
  in it (`all_in_list`) is a new list under the same made-up name, and its songs play.
- `handle_load` takes the songs brought (`brought`), as they came. When the list that was
  named was asked for and couldn't be had, and no list is left to play from, those songs
  play and the log says which name failed. A command whose name and page are both unusable
  still fails before that.
- Songs brought are loaded as they came, with the ids Spotify knows them by. Before, a
  command with no name was loaded from the songs' uris alone and the ids put back by song,
  which gave the first of two alike the id of the second.
- Starting a list of songs no longer leaves the list before it being asked for: its later
  pages and the similar songs after it would have been added to the new list.
- `load_context_from_uri` only plays from the list already here when there is one. A
  handover names its playlist before it has it, so the same playlist asked for again after
  that failed was not fetched at all.

Not changed: a list Spotify couldn't give is never asked for again by that player.
`ContextResolver::add` means to try again after an hour but measures the time backwards
(`i.duration_since(Instant::now())`), so it never does. `gave_up_on` goes by the same rule.

Run its tests with `cargo test -p librespot-connect spirc::tests`.

## The volume: set while the music is elsewhere, and said when set from elsewhere

`handle_command` dropped every command while this device wasn't the active one, a volume
among them. A volume chosen in the app while the music was elsewhere or nowhere changed what
was heard at once (the app sets its output itself) but never reached the player, which gave
its old volume back when the music came. `SpircCommand::SetVolume` is now taken either way,
as a volume Spotify passes on always was.

`set_volume` says the new volume (`VolumeChanged`) whether or not the music is here. Before,
one set while the music was elsewhere was only said once the music came. An app that keeps
the volume for the player's next start missed it when the player started again first.
The volume the player starts with is still not said (`take_volume`): the app gave it, and
hearing it back late would undo a volume chosen while the player was still connecting.

A volume Spotify passes on (`hm://connect-state/v1/connect/volume`) comes from another device
or app. `volume_asked_through_spotify` in `src/spirc.rs` notes that, and once the volume has
settled the app's log says "Another device or app set this player's volume to N%", unless
one was chosen here in between. Spotify doesn't say which device asked, and a volume this
app sends to its own player through Spotify's API counts as from elsewhere too.

The line goes on with what there is to tell one asker from another by (`volume_command`):
how many commands came before the volume settled (one is a jump, many are steps, as volume
buttons make), and what came with the last: its connection type, interaction and page ids,
message id and the message's headers. A header that holds a connection id or a token is
written as "(hidden)".

A player made with `ConnectConfig::disable_volume` tells Spotify its volume isn't for others
to set. librespot only said so and still took any volume that came. It now refuses one and
says so in the app's log ("Refused a volume of N% another device or app asked for"), at most
every five seconds, with how many were refused since; the ones after that line are said once
no more have come for five seconds. Nothing is said back to Spotify, so an
asker that sends again whenever the volume isn't the one it wants gets no reason to. The
app's own volume (`Spirc::set_volume`) works as before. Spotlight sets this from its "Let other devices change the volume" setting.
