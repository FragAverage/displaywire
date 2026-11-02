# Contributing to DisplayWire

Thanks for helping. This is the developer side of the project; how to *use* DisplayWire is in the [README](README.md).

Contributions are accepted under the project's licence (MIT or Apache-2.0, at your option), with no extra terms.

## How the project fits together

A Windows **server** captures the screen, encodes it and sends it over UDP; a macOS **client** decodes and shows it,
and sends keyboard, mouse, clipboard, files and microphone back.

| Path | What |
|---|---|
| `server/` | Windows service and tray: capture (DXGI Desktop Duplication), NV12 conversion, NVENC / Media Foundation HEVC, WASAPI audio, input injection, pairing, the device window (egui) |
| `client/` | macOS app (winit + AppKit via objc2): receive, reassemble, VideoToolbox decode, Metal/CoreAnimation present, the PC launcher and pairing window |
| `crates/proto` | Wire format and discovery messages |
| `crates/transport` | Sockets, frame packetizing and reassembly, FEC, Windows Wi-Fi tuning |
| `crates/secure` | Noise handshake, the encrypted transport, and the PIN pairing crypto (SPAKE2) |
| `crates/pairing` | The client side of pairing, as a small state machine |
| `crates/update` | Signed-update checks and downloads (the apps' side) |
| `crates/xfer` | Reliable file transfer over the session |
| `crates/hevc`, `crates/videotoolbox`, `crates/timing` | Annex B parsing, VideoToolbox FFI, clocks and percentiles |
| `tools/netprobe`, `tools/decodebench`, `tools/pairprobe` | Network jitter probe, decode benchmark, a CLI that pairs against a server |
| `tools/dwsign` | Release signing, run by CI |

Design notes: [`PLAN.md`](PLAN.md) (goals and decisions), [`docs/AUTH.md`](docs/AUTH.md) (handshake and encryption),
[`docs/PAIRING.md`](docs/PAIRING.md) (PIN pairing and per-device keys), [`docs/UPDATES.md`](docs/UPDATES.md)
(signed updates). Read the one for the area you are changing: the protocols are specified there, and the code
follows them.

## Building

You need Rust 1.88 or newer.

On a Mac, the client and the shared crates build natively:

```
cargo build --release          # the Mac client and tools
cargo test --workspace
```

The server only builds for Windows. Either build it on a Windows machine with the same cargo commands
(`cargo build --release -p displaywire-server`), or cross-compile from the Mac:

```
cargo install cargo-xwin       # plus LLVM: brew install llvm (for llvm-lib)
cargo xwin build --release -p displaywire-server --target x86_64-pc-windows-msvc
```

Windows-only code isn't compiled on the Mac, so **CI is the real check for it** (see below). If you change anything
under `server/src/windows/`, build and run clippy on Windows before opening a pull request.

### Working on the PC device window from a Mac

The egui window is built on Windows only, but the `ui-dev` feature builds it on any OS, and `--control-only` runs
just the pairing and control side (no capture or encoding):

```
cargo run -p displaywire-server --features ui-dev -- --control-only --data-dir /tmp/dw-data
# prints:  window is  displaywire-server --ui <port> <token>
cargo run -p displaywire-server --features ui-dev -- --ui <port> <token>
```

Then pair a client against it (`displaywire-client --pair 127.0.0.1`) to see the whole flow without a PC.

### Trying it without a Windows PC

```
scripts/make-clips.sh
target/release/displaywire-server --file bench/clips/hevc-2560x1440.h265 &
target/release/displaywire-client 127.0.0.1
```

## Checks your change must pass

CI (`.github/workflows/ci.yml`) runs these on macOS and Windows for every push and pull request:

```
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # Windows: the portable crates only
```

Run both before you push. Warnings are errors.

## Conventions

- Match the surrounding code: naming, comment density, idiom. Comments say *why*, not what.
- Keep the hot path (capture to encode to send, receive to decode to present) free of allocation, locks and async
  runtimes: dedicated threads and ring buffers. See `PLAN.md`.
- Anything that parses network input must treat it as hostile: no panics on short or malformed packets, and add a
  test for the malformed case. Everything on the wire is authenticated and encrypted after the handshake; keep it so.
- Anything security-sensitive (keys, pairing, updates, file permissions) should come with a test for the failure
  case, not only the working one.
- UI code that can't be driven by a test has debug-only hooks (`DISPLAYWIRE_TEST_SCRIPT`, `DISPLAYWIRE_TEST_PIN_FILE`,
  `DISPLAYWIRE_FAKE_VERSION`, `DISPLAYWIRE_UPDATE_BASE`); they are compiled out of release builds.
- Commit messages: a short summary line in the imperative, then a paragraph on *why* if it isn't obvious.
- Keep pull requests focused: one change, with tests, and docs updated if behaviour changed.

## Debugging

- `DISPLAYWIRE_SIM_LOSS=<percent>` on the client drops that share of incoming video packets, to test loss recovery.
- `DISPLAYWIRE_TRACE=trace.csv` on the client writes one line per frame for timing analysis.
- `netprobe` measures raw network jitter and loss between the two machines, which is the first thing to check for
  "choppy" reports: most stutter turned out to be Wi-Fi.
- The server logs to `C:\ProgramData\DisplayWire\server.log`; the device window logs failures to
  `%TEMP%\displaywire-ui.log`.
- The stats graph (⌃⌥S in the client) shows frame rate, bitrate, latency, RTT, lost frames and audio delay.

## Testing updates

Updating needs a real signed release, but you can test the install step locally. Debug builds only:

```
# serve a downloaded release (latest.txt, latest.txt.sig, DisplayWire-macos.zip) from a folder
python3 -m http.server 8765
DISPLAYWIRE_UPDATE_BASE=http://127.0.0.1:8765 DISPLAYWIRE_FAKE_VERSION=0.0.1 DISPLAYWIRE_TEST_AUTOINSTALL=1 \
  DisplayWire.app/Contents/MacOS/displaywire-client
```

`cargo run -p update --example check -- <current version>` prints what the apps would find.

## Releasing (maintainers)

1. Bump `version` in the root `Cargo.toml` (and `Cargo.lock` via `cargo update --workspace`) and commit.
2. Wait for CI on that commit to pass.
3. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.

The release workflow builds the universal Mac app and the Windows server, signs the update manifest with the key in
the `release` environment (only `v*` tags can use it; the job refuses a tag that doesn't match `Cargo.toml`),
and publishes the release. The Homebrew cask in [FragAverage/homebrew-displaywire](https://github.com/FragAverage/homebrew-displaywire)
follows new releases by itself (a workflow there runs every six hours; run it by hand from the Actions tab to hurry it).
Details, and what to do if the signing key is lost, are in
[`docs/UPDATES.md`](docs/UPDATES.md).

## Reporting security problems

Please don't open a public issue for a vulnerability. Use GitHub's private vulnerability reporting on this
repository (Security tab > Report a vulnerability).
