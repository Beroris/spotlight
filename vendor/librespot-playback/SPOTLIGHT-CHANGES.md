# Why this crate is here

This is librespot-playback 0.8.0 from crates.io with two additions. The main one is crossfade, a
song fading into the next as it ends. librespot plays one song at a time, and only its player
knows where a song ends and has the next one ready, so it has to happen here. The other is a
counter that lets the sink see a seek or a skip as soon as it happens (see below).

## How it works

- `set_crossfade(duration)` sets the length. Zero, the default, turns it off.
- Many songs end in seconds of silence, and a fade over that is only a gap with the next song
  fading in after it. So from the crossfade plus 10 s before the end, the song is read ahead
  (a few milliseconds of decoding at a time) to find where its sound stops: the last packet
  louder than about -36 dB. What is read plays in turn, so nothing is decoded twice, and it is
  dropped on a seek, a stop or a load. Reloading the same song puts its decoder back where
  playing had got to.
- A song that is close enough to where its sound stops, with the next song preloaded, ends
  early. The player sends `EndOfTrack` as at any end, so Spirc moves on and loads the next song
  as usual. If the song couldn't be read to its end in time, or runs past its listed length,
  its listed end is used instead.
- The ended song keeps its decoder and plays on by itself until the next song starts. Then its
  remaining audio is mixed under the new song's first packets, with equal-power gains, until the
  fade is over. Each song is normalised as it would be alone before they are mixed.
- The fade's length is fixed when the song ends early: the setting, or what was left of the
  song's sound if that was less. Changing the setting during a fade doesn't change it.
- Two loud songs at once can add up past full scale. In the middle of a fade, peaks bend smoothly
  towards full scale instead of clipping. At either end of the fade nothing bends.
- No fade when either song is shorter than two fades, or when the next song comes straight after
  this one on the same album, so albums played in order keep their gaps or lack of them. No fade
  with passthrough on either, as that audio is still encoded.
- Only the first song loaded after the early end fades in. Any other load, a seek or a stop drops
  the fading song at once, so skipping is as quick as ever. Pausing holds both.
- Stopped after the early end but before the next song is loaded, the fading song is the only one
  playing, so the player sends `Stopped` for it, as it would for any song.

## Knowing when a seek or a skip has happened

Spotlight silences what's buffered the moment a seek or skip is pressed, and drops whatever the
player hands over until the player has moved. It used to learn that from the `Seeked` or
`PlayRequestIdChanged` event, but the player keeps decoding while the event waits to be read,
so music from the new place could be dropped too. `POSITION_GENERATION` in `player.rs` goes up
just before either event is sent, from the player's own thread, so Spotlight's sink can see the
move with the very next packet.

## What to carry over to a newer librespot

- `src/crossfade.rs`: all of the above that doesn't touch the player's state, with its tests.
- `src/lib.rs`: `mod crossfade`.
- `src/player.rs`:
  - The `fading` and `tail` fields of `PlayerInternal`.
  - `start_crossfade_if_due`, `fading_plays_alone` and `play_fading_alone`, called from `poll`, which also stays awake while the old song plays alone.
  - In `poll`, packets come through the `tail` while there is one for the song playing.
  - `normalise`, moved out of `handle_packet` so both songs can be normalised, and the mixing in `handle_packet`.
  - `drop_tail`, and the `fading` and `tail` lines in `handle_player_stop`, `handle_play`, `handle_pause`, `handle_command_load` and `handle_command_seek`.
  - `POSITION_GENERATION`, and the two lines that raise it, before `PlayRequestIdChanged` in `handle_command_load` and before `Seeked` in `handle_command_seek`.
  - Everything is marked with a `Spotlight:` comment.

The dynamic normalisation method keeps a limiter's state from sample to sample. During a fade
it runs over both songs in turn, which is close enough. Spotlight uses the basic method, which has
no state.

Run its tests with `cargo test -p librespot-playback crossfade`.
