#!/bin/sh
# Wraps a built client binary as DisplayWire.app (ad-hoc signed).
#   scripts/bundle-mac.sh <displaywire-client binary> <output dir>
set -eu
bin=$1
out=$2
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app="$out/DisplayWire.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/displaywire-client"
cp client/assets/DisplayWire.icns "$app/Contents/Resources/"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DisplayWire</string>
  <key>CFBundleDisplayName</key><string>DisplayWire</string>
  <key>CFBundleIdentifier</key><string>app.displaywire.client</string>
  <key>CFBundleExecutable</key><string>displaywire-client</string>
  <key>CFBundleIconFile</key><string>DisplayWire</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.games</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>DisplayWire sends your microphone to the PC you stream from, when you turn it on (Control-Option-V).</string>
  <key>NSLocalNetworkUsageDescription</key><string>DisplayWire finds and connects to the PC you stream from on your local network.</string>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$app"
echo "$app"
