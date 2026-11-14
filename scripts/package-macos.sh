#!/bin/sh
# Builds Spotlight for the Mac and puts Spotlight.app in dist/. Run it on a Mac.
#
# The app is signed only for the Mac that builds it, which is enough to run it there. Handing
# it to other people needs an Apple developer signature and notarization, which this doesn't do.
set -eu

cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
app="dist/Spotlight.app"

cargo build --release

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/Spotlight "$app/Contents/MacOS/Spotlight"

# The Dock's icon, from the logo, at every size the Mac asks for.
iconset="$(mktemp -d)/Spotlight.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    double=$((size * 2))
    sips -z "$size" "$size" assets/spotlight-logo.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    sips -z "$double" "$double" assets/spotlight-logo.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Spotlight.icns"
rm -rf "$(dirname "$iconset")"

# CFBundleIdentifier is also the name of the settings folder, in src/config.rs.
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Spotlight</string>
    <key>CFBundleDisplayName</key>
    <string>Spotlight</string>
    <key>CFBundleIdentifier</key>
    <string>com.beroris.spotlight</string>
    <key>CFBundleExecutable</key>
    <string>Spotlight</string>
    <key>CFBundleIconFile</key>
    <string>Spotlight</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.music</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>NSLocalNetworkUsageDescription</key>
    <string>Spotlight shows up as a device in Spotify on your phone and other computers.</string>
    <key>NSBonjourServices</key>
    <array>
        <string>_spotify-connect._tcp</string>
    </array>
</dict>
</plist>
EOF

codesign --force --sign - "$app"

echo "Built $app ($version)"
