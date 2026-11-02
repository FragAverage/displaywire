# DisplayWire

Low-latency desktop streaming: Windows server (NVIDIA) → macOS client. See `PLAN.md`.

## Install from a release

Download from the repo's Releases page:

- **Mac client** (`DisplayWire-macos.zip`, Apple Silicon and Intel): unzip and
  drag `DisplayWire.app` to Applications. It isn't notarized, so clear the
  download flag once before opening it:
  ```
  xattr -dr com.apple.quarantine /Applications/DisplayWire.app
  ```
  It shows your PCs; click one to connect. From a
  terminal, `/Applications/DisplayWire.app/Contents/MacOS/displaywire-client <server-ip>`
  connects directly and prints live stats.
- **Windows server** (`displaywire-server-windows.zip`): double-click
  `displaywire-server.exe` and answer **Y**. After a UAC prompt it installs itself to
  `Program Files\DisplayWire` as a service (starts with Windows, streams UAC prompts and
  the login screen), opens the firewall for it, adds an entry to *Installed apps*
  and tells you to pair your first device from the tray icon. A tray icon shows who is
  connected; its menu pairs new devices, manages paired ones and opens the received-files
  folder. Uninstall from *Installed apps*, or
  `displaywire-server.exe --uninstall` (`--purge` also deletes the paired devices and logs).

To publish a release, push a tag: `git tag v0.6.1 && git push origin v0.6.1`.

## Build (on the Mac)
Needs Rust 1.88+, and for the server `cargo install cargo-xwin` plus LLVM (`brew install llvm`, for `llvm-lib`).
```
cargo build --release                                                          # Mac client + tools
cargo xwin build --release -p displaywire-server --target x86_64-pc-windows-msvc   # Windows server
```
Server binary: `target/x86_64-pc-windows-msvc/release/displaywire-server.exe`

## Run
Windows (allow udp/9000 through the firewall on first run):
```
displaywire-server.exe [--mbps 50] [--fps 120]
```
Mac:
```
target/release/displaywire-client <server-ip>
```
For lowest Wi-Fi latency on the Mac, disable AWDL while streaming: `sudo ifconfig awdl0 down`
(restore with `up`).

## Run as a service (UAC prompts and the lock screen)

A normal `displaywire-server.exe` can't see UAC prompts or the login/lock screen:
Windows shows them on a separate desktop that only SYSTEM can capture. From an
administrator terminal:
```
displaywire-server.exe --install-service [--mbps 80 --fps 120 ...]
displaywire-server.exe --uninstall-service
```
The service starts at boot, runs the server as SYSTEM in the active session
(the console if someone is there, else e.g. a remote desktop session), and
restarts it when the session changes (logon, logoff, lock, switch user). It
adds a firewall rule for the exe and logs to `C:\ProgramData\DisplayWire\server.log`.
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

Every device has a key of its own, and everything is encrypted with it; a client that isn't paired can't see or do
anything, and the server doesn't answer it at all (details in `docs/AUTH.md` and `docs/PAIRING.md`).

To pair a Mac:

1. On the PC, click the DisplayWire tray icon → **Pair new device…**
2. On the Mac, open DisplayWire → **Add PC…**, enter the PC's address (or click it if it was found on the network).
3. On the PC, **Allow** the request. A 6-digit PIN appears; type it on the Mac.

That's all: no key to copy. The PC's **Manage devices…** window lists paired devices and removes them (which destroys
that device's key and disconnects it at once). Over SSH, `displaywire-server --pair` does the PC side from a console,
and `--list-clients` / `--remove-client <id or name>` manage the list.

The Mac keeps a list of PCs; click one to connect. From a terminal: `displaywire-client --pair <server>` pairs,
then `displaywire-client <server>` connects.

## Updates

Both apps look for a newer release on GitHub and offer to install it (Mac: **DisplayWire > Check for Updates…**; PC:
the tray menu, or `displaywire-server --update`). Releases are signed, and the apps only accept an update whose
signature matches the key built into them. See `docs/UPDATES.md`.

## Clipboard and files

- **Clipboard text** is shared both ways while connected (up to 1 MB; items a
  password manager marks as concealed are never sent). Turn it off in
  Settings (⌘,) on the Mac, or with `--no-clipboard` on the server.
- **Files**: drop them on the stream window, or Connection ▸ Send File to PC…, or
  `displaywire-client <server> --send <file>`. They land in the PC user's
  `Downloads\DisplayWire` folder, owned by that user, at up to ~8 MB/s so they can't
  starve the video. `--no-files` turns uploads off on the server and
  `--max-upload-mb` (default 4096) limits their size. Transfers ride the same
  encrypted session, with their own retransmission (`crates/xfer`).

## Audio and microphone

- **Desktop audio** is captured from every app on the PC, whichever output device it
  uses (Windows 10 2004 or later; older Windows falls back to the default output).
  DisplayWire's own audio is left out. The Mac plays it with a small adaptive cushion, and
  the stats graph (`⌃⌥S`) has an audio-delay row.
- **Microphone** (Mac to PC): `⌃⌥V` or View ▸ Microphone On/Off. It is off at every
  connection, and macOS shows its recording indicator while it is on. The server plays
  it into a virtual audio cable, so install one on the PC, such as the free
  [VB-Cable](https://vb-audio.com/Cable/) (the server finds "CABLE Input" by itself, or
  use `--mic-device <part of an output's name>`), then pick **CABLE Output** as the
  microphone in the PC app that should hear you. `--no-mic` turns it off on the server.
  A driver of its own is not part of DisplayWire yet: Windows only lists devices that a
  signed driver provides.

## Settings

Settings (⌘,) on the Mac: video bitrate, a frame-rate cap, loss protection (FEC),
volume and clipboard sharing. Bitrate, cap and FEC are applied live on the server;
`View ▸ Mute Audio`, `⌃⌥S` (stats) and `⌃⌘F` (full screen) are in the menus too.
On a LAN the connect dialog lists servers that announce themselves (turn that off with
`--no-discovery`), and remembers the ones you used.

## Wi-Fi tuning (macOS)

On Wi-Fi, macOS periodically hops channels for AirDrop and AirPlay (AWDL), which shows up
as bursts of added delay on the stream. **Connection ▸ Set Up Wi-Fi Tuning…** adds a
sudoers rule, after one password prompt, that lets DisplayWire run exactly
`ifconfig awdl0 down` and `ifconfig awdl0 up`. While connected it holds AWDL down and
brings it back when you disconnect or quit. The Settings sheet has a switch for it, and
`sudo rm /etc/sudoers.d/displaywire-awdl` removes the rule.

## Packet loss

Every video frame is sent with 10% repair packets (Reed-Solomon, `--fec <percent>`
on the server, 0 turns it off): a frame that loses a few packets is rebuilt on
the client with no retransmission and no extra latency. The client's stats line
shows `fec-repaired N` for frames saved that way.

## Debugging

`DISPLAYWIRE_SIM_LOSS=<percent> displaywire-client <server>` drops that share of incoming
video packets, to test loss recovery.

`DISPLAYWIRE_TRACE=trace.csv displaywire-client <server>` writes one line per frame
(server capture time, decoded time, size, keyframe) for timing analysis.

## Test without Windows
```
scripts/make-clips.sh
target/release/displaywire-server --file bench/clips/hevc-2560x1440.h265 &
target/release/displaywire-client 127.0.0.1
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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise,
any contribution you submit for inclusion in this project, as defined in the
Apache-2.0 license, is dual licensed as above, without any additional terms or
conditions.
