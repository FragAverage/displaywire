# Pairing and devices

Status: implemented (`crates/secure/src/pair.rs`, `crates/pairing`, `server/src/{clients,pairing,uiserver,ui}.rs`,
`client/src/{servers,pairui,launcher}.rs`). Builds on the handshake in `docs/AUTH.md`.

## What it does

- **Every paired device has a key of its own.** The server keeps a list (name, key, when it was paired, when it
  last connected). Removing a device deletes its key: its session is cut within a fraction of a second and its
  handshakes are refused from then on. Other devices are not affected.
- **A new device pairs with a 6-digit PIN shown on the PC.** Nothing is typed on the PC and no key is typed or
  copied anywhere: the PC hands the new device its key over the network once the PIN is proven.
- The Mac app has a list of PCs ("Your PCs") with a tile per PC; one click connects. The PC has a window
  listing paired devices, with Remove, and the pairing prompt.

## The flow

1. At the PC, tray icon → **Pair new device…** (or `displaywire-server --pair` over SSH). The server starts
   *listening* for two minutes. Until then it stays completely silent to pairing requests, as it is to everything
   else unauthenticated.
2. On the Mac: **Add PC…** (or click a PC that isn't paired). The Mac asks to pair, calling itself by its
   computer name.
3. The PC shows **"Jamie's MacBook wants to pair"**, with the address it came from, and Allow / Deny.
4. **Allow** shows a fresh 6-digit PIN on the PC.
5. Type it on the Mac. If it is right, the Mac is told it is paired and stores its new key; the PC lists it.

Wrong PIN: three tries (the PC shows how many are left), then the pairing is cancelled and a new request and a new
PIN are needed. The PIN also expires after two minutes.

## Why not just a key derived from the PIN

Six digits is a million possibilities. Used as an ordinary secret in the handshake, anyone who recorded one
exchange could try all of them offline in well under a second. So the PIN is the password of a **PAKE (SPAKE2,
Ed25519)**: an eavesdropper learns nothing they can test guesses against, and each guess costs a live exchange with
the server, of which there are three. The exchange also proves both sides know the PIN (so a rogue server can't
hand out a key either), then carries the new key sealed under the shared secret (ChaCha20-Poly1305).

```
client -> server   REQ      pid, device name                    (server: Waiting -> person at the PC allows -> PinRequired)
client -> server   MSG_A    pid, SPAKE2 message                 (after the PIN is typed)
server -> client   MSG_B    pid, SPAKE2 message
client -> server   CONFIRM  pid, tag                            (proves the client used the same PIN)
server -> client   DONE     pid, sealed(new key, server name)   or STATUS WrongPin(n) / Locked
```

All of these are small unauthenticated UDP datagrams (`"DW"` + type 0x40..=0x46 + an 8-byte pairing id); the client
repeats each until it is answered, and the server answers repeats identically (so a lost reply loses nothing).
Replies are never larger than the request that caused them.

## Limits and what they protect against

- **Silent unless listening.** Outside the two minutes after "Pair new device…" the server sends nothing in reply
  to pairing packets.
- **One pairing at a time.** A second device is told "busy". Requests only from the address that started.
- **Nothing happens without a person at the PC.** A request just shows a prompt. Allow is a click on the PC (or a
  `y` in an administrator's console, for `--pair`); the PIN is only ever shown on the PC.
- **Three wrong PINs, then locked.** Rate limiting on pairing packets as a whole.
- A fresh random key per device (32 bytes from the OS), never derived from the PIN.

Not covered: someone with administrator rights on the PC can do anything, including pair themselves.

## Where things are stored

- **Server:** `C:\ProgramData\DisplayWire\clients` (override with `--data-dir`), one tab-separated line per device,
  readable only by SYSTEM and Administrators (the folder is locked down, and a file owned by anyone else is
  refused). `displaywire-server --list-clients` and `--remove-client <id or name>` work on it directly, even while
  the service runs.
- **Upgrading:** the single shared key of earlier versions is kept, once, as a device called **"Previous device"**
  so nobody is locked out by the upgrade. Remove it when the devices have paired with a PIN.
- **Mac:** `~/Library/Application Support/DisplayWire/pcs` (mode 0600): address, name, key, last used. Earlier
  lists of recent servers and per-address key files are imported the first time.

## The windows

- **PC:** the tray icon's **Pair new device…** and **Manage devices…** open the device window. It runs as a separate
  process, as the logged-in user (not SYSTEM), so a problem in it can't interrupt a stream. It talks to the server
  over a loopback TCP connection that has to begin with a random token; the token is passed on the command line by
  the server and kept in `ProgramData\DisplayWire\ui`, readable by administrators only (that's how `--pair` over
  SSH works). A pairing request while the window is closed opens it. Built with egui.
- **Mac:** the launcher grid and the PIN window are plain AppKit. Tiles show online/offline (the PC answers a
  discovery query), PCs found on the network, and "Not paired: click to pair". Right-click a tile to rename or
  forget it. If a PC stops answering because it removed this Mac, the app offers to pair again.

## Command line

```
displaywire-server --pair                     # pair a device from a console (SSH): allow it here, read out the PIN
displaywire-server --list-clients
displaywire-server --remove-client "Jamie's MacBook"
displaywire-client --pair <server>            # pair from a terminal, asks for the PIN
tools/pairprobe <server:port> --name NAME     # test tool: pairs and prints the key it was given
```
