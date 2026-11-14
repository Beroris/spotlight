#!/bin/sh
# Builds Spotlight for Linux and puts it in dist/linux/. Run it on Linux.
#
#   scripts/package-linux.sh              build only
#   scripts/package-linux.sh --install    build, then install for this user, without root
#   scripts/package-linux.sh --uninstall  take that install away again (settings stay)
#
# Installing puts the program in ~/.local/bin, its launcher entry in
# ~/.local/share/applications and its icon in ~/.local/share/icons. The entry's file name is
# the app id the window gives the desktop, in src/window_chrome_linux.rs, which is how the
# desktop matches the window to its launcher and icon.
#
# Building needs a C compiler and the development files for ALSA, OpenSSL, fontconfig,
# Wayland and xkbcommon. On Debian or Ubuntu:
#   sudo apt install build-essential libasound2-dev libssl-dev libfontconfig-dev libwayland-dev libxkbcommon-dev
set -eu

cd "$(dirname "$0")/.."

id="com.beroris.spotlight"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"
program="$bin/spotlight"
entry="$data/applications/$id.desktop"
sizes="48 64 128 256 512"

refresh() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$data/applications" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    [ -x "$program" ] && "$program" --quit || true
    rm -f "$program" "$entry" "$data/icons/$id.png" "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/$id.desktop"
    for size in $sizes; do
        rm -f "$data/icons/hicolor/${size}x${size}/apps/$id.png"
    done
    refresh
    echo "Spotlight is uninstalled. Its settings are still in $data/spotlight."
    exit 0
fi

cargo build --release

stage="dist/linux"
rm -rf "$stage"
mkdir -p "$stage/icons"
cp target/release/Spotlight "$stage/spotlight"

# The launcher's icon, from the logo, at the sizes desktops ask for. Whichever of these tools
# is there does the resizing. With none, the logo is installed whole, which works as well.
resize() {
    if command -v magick >/dev/null 2>&1; then
        magick assets/spotlight-logo.png -resize "$1x$1" "$2"
    elif command -v convert >/dev/null 2>&1; then
        convert assets/spotlight-logo.png -resize "$1x$1" "$2"
    else
        python3 -c 'import sys; from PIL import Image; Image.open("assets/spotlight-logo.png").resize((int(sys.argv[1]),) * 2, Image.LANCZOS).save(sys.argv[2])' "$1" "$2" 2>/dev/null
    fi
}
for size in $sizes; do
    resize "$size" "$stage/icons/$size.png" || { rm -f "$stage"/icons/*.png; break; }
done
cp assets/spotlight-logo.png "$stage/icons/whole.png"

# The launcher entry. $1 is what it runs.
entry_for() {
    cat <<ENTRY
[Desktop Entry]
Type=Application
Name=Spotlight
GenericName=Music Player
Comment=Play your Spotify music
Exec=$1
Icon=$id
Terminal=false
Categories=AudioVideo;Audio;Player;
StartupWMClass=$id
StartupNotify=true
ENTRY
}
entry_for spotlight > "$stage/$id.desktop"

echo "Built $stage/spotlight"
[ "${1:-}" = "--install" ] || exit 0

# A copy that's running quits first, so the file can be replaced.
[ -x "$program" ] && "$program" --quit || true

mkdir -p "$bin" "$data/applications" "$data/icons"
install -m 755 "$stage/spotlight" "$program"
if [ -f "$stage/icons/512.png" ]; then
    for size in $sizes; do
        mkdir -p "$data/icons/hicolor/${size}x${size}/apps"
        install -m 644 "$stage/icons/$size.png" "$data/icons/hicolor/${size}x${size}/apps/$id.png"
    done
    rm -f "$data/icons/$id.png"
else
    install -m 644 "$stage/icons/whole.png" "$data/icons/$id.png"
fi
# The full path, as ~/.local/bin isn't on every PATH. Quoted, as a path may have spaces in
# it, with two backslashes before what the quotes don't cover.
quoted=$(printf '%s' "$program" | sed 's/\\/\\\\\\\\/g; s/["$`]/\\\\&/g')
entry_for "\"$quoted\"" > "$entry"
refresh

echo "Installed Spotlight for $(id -un): $program"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "$bin isn't on your PATH, so start Spotlight from the launcher, or by its full path." ;;
esac
