#!/bin/sh
# Builds displaywire-tv for LG webOS TVs (armv7) in Docker. Output:
# target/armv7-unknown-linux-gnueabi/release/displaywire-tv
set -eu
cd "$(dirname "$0")/.."
docker build -q -t displaywire-webos webos/docker >/dev/null
docker run --rm -v "$PWD":/src -v displaywire-webos-cargo:/root/.cargo/registry displaywire-webos \
    cargo build --release -p displaywire-tv --target armv7-unknown-linux-gnueabi "$@"
file target/armv7-unknown-linux-gnueabi/release/displaywire-tv
