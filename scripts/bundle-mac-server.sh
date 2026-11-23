#!/bin/sh
# Wraps a built server binary as "DisplayWire Server.app": a menu-bar app, so macOS lists it by
# name when it asks for Screen Recording and Accessibility.
#   scripts/bundle-mac-server.sh <displaywire-server binary> <output dir>
# Ad-hoc signed unless SIGN_IDENTITY names a certificate. An ad-hoc signature changes with every
# build, and macOS then forgets the permissions; a certificate (even a self-signed one in the
# login keychain) keeps them across rebuilds.
set -eu
bin=$1
out=$2
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app="$out/DisplayWire Server.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/displaywire-server"
cp client/assets/DisplayWire.icns "$app/Contents/Resources/"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DisplayWire Server</string>
  <key>CFBundleDisplayName</key><string>DisplayWire Server</string>
  <key>CFBundleIdentifier</key><string>app.displaywire.server</string>
  <key>CFBundleExecutable</key><string>displaywire-server</string>
  <key>CFBundleIconFile</key><string>DisplayWire</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppleEventsUsageDescription</key><string>DisplayWire Server asks Finder which folder a file dropped from the other computer goes into.</string>
  <key>NSLocalNetworkUsageDescription</key><string>DisplayWire Server answers the computers you stream this Mac's screen to on your local network.</string>
</dict>
</plist>
PLIST
case "${SIGN_IDENTITY:--}" in
  "Developer ID"*)
    # For notarizing: the hardened runtime, with what the app needs from it.
    codesign --force --deep --options runtime --timestamp --entitlements scripts/server.entitlements --sign "$SIGN_IDENTITY" "$app" ;;
  *)
    codesign --force --deep --sign "${SIGN_IDENTITY:--}" "$app" ;;
esac
echo "$app"
