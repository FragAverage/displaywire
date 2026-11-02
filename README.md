# DisplayWire

Stream your Windows PC's desktop to your Mac with very low latency. Play games, run apps and use the PC as if you
were sitting at it: the picture, sound, keyboard, mouse, clipboard, files and your microphone all go across.

- **Fast:** hardware video encoding on the PC (NVIDIA), hardware decoding on the Mac, a custom UDP protocol, and
  repair packets so a lost packet doesn't cost a frame. Typically 10 to 20 ms from the PC's screen to yours.
- **Private:** everything is encrypted, and each Mac has its own key. A device that isn't paired can't see or do
  anything, and the PC doesn't even answer it.
- **Simple:** pair with a 6-digit PIN, then click your PC to connect.

## What you need

- **PC:** Windows 10 or 11, with an NVIDIA graphics card for the best performance. (Sound from every app needs
  Windows 10 version 2004 or later; older versions send the default output only.)
- **Mac:** macOS 12 or later, Apple Silicon or Intel.
- Both on the same network, or on the same [Tailscale](https://tailscale.com) network (use the PC's Tailscale address in
  **Add PC…**). A wired connection on at least one side helps a lot. DisplayWire uses UDP port 9000 on the PC; the
  installer opens it in the Windows firewall.

## Install

### On the Mac

With [Homebrew](https://brew.sh):

```
brew install --cask FragAverage/displaywire/displaywire
```

(If Homebrew says it doesn't trust the tap, run `brew trust FragAverage/displaywire` and try again.)

Or download `DisplayWire-macos.zip` from the [latest release](https://github.com/FragAverage/displaywire/releases/latest),
unzip it and drag **DisplayWire** to Applications. The app isn't notarized by Apple, so the first time macOS may
refuse to open it. Clear the flag once:

```
xattr -dr com.apple.quarantine /Applications/DisplayWire.app
```

### On the PC

1. Download `displaywire-server-windows.zip` from the [latest release](https://github.com/FragAverage/displaywire/releases/latest) and unzip it.
2. Double-click `displaywire-server.exe` and answer **Y**. After a UAC prompt it installs itself as a Windows
   service (so it starts with Windows and can show UAC prompts and the lock screen), opens the firewall for it and
   adds an entry to *Installed apps*.

A DisplayWire icon appears in the notification area. Its menu pairs new devices, manages paired ones, opens the
folder of received files, and updates the server.

## Connect your Mac to your PC (once)

1. On the PC, click the DisplayWire tray icon and choose **Pair new device…**
2. On the Mac, open DisplayWire and click **Add PC…**. Enter the PC's address, or just click it if it was found on
   your network.
3. On the PC, click **Allow**. A 6-digit PIN appears. Type it on the Mac.

That's it: the Mac now has its own key and shows your PC as a tile in **Your PCs**. Click the tile any time to connect.
The PC only listens for new devices for two minutes after you click **Pair new device…**, and a wrong PIN three times
cancels the attempt.

To remove a device, open **Manage devices…** from the PC's tray icon and click **Remove**. Its key is destroyed and it
is disconnected at once. (A Mac can also *forget* a PC: right-click its tile.)

## Using it

Click your PC's tile to connect. Keyboard and mouse go to the PC while the stream window has focus. To go back to the
list of PCs, close the stream window or choose **Connection > Disconnect**.

| Shortcut | What it does |
|---|---|
| **⌃⌥M** | Capture or release the mouse (for games: relative motion, Mac cursor hidden) |
| **⌃⌥S** | Show or hide the stats graph: frame rate, bitrate, latency, lost frames, audio delay |
| **⌃⌥V** | Microphone on or off (it is off every time you connect) |
| **⌃⌘F** | Full screen |
| **⌘,** | Settings |
| **⌘Q** | Quit DisplayWire |

The ⌘ key acts as the Windows key on the PC. Switching away from the window lets go of every key and button.

### Sound and microphone

The PC's sound plays on your Mac, from every app, whichever output the app uses. DisplayWire's own sound is left out.

To talk back (a game chat, a call), turn the microphone on with **⌃⌥V**. macOS shows its recording indicator while it
is on. Windows only lists microphones that a signed driver provides, so DisplayWire plays your voice into a virtual
audio cable. Install the free [VB-Cable](https://vb-audio.com/Cable/) on the PC, then in the app that should hear you,
choose **CABLE Output** as the microphone.

### Clipboard and files

- Copied text is shared both ways while connected. Things a password manager marks as secret are never sent.
- To send files, drop them on the stream window, or use **Connection > Send File to PC…**. They land in the PC
  user's `Downloads\DisplayWire` folder (the tray menu opens it).

### Settings (⌘,)

Video bitrate, a frame-rate limit, loss protection, volume, clipboard sharing, and Wi-Fi tuning. The changes apply to
the running stream.

### Smoother Wi-Fi (optional)

On Wi-Fi, macOS regularly hops channels for AirDrop and AirPlay, which shows up as bursts of lag. **Connection >
Set Up Wi-Fi Tuning…** (one password prompt) lets DisplayWire pause that while you are connected and bring it back
when you disconnect or quit. It adds one rule that allows exactly `ifconfig awdl0 down` and `ifconfig awdl0 up`.
To remove it: `sudo rm /etc/sudoers.d/displaywire-awdl`.

## Updates

DisplayWire checks for new versions and asks before installing one. On the Mac, choose **DisplayWire > Check for
Updates…**. On the PC, use the tray menu, or run `displaywire-server --update` as an administrator. Every release is
signed, and the apps only accept an update that matches the key built into them
([how](docs/UPDATES.md)). To turn the automatic checks off, set `DISPLAYWIRE_NO_UPDATE_CHECK=1`.

## If something isn't right

- **The Mac says "no reply" or can't find the PC.** The PC may be off, asleep or on another network; or its firewall
  blocks UDP port 9000 (the installer opens it); or the PC no longer has this Mac paired. In that case the app offers
  **Pair again**.
- **I can't see the PC in the list.** Click **Add PC…** and type its IP address or name. Finding PCs automatically
  only works on the same local network.
- **It stutters or hitches.** Almost always the network. Use a wired connection on one side if you can, set up
  Wi-Fi tuning above, and watch the stats graph (⌃⌥S): lost frames and spikes in latency point at the network.
  `netprobe` (in the release's server zip) measures jitter and loss between the two machines.
- **No sound on the Mac.** Check the PC isn't muted, and that the stream's volume in Settings isn't zero.
- **Nobody hears me.** Install VB-Cable on the PC and pick **CABLE Output** as the microphone in the app, and make
  sure the microphone is on (⌃⌥V).
- **macOS won't open the app.** Run the `xattr` command in the install section.
- **Logs.** The server writes `C:\ProgramData\DisplayWire\server.log`.

Run `displaywire-server --help` and `displaywire-client --help` for the command-line options (a different port,
a fixed bitrate or frame rate, turning clipboard, files, audio or the microphone off, and so on).

## Uninstall

- **Mac:** delete `DisplayWire.app`. Your saved PCs are in `~/Library/Application Support/DisplayWire`.
  With Homebrew: `brew uninstall --cask displaywire`.
- **PC:** *Settings > Apps > Installed apps > DisplayWire Server*, or `displaywire-server.exe --uninstall`
  (add `--purge` to delete the paired devices and logs as well).

## Security

Each device you pair gets a random key of its own; nothing is typed or copied. Everything after the handshake is
encrypted and checked, a recorded session can't be decrypted later, and the PC stays silent to anything unauthenticated.
Pairing needs you at the PC (to click **Pair new device…** and **Allow**) and a PIN that is checked without ever
being sent. Details: [docs/AUTH.md](docs/AUTH.md) and [docs/PAIRING.md](docs/PAIRING.md). To report a vulnerability,
use the repository's *Security* tab rather than a public issue.

## Contributing

Building from source, the project layout, tests and how releases are made are in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
Unless you explicitly state otherwise, any contribution you submit for inclusion in this project, as defined in the
Apache-2.0 license, is dual licensed as above, without any additional terms or conditions.
