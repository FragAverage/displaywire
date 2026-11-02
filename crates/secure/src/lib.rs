//! Pairing key, handshake and datagram encryption. See `docs/AUTH.md`.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use snow::{Builder, StatelessTransportState};

pub mod pair;

const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const MAGIC: [u8; 2] = *b"DW";
pub const HS1: u8 = 0x20;
pub const HS2: u8 = 0x21;
pub const SEC: u8 = 0x22;
const TAG: usize = 16;
/// "DW", type, session id, counter.
const HEADER: usize = 2 + 1 + 4 + 8;
/// Bytes added to every datagram.
pub const OVERHEAD: usize = HEADER + TAG;
// `proto` sizes its messages for this.
const _: () = assert!(OVERHEAD == proto::SECURE_OVERHEAD);
/// Largest handshake datagram.
pub const MAX_HANDSHAKE: usize = 256;
/// How far apart client and server clocks may be, in a handshake.
const MAX_SKEW_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    BadKey,
    /// Wrong key, damaged or replayed handshake, or clocks too far apart.
    Rejected,
    Crypto,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::BadKey => "not a valid pairing key (expected dw1- followed by 64 hex digits)",
            Error::Rejected => "handshake rejected",
            Error::Crypto => "crypto error",
        })
    }
}

impl std::error::Error for Error {}

/// The 32-byte pairing key, written `dw1-<64 hex digits>`.
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; 32]);

impl Key {
    pub fn generate() -> Key {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).expect("system random number generator");
        Key(k)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Key {
        Key(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn parse(s: &str) -> Result<Key, Error> {
        let hex = s.trim().strip_prefix("dw1-").ok_or(Error::BadKey)?;
        if hex.len() != 64 || !hex.is_ascii() {
            return Err(Error::BadKey);
        }
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| Error::BadKey)?;
        }
        Ok(Key(k))
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("dw1-")?;
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

// Never print a key by accident.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

/// What the client says about itself in its handshake.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct ClientHello {
    pub version: u16,
    pub caps: u8,
}

/// What the server says in its reply.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct ServerHello {
    pub version: u16,
}

fn builder(key: &Key) -> Result<Builder<'_>, Error> {
    let params = PATTERN.parse().map_err(|_| Error::Crypto)?;
    Builder::new(params).psk(0, &key.0).map_err(|_| Error::Crypto)
}

/// The client's side of a handshake in progress.
pub struct Initiator {
    state: snow::HandshakeState,
}

impl Initiator {
    /// Starts a handshake. Returns it with the HS1 datagram to send (repeat that
    /// until a reply comes; the reply is for this `Initiator` only).
    pub fn start(key: &Key, hello: ClientHello, now_unix_ms: u64) -> Result<(Initiator, Vec<u8>), Error> {
        let mut state = builder(key)?.build_initiator().map_err(|_| Error::Crypto)?;
        let mut payload = Vec::with_capacity(11);
        payload.extend_from_slice(&hello.version.to_le_bytes());
        payload.push(hello.caps);
        payload.extend_from_slice(&now_unix_ms.to_le_bytes());
        let mut out = vec![0u8; MAX_HANDSHAKE];
        out[..3].copy_from_slice(&[MAGIC[0], MAGIC[1], HS1]);
        let n = state.write_message(&payload, &mut out[3..]).map_err(|_| Error::Crypto)?;
        out.truncate(3 + n);
        Ok((Initiator { state }, out))
    }

    /// Completes the handshake with the server's HS2 datagram.
    pub fn finish(mut self, hs2: &[u8]) -> Result<(Session, ServerHello), Error> {
        let body = hs2.strip_prefix(&[MAGIC[0], MAGIC[1], HS2]).ok_or(Error::Rejected)?;
        let mut payload = [0u8; 64];
        let n = self.state.read_message(body, &mut payload).map_err(|_| Error::Rejected)?;
        if n != 6 {
            return Err(Error::Rejected);
        }
        let hello = ServerHello { version: u16::from_le_bytes([payload[0], payload[1]]) };
        let id = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
        let state = self.state.into_stateless_transport_mode().map_err(|_| Error::Crypto)?;
        Ok((Session::new(id, state), hello))
    }
}

/// Server side: checks an HS1 and, if it is valid, answers it. Returns the HS2
/// datagram to send back, the new session (pending until the client proves
/// itself with a datagram encrypted under it) and what the client said.
pub fn accept(key: &Key, hs1: &[u8], now_unix_ms: u64, server_version: u16) -> Result<(Vec<u8>, Session, ClientHello), Error> {
    let body = hs1.strip_prefix(&[MAGIC[0], MAGIC[1], HS1]).ok_or(Error::Rejected)?;
    let mut state = builder(key)?.build_responder().map_err(|_| Error::Crypto)?;
    // Authenticates the key with hashing only; no Diffie-Hellman has happened yet.
    let mut payload = [0u8; 64];
    let n = state.read_message(body, &mut payload).map_err(|_| Error::Rejected)?;
    if n != 11 {
        return Err(Error::Rejected);
    }
    let hello = ClientHello { version: u16::from_le_bytes([payload[0], payload[1]]), caps: payload[2] };
    let sent = u64::from_le_bytes(payload[3..11].try_into().unwrap());
    if sent.abs_diff(now_unix_ms) > MAX_SKEW_MS {
        return Err(Error::Rejected);
    }

    let mut id = [0u8; 4];
    getrandom::fill(&mut id).map_err(|_| Error::Crypto)?;
    let id = u32::from_le_bytes(id);
    let mut reply = Vec::with_capacity(6);
    reply.extend_from_slice(&server_version.to_le_bytes());
    reply.extend_from_slice(&id.to_le_bytes());
    let mut out = vec![0u8; MAX_HANDSHAKE];
    out[..3].copy_from_slice(&[MAGIC[0], MAGIC[1], HS2]);
    let n = state.write_message(&reply, &mut out[3..]).map_err(|_| Error::Crypto)?;
    out.truncate(3 + n);
    let state = state.into_stateless_transport_mode().map_err(|_| Error::Crypto)?;
    Ok((out, Session::new(id, state), hello))
}

/// Datagrams already seen from the other side, so a replay is dropped.
pub struct ReplayWindow {
    highest: u64,
    seen_any: bool,
    bits: [u64; Self::WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        ReplayWindow { highest: 0, seen_any: false, bits: [0; Self::WORDS] }
    }
}

impl ReplayWindow {
    const WORDS: usize = 32;
    pub const SIZE: u64 = Self::WORDS as u64 * 64;

    fn bit(n: u64) -> (usize, u64) {
        let i = n % Self::SIZE;
        ((i / 64) as usize, 1 << (i % 64))
    }

    pub fn is_new(&self, n: u64) -> bool {
        if !self.seen_any || n > self.highest {
            return true;
        }
        if self.highest - n >= Self::SIZE {
            return false;
        }
        let (w, m) = Self::bit(n);
        self.bits[w] & m == 0
    }

    pub fn mark(&mut self, n: u64) {
        if !self.seen_any {
            self.seen_any = true;
            self.highest = n;
            self.bits = [0; Self::WORDS];
        } else if n > self.highest {
            let gap = n - self.highest;
            if gap >= Self::SIZE {
                self.bits = [0; Self::WORDS];
            } else {
                for k in self.highest + 1..=n {
                    let (w, m) = Self::bit(k);
                    self.bits[w] &= !m;
                }
            }
            self.highest = n;
        }
        let (w, m) = Self::bit(n);
        self.bits[w] |= m;
    }
}

/// Keys for one connection, both directions. Shared by the sending threads.
pub struct Session {
    id: u32,
    state: StatelessTransportState,
    counter: AtomicU64,
}

impl Session {
    fn new(id: u32, state: StatelessTransportState) -> Session {
        Session { id, state, counter: AtomicU64::new(0) }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// Encrypts `plain` into a datagram in `out` (which must hold
    /// `plain.len() + OVERHEAD`) and returns its length.
    pub fn seal(&self, plain: &[u8], out: &mut [u8]) -> usize {
        let counter = self.counter.fetch_add(1, Relaxed);
        out[..3].copy_from_slice(&[MAGIC[0], MAGIC[1], SEC]);
        out[3..7].copy_from_slice(&self.id.to_le_bytes());
        out[7..HEADER].copy_from_slice(&counter.to_le_bytes());
        let n = self.state.write_message(counter, plain, &mut out[HEADER..]).expect("buffer sized for the message");
        HEADER + n
    }

    /// Decrypts a datagram from the other side into `out`. `None` if it isn't
    /// ours, doesn't verify, or was seen before.
    pub fn open(&self, datagram: &[u8], window: &mut ReplayWindow, out: &mut [u8]) -> Option<usize> {
        let (id, counter) = parse_header(datagram)?;
        if id != self.id || !window.is_new(counter) {
            return None;
        }
        let n = self.state.read_message(counter, &datagram[HEADER..], out).ok()?;
        window.mark(counter);
        Some(n)
    }
}

/// Session id and counter of a transport datagram, if it is one.
pub fn parse_header(datagram: &[u8]) -> Option<(u32, u64)> {
    if datagram.len() < OVERHEAD || datagram[..3] != [MAGIC[0], MAGIC[1], SEC] {
        return None;
    }
    Some((u32::from_le_bytes(datagram[3..7].try_into().ok()?), u64::from_le_bytes(datagram[7..HEADER].try_into().ok()?)))
}

/// The type byte of a datagram that starts with the protocol magic.
pub fn kind(datagram: &[u8]) -> Option<u8> {
    (datagram.len() >= 3 && datagram[..2] == MAGIC).then(|| datagram[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn pair() -> (Session, Session) {
        let key = Key::generate();
        let (init, hs1) = Initiator::start(&key, ClientHello { version: 5, caps: 3 }, NOW).unwrap();
        let (hs2, server, hello) = accept(&key, &hs1, NOW, 5).unwrap();
        assert_eq!(hello, ClientHello { version: 5, caps: 3 });
        let (client, server_hello) = init.finish(&hs2).unwrap();
        assert_eq!(server_hello.version, 5);
        assert_eq!(client.id(), server.id());
        (client, server)
    }

    #[test]
    fn key_roundtrip_and_validation() {
        let k = Key::generate();
        assert_eq!(Key::parse(&k.to_string()).unwrap(), k);
        assert_eq!(Key::parse(&format!("  {k}\n")).unwrap(), k);
        for bad in ["", "dw1-", "dw1-zz", &format!("dw2-{}", &k.to_string()[4..]), &k.to_string()[..40], &format!("{k}00")] {
            assert_eq!(Key::parse(bad), Err(Error::BadKey), "{bad}");
        }
        assert_eq!(format!("{k:?}"), "Key(..)");
    }

    #[test]
    fn datagrams_flow_both_ways() {
        let (client, server) = pair();
        let (mut cw, mut sw) = (ReplayWindow::default(), ReplayWindow::default());
        let mut buf = [0u8; 1200];
        let mut out = [0u8; 1200];
        for i in 0..50u8 {
            let plain = vec![i; 100 + i as usize];
            let n = client.seal(&plain, &mut buf);
            assert_eq!(n, plain.len() + OVERHEAD);
            let m = server.open(&buf[..n], &mut sw, &mut out).expect("client to server");
            assert_eq!(&out[..m], &plain[..]);
            let n = server.seal(&plain, &mut buf);
            let m = client.open(&buf[..n], &mut cw, &mut out).expect("server to client");
            assert_eq!(&out[..m], &plain[..]);
        }
    }

    #[test]
    fn wrong_key_is_rejected_before_any_reply() {
        let (_, hs1) = Initiator::start(&Key::generate(), ClientHello { version: 5, caps: 0 }, NOW).unwrap();
        assert_eq!(accept(&Key::generate(), &hs1, NOW, 5).err(), Some(Error::Rejected));
    }

    #[test]
    fn handshake_clock_skew_and_garbage() {
        let key = Key::generate();
        let (_, hs1) = Initiator::start(&key, ClientHello { version: 5, caps: 0 }, NOW).unwrap();
        assert!(accept(&key, &hs1, NOW + MAX_SKEW_MS, 5).is_ok());
        assert_eq!(accept(&key, &hs1, NOW + MAX_SKEW_MS + 1, 5).err(), Some(Error::Rejected));
        assert_eq!(accept(&key, &[b'S', b'Y', HS1, 1, 2, 3], NOW, 5).err(), Some(Error::Rejected));
        assert_eq!(accept(&key, b"", NOW, 5).err(), Some(Error::Rejected));
        let mut damaged = hs1.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert_eq!(accept(&key, &damaged, NOW, 5).err(), Some(Error::Rejected));
    }

    #[test]
    fn replays_tampering_and_strangers_are_dropped() {
        let (client, server) = pair();
        let (_, stranger) = pair();
        let mut win = ReplayWindow::default();
        let mut buf = [0u8; 300];
        let mut out = [0u8; 300];
        let n = client.seal(b"hello", &mut buf);
        let good = buf[..n].to_vec();
        assert!(server.open(&good, &mut win, &mut out).is_some());
        assert!(server.open(&good, &mut win, &mut out).is_none(), "replay");
        let mut bad = good.clone();
        bad[HEADER] ^= 1;
        assert!(server.open(&bad, &mut ReplayWindow::default(), &mut out).is_none(), "tampered");
        let mut bad = good.clone();
        bad[8] ^= 1; // counter changed: a different nonce
        assert!(server.open(&bad, &mut ReplayWindow::default(), &mut out).is_none(), "counter changed");
        assert!(stranger.open(&good, &mut ReplayWindow::default(), &mut out).is_none(), "wrong session");
        // A datagram can't be turned around and played back to its sender.
        assert!(client.open(&good, &mut ReplayWindow::default(), &mut out).is_none(), "reflected");
    }

    #[test]
    fn replay_window_accepts_reordering_but_not_repeats() {
        let mut w = ReplayWindow::default();
        for n in [5u64, 3, 4, 100, 98, 99, 5000, 4999] {
            assert!(w.is_new(n), "{n} should be new");
            w.mark(n);
            assert!(!w.is_new(n), "{n} should now be seen");
        }
        // Far behind the newest: dropped even though never seen.
        assert!(!w.is_new(5000 - ReplayWindow::SIZE));
        assert!(w.is_new(5000 - ReplayWindow::SIZE + 1));
        // A huge jump forgets everything older.
        w.mark(1_000_000);
        assert!(!w.is_new(5000));
        assert!(w.is_new(999_999));
    }

    #[test]
    fn counters_are_unique_across_threads() {
        let (client, server) = pair();
        let client = std::sync::Arc::new(client);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let c = client.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 100];
                    (0..500).map(|_| c.seal(b"x", &mut buf)).count()
                })
            })
            .collect();
        for t in threads {
            assert_eq!(t.join().unwrap(), 500);
        }
        // 2000 datagrams used 2000 distinct counters: the next one is #2000.
        let mut buf = [0u8; 100];
        let n = client.seal(b"x", &mut buf);
        assert_eq!(parse_header(&buf[..n]).unwrap().1, 2000);
        let _ = server;
    }
}
