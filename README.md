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
- **Windows server** (`streamy-server-windows.zip`): double-click
  `streamy-server.exe` and answer **Y**. After a UAC prompt it installs itself to
  `Program Files\Streamy` as a service (starts with Windows, streams UAC prompts and
  the login screen), opens the firewall for it, adds an entry to *Installed apps*
  and prints the pairing key to enter in the Mac client. A tray icon shows who is
  connected; its menu opens the received-files folder and shows the pairing key
  (after a permission prompt). Uninstall from *Installed apps*, or
  `streamy-server.exe --uninstall` (`--purge` also deletes the key and logs).

To publish a release, push a tag: `git tag v0.3.0 && git push origin v0.3.0`.

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
releases every key and button.
**⌃⌥S** shows or hides a graph of frame rate, bitrate, latency, RTT and
lost frames over the last 20 seconds (remembered between launches). Run the server as administrator to control
elevated windows.

## Pairing

The server has a pairing key and encrypts everything with it; a client without
the key can't see or do anything, and the server doesn't answer it at all
(details in `docs/AUTH.md`).

- `streamy-server.exe --install-service` creates the key (in
  `C:\ProgramData\Streamy\key`, readable only by administrators) and prints it.
  `--show-key` prints it again, `--new-key` replaces it (every client then needs
  the new one).
- On the Mac, once per server: `streamy-client --set-key <server> <key>`, or
  `streamy-client <server> --key <key>`. It is kept in
  `~/Library/Application Support/Streamy/keys`.
- You can fetch the key from the Mac over SSH, without touching the PC:
  `ssh gaming-pc 'C:\ProgramData\Streamy\streamy-server.exe --show-key'`.

## Clipboard and files

- **Clipboard text** is shared both ways while connected (up to 1 MB; items a
  password manager marks as concealed are never sent). Turn it off in
  Settings (⌘,) on the Mac, or with `--no-clipboard` on the server.
- **Files**: drop them on the stream window, or Connection ▸ Send File to PC…, or
  `streamy-client <server> --send <file>`. They land in the PC user's
  `Downloads\Streamy` folder, owned by that user, at up to ~8 MB/s so they can't
  starve the video. `--no-files` turns uploads off on the server and
  `--max-upload-mb` (default 4096) limits their size. Transfers ride the same
  encrypted session, with their own retransmission (`crates/xfer`).

## Settings

Settings (⌘,) on the Mac: video bitrate, a frame-rate cap, loss protection (FEC),
volume and clipboard sharing. Bitrate, cap and FEC are applied live on the server;
`View ▸ Mute Audio`, `⌃⌥S` (stats) and `⌃⌘F` (full screen) are in the menus too.
On a LAN the connect dialog lists servers that announce themselves (turn that off with
`--no-discovery`), and remembers the ones you used.

## Packet loss

Every video frame is sent with 10% repair packets (Reed-Solomon, `--fec <percent>`
on the server, 0 turns it off): a frame that loses a few packets is rebuilt on
the client with no retransmission and no extra latency. The client's stats line
shows `fec-repaired N` for frames saved that way.

## Debugging

`STREAMY_SIM_LOSS=<percent> streamy-client <server>` drops that share of incoming
video packets, to test loss recovery.

`STREAMY_TRACE=trace.csv streamy-client <server>` writes one line per frame
(server capture time, decoded time, size, keyframe) for timing analysis.

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
