# Spotlight

A Spotify player for Windows that feels instant. It's native Rust and Slint with no browser
inside, it animates at your monitor's full refresh rate, and when you leave it alone it all but
disappears: no frames drawn, next to no CPU, and its idle memory handed back to Windows.

## Install

Download **`Spotlight-1.2.0-Setup.exe`** from the
[latest release](https://github.com/Beroris/spotlight/releases/latest) and run it. It installs for
your account, adds Spotlight to the Start menu and can start it in the tray when you sign in.
Uninstall it from Settings → Apps.

Rather not install anything? Take **`Spotlight-1.2.0-Portable.exe`** instead. It runs from
anywhere and keeps its settings in a "Spotlight settings" folder beside itself.

You need Windows 10 (version 1809) or later, 64-bit, and Spotify Premium to play music on this PC.

## Updating

Spotlight looks for a new version when it opens and once a day. When one is out, a dot appears on
the Settings button, and the update waits at the top of Settings. Press **Update**: it downloads
the new version, checks it against the release, installs it and opens Spotlight again, with your
settings and sign-in as they were. The portable copy replaces itself and keeps its name, so
shortcuts to it keep working.

You can also check by hand in Settings → General, or turn the daily check off there. Running a
newer Setup over an older Spotlight works too.

## Connect Spotify

Spotlight signs in through a Spotify app of your own, which takes a minute to make. The setup
guide walks you through it the first time Spotlight opens, and opens again whenever you're
signed out. By hand:

1. Open the [Spotify developer dashboard](https://developer.spotify.com/dashboard) and create an app.
   If you already have one, you can use that.
2. Add this redirect URI to it, exactly:

   ```text
   http://127.0.0.1:5543/callback/
   ```

3. Tick Web API, save, and paste the app's Client ID into Spotlight.

Your browser then opens twice: once to let Spotlight see your music, and once more, through
librespot's own sign-in on `http://127.0.0.1:5544/login`, to play it on this PC.

## What's in it

- **Home** with your mixes and favorite artists first, and **Recently played** in the sidebar,
  with the songs played here and on your other devices.
- **Your library**: playlists and Liked Songs, a filter for any list (Ctrl+F), and one box to
  search songs or jump to a playlist (Ctrl+K).
- **Make playlists** and add songs to them, and save songs to Liked Songs.
- **Right-click a song** to play it next, queue it, go to its artist or album, start its radio or
  copy its link.
- **Synced lyrics**, big in the middle of the window, that follow the song. Click a line to
  jump to it.
- **Picks up where you left off**: open Spotlight and the last song is there, paused at the same
  spot, with the same songs coming up, shuffle and repeat as you left them. Press play to carry on.
- **A mini player** that stays on top while you work. Drag it anywhere, double-click it to come back.
- **Both sidebars resize**: drag their edges, double-click to reset.
- **Plays here or anywhere**: Spotlight is a Spotify Connect device called "Spotlight PC", and it
  can control your other devices too. It never takes over playback on its own.
- **Fits into Windows**: the media controls by the volume and on the lock screen, play and skip
  buttons on the taskbar preview, media keys and your own global shortcuts.

## Build from source

You need a Rust toolchain on Windows.

```powershell
cargo build --release
```

To build both release files into `dist/`, install [Inno Setup 6](https://jrsoftware.org/isinfo.php)
(`winget install JRSoftware.InnoSetup`) and run:

```powershell
pwsh scripts/package-windows.ps1
```

## Notes

- Settings, the sign-in, saved covers, where the music was left and the last 50 songs played here live in
  `%LOCALAPPDATA%\Spotlight`, or in "Spotlight settings" beside the portable copy. The sign-in
  tokens are stored unencrypted.
- Spotify doesn't count songs that Spotlight plays: they don't reach your Spotify listening
  history, Wrapped or Last.fm. Spotlight keeps its own list for Recently played.
- If Spotlight ever closes on an error, what went wrong is in `crash.log` in that same folder.
- The update check asks GitHub's public API for the latest release. It sends nothing about you.
- Sound goes through librespot. The Web API login asks Spotify for these permissions:
  `app-remote-control`, `streaming`, `user-read-email`, `user-read-private`,
  `user-read-playback-state`, `user-modify-playback-state`, `user-read-currently-playing`,
  `playlist-read-private`, `playlist-read-collaborative`, `playlist-modify-public`,
  `playlist-modify-private`, `user-library-read`, `user-library-modify` and
  `user-read-recently-played`.

## Support

Spotlight is free. If you enjoy it, you can [support me on Ko-fi](https://ko-fi.com/beroris).

## License

GPL-3.0. See [LICENSE](LICENSE). The Geist font in `assets/fonts` keeps its own SIL Open Font
License. The changed copies of librespot (MIT) and Slint's winit backend in `vendor` keep their own
licenses, and each folder's `SPOTLIGHT-CHANGES.md` says what was changed.
