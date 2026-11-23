# DisplayWire

Stream your Windows PC's desktop to your Mac, or to another Windows PC, with very low latency. Play games, run apps
and use the PC as if you were sitting at it: the picture, sound, keyboard, mouse, clipboard, files and your microphone
all go across. Each of the PC's monitors can have a window of its own. It works the other way too: a Mac can be the
one streamed, so you can use your Mac from your PC (see [Stream a Mac](#stream-a-mac)).

- **Fast:** hardware video encoding on the PC (NVIDIA), hardware decoding on the Mac, a custom UDP protocol, and
  repair packets so a lost packet doesn't cost a frame. Typically 10 to 20 ms from the PC's screen to yours.
- **Private:** everything is encrypted, and each Mac has its own key. A device that isn't paired can't see or do
  anything, and the PC doesn't even answer it.
- **Simple:** pair with a 6-digit PIN, then click your PC to connect.

## What you need

- **PC:** Windows 10 or 11, with an NVIDIA graphics card for the best performance. (Sound from every app needs
  Windows 10 version 2004 or later; older versions send the default output only.)
- **Mac:** macOS 12 or later, Apple Silicon or Intel.
- **Or a Windows PC to view from:** Windows 10 or 11 with any graphics card that decodes HEVC, and Microsoft's
  *HEVC Video Extensions* (many PCs have them already; otherwise they are in the Microsoft Store).
- Both on the same network, or on the same [Tailscale](https://tailscale.com) network (use the PC's Tailscale address in
  **Add PC…**). A wired connection on at least one side helps a lot. DisplayWire uses UDP port 9000 on the PC; the
  installer opens it in the Windows firewall for the local network and Tailscale only. To reach the PC any other
  way (a port forward, another VPN), add a firewall rule for `displaywire-server.exe` yourself.

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

### On a Windows PC you view from

Download `DisplayWire-windows.zip` from the [latest release](https://github.com/FragAverage/displaywire/releases/latest),
unzip it and run **DisplayWire.exe**. (Releases up to 0.7.0 don't have it yet: build it with
`cargo build --release -p displaywire-client`, see [CONTRIBUTING.md](CONTRIBUTING.md).) Everything below works the
same as on a Mac, except where the *On Windows* section says otherwise.

### On the PC

1. Download `displaywire-server-windows.zip` from the [latest release](https://github.com/FragAverage/displaywire/releases/latest) and unzip it.
2. Double-click `displaywire-server.exe` and answer **Y**. After a UAC prompt it installs itself as a Windows
   service (so it starts with Windows and can show UAC prompts and the lock screen), opens the firewall for it and
   adds an entry to *Installed apps*.

A DisplayWire icon appears in the notification area. Its menu pairs new devices, manages paired ones, opens the
folder of received files, and updates the server.

### Stream a Mac

A Mac (macOS 13 or later) can be streamed too, to a PC or another Mac. Download `DisplayWire-Server-macos.zip` from
the [latest release](https://github.com/FragAverage/displaywire/releases/latest) (releases up to 0.7.0 don't have it:
build it as [CONTRIBUTING.md](CONTRIBUTING.md#the-mac-server) says), unzip it, drag **DisplayWire Server** to
Applications, clear the quarantine flag as for the app above (`xattr -dr com.apple.quarantine "/Applications/DisplayWire Server.app"`)
and open it.
A display icon appears in the menu bar. Then:

1. macOS asks to allow **Screen & System Audio Recording** and **Accessibility** for DisplayWire Server (System
   Settings › Privacy & Security). Allow both, then quit it from its menu and open it again. The menu shows a
   shortcut for each permission that's still missing.
2. Choose **Start at Login** in its menu so it runs whenever you're logged in.
3. Pair from the viewing computer as below, choosing **Pair New Device…** in the Mac's menu instead of the PC's tray.

The Windows key acts as ⌘ and Alt as ⌥. Files you send land in `~/Downloads/DisplayWire`. To send your microphone
to the Mac, install [BlackHole 2ch](https://existential.audio/blackhole/) (`brew install blackhole-2ch`) and choose
**BlackHole 2ch** as the microphone in the Mac app you're using. A Mac can't be streamed at the login window, and
controllers aren't passed through to a Mac yet. The log is `~/Library/Logs/DisplayWire/server.log` when it starts at
login.

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

To skip ⌃⌥M, turn on **Capture the pointer when the window has focus** in Settings. The mouse is then captured
whenever the stream window has focus. ⌃⌥M still releases it, a click in the window captures it again, and
switching away (⌘Tab) releases it.

### On Windows

The Windows client has the same list of PCs, pairing and settings, and a menu bar on the stream window.

| Shortcut | What it does |
|---|---|
| **Ctrl+Alt+M** | Capture or release the mouse |
| **Ctrl+Alt+F** | Full screen (for the stream window that has focus) |
| **Ctrl+Shift+W** | Full screen for every stream window at once (each on its own monitor), or back to windows |
| **Ctrl+Alt+A** | Open every monitor of the PC, each in a window of its own |
| **Ctrl+Alt+S** | Show or hide the stats graph |
| **Ctrl+Alt+V** | Microphone on or off |
| **Ctrl+Alt+D** | Disconnect |

- **More than one monitor:** **Monitors** in the menu chooses which one the main window shows, and opens the others
  in windows of their own (each goes to a display of yours that has no stream window yet). The windows that were open
  are opened again the next time you connect to that PC.
- **The Windows key, Alt+Tab and Alt+F4** go to the remote PC while the stream is in full screen or the mouse is
  captured. In an ordinary window they stay with the PC you are sitting at.
- Xbox-style (XInput) controllers work, with rumble. Wi-Fi tuning needs no setup. There are no automatic updates yet.

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
  A plain `ping` to the PC that shows irregular times is the same story.
- **No sound on the Mac.** Check the PC isn't muted, and that the stream's volume in Settings isn't zero.
- **Nobody hears me.** Install VB-Cable on the PC and pick **CABLE Output** as the microphone in the app, and make
  sure the microphone is on (⌃⌥V).
- **macOS won't open the app.** Run the `xattr` command in the install section.
- **Logs.** The server writes `C:\ProgramData\DisplayWire\server.log` (a Mac server started at login:
  `~/Library/Logs/DisplayWire/server.log`).

Run `displaywire-server --help` and `displaywire-client --help` for the command-line options (a different port,
a fixed bitrate or frame rate, turning clipboard, files, audio or the microphone off, and so on).

## Uninstall

- **Mac:** delete `DisplayWire.app`. Your saved PCs are in `~/Library/Application Support/DisplayWire`.
  With Homebrew: `brew uninstall --cask displaywire`.
- **PC:** *Settings > Apps > Installed apps > DisplayWire Server*, or `displaywire-server.exe --uninstall`
  (add `--purge` to delete the paired devices and logs as well).
- **A streamed Mac:** turn off **Start at Login** in its menu, quit it and delete `DisplayWire Server.app`. Its paired
  devices are in `~/Library/Application Support/DisplayWire Server`.

## Security

Each device you pair gets a random key of its own; nothing is typed or copied. Everything after the handshake is
encrypted and checked, a recorded session can't be decrypted later, and the PC stays silent to anything unauthenticated.
Pairing needs you at the PC (to click **Pair new device…** and **Allow**) and a PIN that is checked without ever
being sent. Details: [docs/AUTH.md](docs/AUTH.md) and [docs/PAIRING.md](docs/PAIRING.md). To report a vulnerability,
use the repository's *Security* tab rather than a public issue.

## Contributing

Building from source, the project layout, tests and how releases are made are in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under the [MIT license](LICENSE). Contributions are accepted under the same licence.
