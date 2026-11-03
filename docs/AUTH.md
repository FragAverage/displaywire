# DisplayWire authentication and encryption (protocol v5)

Status: implemented in `crates/secure` and used by the server and client (protocol
v5). Replaces the unauthenticated protocol of v4 and earlier.

## Goals

- Only a client holding the server's **pairing key** can see the screen, send
  input, or use clipboard and file transfer.
- Everything after the handshake (video, audio, input, clipboard, files) is
  encrypted and integrity-protected, with replay protection.
- A passive attacker can't decrypt recorded traffic later, even if the key
  leaks afterwards (forward secrecy).
- The server stays silent to anything unauthenticated, with two exceptions a
  device needs before it has a key: discovery replies (only to private,
  link-local and loopback addresses, rate-limited, and they name the PC and its
  port), and pairing replies (only while someone has chosen **Pair new
  device…** on the PC, see `docs/PAIRING.md`). No amplification beyond that
  small reply, nothing to fingerprint from outside the local network.
- Pairing must be possible **without sitting at the server**: the key is a
  small string that can be copied over SSH or any other channel.
- No cost on the hot path beyond ~1 µs of ChaCha20-Poly1305 per datagram.

Per-device keys, revoking a single device and PIN pairing are described in `docs/PAIRING.md`. Non-goals (for
now): certificate-style identity for the server. See "Later".

## Pairing key

32 random bytes, written as `dw1-` plus 64 hex characters. **Each paired device has its own** (see
`docs/PAIRING.md`): a new device is given one by the server during PIN pairing, and nobody types or copies it.

- Server: the list of devices and their keys is `C:\ProgramData\DisplayWire\clients`, readable only by SYSTEM
  and Administrators. The server locks that folder down to them before using it, and refuses a file owned by
  anyone else, so a standard user can't plant their own key. On a handshake it tries each device's key; the one
  that opens it says who is connecting. Removing a device (`displaywire-server --remove-client`, or the window)
  disconnects it and refuses its key from then on.
- Client: the key for each PC is kept with the saved PCs in
  `~/Library/Application Support/DisplayWire/pcs`, mode 0600.

Anyone holding a device's key can connect as that device. Treat it like a password.

## Handshake

Noise `NNpsk0_25519_ChaChaPoly_BLAKE2s` (the `snow` crate): both sides mix the
pairing key in first, then exchange ephemeral X25519 keys. This gives mutual
authentication (only a holder of the key can produce a valid message, either
way), forward secrecy, and two directional keys.

```
client -> server   HS1: "DW" 0x20 | noise msg 1
                        payload (encrypted): version u16, caps u8, unix_ms u64
server -> client   HS2: "DW" 0x21 | noise msg 2
                        payload (encrypted): version u16, session u32
```

- The client repeats HS1 every 200 ms until HS2 arrives.
- The server checks HS1 with hashing only (the PSK is mixed in before any
  Diffie-Hellman), so garbage and floods are rejected cheaply. It also
  rejects a `unix_ms` more than 5 minutes from its own clock, and rate-limits
  handshakes.
- A valid HS1 creates a **pending** session for the sender's address. It does
  not become the active session, and the server sends nothing but HS2, until a
  datagram **encrypted under it** arrives from that address. So a replayed HS1
  from an attacker yields only an HS2 they can't use, and can't displace the
  real client.
- A new HS1 from the active client (restart, reconnect) replaces its session
  once it too proves itself with a valid datagram.

## Transport

After the handshake every datagram is:

```
"DW" | 0x22 | session u32 | counter u64 | ciphertext | tag(16)
```

- Plaintext is a message as `Msg::encode` writes it. The header is sent in the
  clear; the counter in it is the AEAD nonce, so altering it makes decryption
  fail, and the session id only selects which keys to try.
- The counter is a 64-bit number per direction (shared by all sending threads
  through an atomic), never reused, never wraps in practice.
- The receiver keeps a 2048-wide sliding replay window per direction and drops
  duplicates and anything older.
- Overhead is 31 bytes, so `MAX_PLAINTEXT` = 1200 - 31 = 1169 and the video and
  audio payload sizes shrink to match (video: 1146 bytes).
- Datagrams with an unknown session, a bad tag or a replayed counter are
  dropped silently.

## Server behaviour

- One active client at a time, as before. It becomes active on its first
  valid transport datagram, and times out as before after 3 s of silence.
- Handshakes are checked before any rate limit applies (a forgery costs only
  hashing), with a ceiling of 2000 checks a second. Handshakes that verify are
  limited to 2 a second (bursts of 5) per source address, and one address holds
  at most 2 of the 16 pending sessions, so a flood can't crowd out a real device.
- Video, audio and everything else are sent only to the active client, only
  through its session.
- Clipboard and file features exist only inside the session, so by construction
  only authenticated clients can use them.

## Failure modes the client reports

- No HS2 within 5 s: server not running, firewall, **or the PC no longer has this device paired**
  (the server deliberately doesn't say which). The error names them, and the Mac app offers to pair again.
- Session lost mid-stream: the client re-handshakes automatically.

## Later

- Windows firewall scoping to LAN and tailnet only.
- More than one device connected at a time (today one is active; another connecting takes over).
