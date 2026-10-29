# Streamy

Low-latency desktop streaming: Windows server (NVIDIA) → macOS client. See `PLAN.md`.

## Install from a release

Download from the repo's Releases page:

- **Mac client** (`Streamy-macos.zip`, Apple Silicon and Intel): unzip and
  drag `Streamy.app` to Applications. It isn't notarized, so clear the
  download flag once before opening it:
  ```
  xattr -dr com.apple.quarantine /Applications/Streamy.app
  ```
  It asks for the server's address on launch and remembers it. From a
  terminal, `/Applications/Streamy.app/Contents/MacOS/streamy-client <server-ip>`
  connects directly and prints live stats.
- **Windows server** (`streamy-server-windows.zip`): run `streamy-server.exe`
  and allow it through the firewall (udp/9000).

To publish a release, push a tag: `git tag v0.2.0 && git push origin v0.2.0`.

## Build (on the Mac)
```
cargo build --release                                                          # Mac client + tools
cargo xwin build --release -p streamy-server --target x86_64-pc-windows-msvc   # Windows server
```
Server binary: `target/x86_64-pc-windows-msvc/release/streamy-server.exe`

## Run
Windows (allow udp/9000 through the firewall on first run):
```
streamy-server.exe [--mbps 50] [--fps 120]
```
Mac:
```
target/release/streamy-client <server-ip>
```
For lowest Wi-Fi latency on the Mac, disable AWDL while streaming: `sudo ifconfig awdl0 down`
(restore with `up`).

## Run as a service (UAC prompts and the lock screen)

A normal `streamy-server.exe` can't see UAC prompts or the login/lock screen:
Windows shows them on a separate desktop that only SYSTEM can capture. From an
administrator terminal:
```
streamy-server.exe --install-service [--mbps 80 --fps 120 ...]
streamy-server.exe --uninstall-service
```
The service starts at boot, runs the server as SYSTEM in the active session
(the console if someone is there, else e.g. a remote desktop session), and
restarts it when the session changes (logon, logoff, lock, switch user). It
adds a firewall rule for the exe and logs to `C:\ProgramData\Streamy\server.log`.
Install from a stable path, since the service points at the exe you ran.

## Controls

Keyboard and mouse go to the PC while the client window has focus (injected
with `SendInput`, like Sunshine and Parsec). The pointer follows the Mac
cursor by default; **⌃⌥M** captures it for games (relative motion, Mac
cursor hidden) and releases it again. Switching away from the window
releases every key and button. Run the server as administrator to control
elevated windows.

## Test without Windows
```
scripts/make-clips.sh
target/release/streamy-server --file bench/clips/hevc-2560x1440.h265 &
target/release/streamy-client 127.0.0.1
```

## Layout
| Path | What |
|---|---|
| `server/` | Capture (DXGI Desktop Duplication) → NV12 (D3D11 video processor) → HEVC (Media Foundation hardware MFT) → UDP |
| `client/` | UDP → reassembly → VideoToolbox → IOSurface on a CoreAnimation layer |
| `crates/proto` | Wire format |
| `crates/transport` | Sockets, frame packetizing/reassembly, Windows Wi-Fi tuning |
| `crates/hevc` | Annex B parsing |
| `crates/videotoolbox` | VideoToolbox FFI |
| `crates/timing` | Clock, precise sleeps, percentiles |
| `tools/netprobe` | Network jitter/loss measurement |
| `tools/decodebench` | VideoToolbox decode latency benchmark |
