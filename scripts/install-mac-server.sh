#!/bin/sh
# Installs (or updates) DisplayWire Server on this Mac, in one line:
#   curl -fsSL https://raw.githubusercontent.com/denvergj/displaywire/main/scripts/install-mac-server.sh | sh
# It downloads the latest release, puts DisplayWire Server.app in Applications and opens it.
# The app then starts at every login, sits in the menu bar, and shows a window listing the
# permissions to allow. (Downloaded with curl, the app isn't quarantined, so macOS doesn't
# refuse to open a build that isn't notarized.)
set -eu
repo=${DISPLAYWIRE_REPO:-denvergj/displaywire}
url="https://github.com/$repo/releases/latest/download/DisplayWire-Server-macos.zip"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading DisplayWire Server…"
curl -fL --progress-bar "$url" -o "$tmp/server.zip"
ditto -x -k "$tmp/server.zip" "$tmp"

dest=/Applications
[ -w "$dest" ] || dest="$HOME/Applications"
mkdir -p "$dest"
app="$dest/DisplayWire Server.app"

# Stop a running copy before replacing it.
launchctl bootout "gui/$(id -u)/app.displaywire.server" 2>/dev/null || true
pkill -x displaywire-server 2>/dev/null || true
rm -rf "$app"
ditto "$tmp/DisplayWire Server.app" "$app"
xattr -dr com.apple.quarantine "$app" 2>/dev/null || true

open "$app"
echo "Installed in $dest. DisplayWire Server is in the menu bar; allow the permissions in the window that opens."
