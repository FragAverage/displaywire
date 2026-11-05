#!/bin/sh
# Packages the webOS client as an IPK (run scripts/build-webos.sh first). Extra files to bundle
# (a test stream, an `args` file) can be given as arguments. Output: target/webos/*.ipk
set -eu
cd "$(dirname "$0")/.."
out=target/webos
rm -rf "$out/app" "$out"/*.ipk && mkdir -p "$out/app"
cp webos/app/* "$out/app/"
docker run --rm -v "$PWD":/src displaywire-webos arm-webos-linux-gnueabi-strip \
    -o "/src/$out/app/displaywire-tv" /src/target/armv7-unknown-linux-gnueabi/release/displaywire-tv
for f in "$@"; do cp "$f" "$out/app/"; done
ares-package -o "$out" "$out/app" >/dev/null
ls "$out"/*.ipk
