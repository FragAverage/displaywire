//! Reliable transfer of files and clipboard text over the lossy datagram link.
//!
//! A transfer is numbered blocks: block 0 describes it (`Meta`), blocks 1.. carry
//! the data, `BLOCK` bytes each. The receiver acknowledges with the first block
//! it is missing plus a bitmap of the 64 after it; the sender keeps a window of
//! blocks in flight, retransmits on a timer fed by measured round-trip times,
//! and never sends faster than its rate cap, so an upload can't starve video.
//!
//! This crate only decides what to send and what has arrived. Sockets, files
//! and clocks belong to the caller.

mod endpoint;
pub use endpoint::{Endpoint, Event, Outgoing};

use std::collections::BTreeMap;
use std::io;

/// Data bytes per block.
pub const BLOCK: usize = 1024;
/// Blocks that may be unacknowledged at once.
pub const WINDOW: usize = 512;
/// Longest file name carried.
pub const MAX_NAME: usize = 255;

const MS: u64 = 1_000_000;
const INITIAL_RTO_NS: u64 = 250 * MS;
const MIN_RTO_NS: u64 = 40 * MS;
const MAX_RTO_NS: u64 = 1_000 * MS;
/// No acknowledgement progress for this long: give up.
pub const STALL_NS: u64 = 15_000 * MS;
/// The receiver acknowledges after this many new blocks, or this long.
const ACK_EVERY: u32 = 8;
const ACK_AFTER_NS: u64 = 10 * MS;
/// Blocks held that arrived before the transfer's description did.
const MAX_EARLY: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File = 0,
    Clipboard = 1,
}

/// What block 0 says about a transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    /// Bytes of data.
    pub size: u64,
    /// File name (no directories) or empty.
    pub name: String,
}

impl Meta {
    pub fn encode(&self) -> Vec<u8> {
        let name = &self.name.as_bytes()[..floor_char_boundary(&self.name, MAX_NAME)];
        let mut v = Vec::with_capacity(9 + name.len());
        v.push(self.kind as u8);
        v.extend_from_slice(&self.size.to_le_bytes());
        v.extend_from_slice(name);
        v
    }

    pub fn decode(b: &[u8]) -> Option<Meta> {
        let kind = match *b.first()? {
            0 => Kind::File,
            1 => Kind::Clipboard,
            _ => return None,
        };
        let size = u64::from_le_bytes(b.get(1..9)?.try_into().ok()?);
        let name = std::str::from_utf8(b.get(9..)?).ok()?.to_string();
        (name.len() <= MAX_NAME).then_some(Meta { kind, size, name })
    }

    /// Blocks in the transfer, the description included.
    pub fn blocks(&self) -> u32 {
        1 + self.size.div_ceil(BLOCK as u64).min(u32::MAX as u64 - 1) as u32
    }
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Where a sender reads its data from.
pub trait Source {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

/// Data already in memory (clipboard text).
pub struct Bytes(pub Vec<u8>);

impl<T: Source + ?Sized> Source for Box<T> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, buf)
    }
}

impl Source for Bytes {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }
}

impl Source for std::fs::File {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::io::{Read, Seek, SeekFrom};
        self.seek(SeekFrom::Start(offset))?;
        let mut done = 0;
        while done < buf.len() {
            match self.read(&mut buf[done..])? {
                0 => break,
                n => done += n,
            }
        }
        Ok(done)
    }
}

/// Where a receiver puts its data.
pub trait Sink: Send {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Every block has arrived.
    fn finish(self: Box<Self>) -> io::Result<()>;
    /// The transfer was abandoned.
    fn abort(self: Box<Self>) {}
}

/// Limits the send rate: a token bucket of bytes.
struct Pacer {
    bytes_per_ns: f64,
    tokens: f64,
    burst: f64,
    last_ns: u64,
}

impl Pacer {
    fn new(bytes_per_sec: u64) -> Pacer {
        let burst = (BLOCK * 32) as f64;
        Pacer { bytes_per_ns: bytes_per_sec as f64 / 1e9, tokens: burst, burst, last_ns: 0 }
    }

    fn take(&mut self, now: u64, bytes: usize) -> bool {
        if self.last_ns != 0 {
            self.tokens = (self.tokens + now.saturating_sub(self.last_ns) as f64 * self.bytes_per_ns).min(self.burst);
        }
        self.last_ns = now;
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }
}

struct Flight {
    sent_ns: u64,
    tries: u32,
}

pub struct Sender<S: Source> {
    meta: Meta,
    source: S,
    total: u32,
    /// Next block never sent yet. Everything below it is either in `flying` or acknowledged.
    next_new: u32,
    flying: BTreeMap<u32, Flight>,
    pacer: Pacer,
    srtt_ns: Option<f64>,
    rttvar_ns: f64,
    rto_ns: u64,
    last_progress_ns: u64,
}

impl<S: Source> Sender<S> {
    /// `rate` is the most bytes per second to send.
    pub fn new(meta: Meta, source: S, rate: u64) -> Sender<S> {
        let total = meta.blocks();
        Sender {
            meta,
            source,
            total,
            next_new: 0,
            flying: BTreeMap::new(),
            pacer: Pacer::new(rate.max(BLOCK as u64)),
            srtt_ns: None,
            rttvar_ns: 0.0,
            rto_ns: INITIAL_RTO_NS,
            last_progress_ns: 0,
        }
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Bytes of data acknowledged so far, and in all.
    pub fn progress(&self) -> (u64, u64) {
        let acked_blocks = (self.next_new as usize).saturating_sub(self.flying.len()).saturating_sub(1);
        ((acked_blocks as u64 * BLOCK as u64).min(self.meta.size), self.meta.size)
    }

    pub fn is_done(&self) -> bool {
        self.next_new == self.total && self.flying.is_empty()
    }

    /// Nothing has been acknowledged for too long.
    pub fn is_stalled(&self, now: u64) -> bool {
        self.last_progress_ns != 0 && !self.flying.is_empty() && now.saturating_sub(self.last_progress_ns) > STALL_NS
    }

    /// Sends what is due: blocks whose retransmit timer ran out, then new ones,
    /// as far as the window and rate allow. `send` gets (block index, payload).
    pub fn poll(&mut self, now: u64, send: &mut dyn FnMut(u32, &[u8])) -> io::Result<()> {
        if self.last_progress_ns == 0 {
            self.last_progress_ns = now;
        }
        let overdue: Vec<u32> = self.flying.iter().filter(|(_, f)| now.saturating_sub(f.sent_ns) >= self.rto_ns).map(|(&i, _)| i).collect();
        let mut timed_out = false;
        for i in overdue {
            if !self.pacer.take(now, BLOCK) {
                break;
            }
            let tries = self.flying[&i].tries + 1;
            self.send_block(i, now, tries, send)?;
            timed_out = true;
        }
        if timed_out {
            // Back off: the path is slower or lossier than we thought.
            self.rto_ns = (self.rto_ns * 2).min(MAX_RTO_NS);
        }
        while self.flying.len() < WINDOW && self.next_new < self.total && self.pacer.take(now, BLOCK) {
            let i = self.next_new;
            self.next_new += 1;
            self.send_block(i, now, 1, send)?;
        }
        Ok(())
    }

    fn send_block(&mut self, i: u32, now: u64, tries: u32, send: &mut dyn FnMut(u32, &[u8])) -> io::Result<()> {
        let mut buf = [0u8; BLOCK];
        let payload: &[u8] = if i == 0 {
            &[]
        } else {
            let offset = (i as u64 - 1) * BLOCK as u64;
            let want = (self.meta.size - offset).min(BLOCK as u64) as usize;
            let got = self.source.read_at(offset, &mut buf[..want])?;
            if got != want {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the source is shorter than it said"));
            }
            &buf[..got]
        };
        if i == 0 {
            send(0, &self.meta.encode());
        } else {
            send(i, payload);
        }
        self.flying.insert(i, Flight { sent_ns: now, tries });
        Ok(())
    }

    /// An acknowledgement: every block below `next` has arrived, and so have the
    /// ones set in `sack` (bit `n` is block `next + 1 + n`).
    pub fn on_ack(&mut self, now: u64, next: u32, sack: u64) {
        let before = self.flying.len();
        let mut sample: Option<u64> = None;
        let mut done = |flying: &mut BTreeMap<u32, Flight>, i: u32| {
            if let Some(f) = flying.remove(&i)
                && f.tries == 1
            {
                // The newest block acknowledged gives the freshest round-trip time.
                sample = Some(sample.map_or(now - f.sent_ns, |s| s.min(now - f.sent_ns)));
            }
        };
        let below: Vec<u32> = self.flying.range(..next).map(|(&i, _)| i).collect();
        for i in below {
            done(&mut self.flying, i);
        }
        for bit in 0..64u32 {
            if sack >> bit & 1 == 1 {
                done(&mut self.flying, next.saturating_add(1).saturating_add(bit));
            }
        }
        if self.flying.len() < before {
            self.last_progress_ns = now;
        }
        if let Some(rtt) = sample {
            let rtt = rtt as f64;
            match self.srtt_ns {
                None => {
                    self.srtt_ns = Some(rtt);
                    self.rttvar_ns = rtt / 2.0;
                }
                Some(s) => {
                    self.rttvar_ns = 0.75 * self.rttvar_ns + 0.25 * (s - rtt).abs();
                    self.srtt_ns = Some(0.875 * s + 0.125 * rtt);
                }
            }
            let rto = self.srtt_ns.unwrap() + (4.0 * self.rttvar_ns).max(5.0 * MS as f64);
            self.rto_ns = (rto as u64).clamp(MIN_RTO_NS, MAX_RTO_NS);
        }
    }
}

/// What a receiver reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    /// The first block not received yet (`total` when everything has).
    pub next: u32,
    /// Bit `n` set: block `next + 1 + n` has arrived.
    pub sack: u64,
}

/// Makes the sink for a transfer once its description is known.
pub type Factory = std::sync::Arc<dyn Fn(&Meta) -> io::Result<Box<dyn Sink>> + Send + Sync>;

pub struct Receiver {
    max_size: u64,
    factory: Factory,
    meta: Option<Meta>,
    sink: Option<Box<dyn Sink>>,
    total: u32,
    have: Vec<u64>,
    /// First block not received.
    cum: u32,
    count: u32,
    early: BTreeMap<u32, Vec<u8>>,
    unacked: u32,
    last_ack_ns: u64,
    done: bool,
}

impl Receiver {
    /// `max_size` is the largest transfer accepted; `factory` makes the sink once
    /// the transfer's description arrives.
    pub fn new(max_size: u64, factory: Factory) -> Receiver {
        Receiver {
            max_size,
            factory,
            meta: None,
            sink: None,
            total: 0,
            have: Vec::new(),
            cum: 0,
            count: 0,
            early: BTreeMap::new(),
            unacked: 0,
            last_ack_ns: 0,
            done: false,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn meta(&self) -> Option<&Meta> {
        self.meta.as_ref()
    }

    /// Bytes received so far, and in all (once known).
    pub fn progress(&self) -> (u64, u64) {
        let size = self.meta.as_ref().map_or(0, |m| m.size);
        ((self.count.saturating_sub(1) as u64 * BLOCK as u64).min(size), size)
    }

    fn has(&self, i: u32) -> bool {
        self.have.get(i as usize / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    fn mark(&mut self, i: u32) -> bool {
        let w = i as usize / 64;
        if w >= self.have.len() || self.has(i) {
            return false;
        }
        self.have[w] |= 1 << (i % 64);
        self.count += 1;
        while self.cum < self.total && self.has(self.cum) {
            self.cum += 1;
        }
        true
    }

    /// Takes a block. Returns the transfer's description once the last block is in.
    /// An error means the transfer can't go on (too big, can't write): drop it.
    pub fn on_block(&mut self, now: u64, index: u32, payload: &[u8]) -> io::Result<Option<Meta>> {
        self.unacked += 1;
        if self.done {
            return Ok(None);
        }
        if index == 0 {
            if self.meta.is_none() {
                self.start(payload)?;
                // Data that got here first.
                for (i, data) in std::mem::take(&mut self.early) {
                    self.store(i, &data)?;
                }
            }
            self.mark(0);
        } else if self.meta.is_none() {
            if self.early.len() < MAX_EARLY {
                self.early.insert(index, payload.to_vec());
            }
        } else {
            self.store(index, payload)?;
        }
        let _ = now;
        if self.meta.is_some() && self.count == self.total {
            self.done = true;
            if let Some(sink) = self.sink.take() {
                sink.finish()?;
            }
            return Ok(self.meta.clone());
        }
        Ok(None)
    }

    fn start(&mut self, payload: &[u8]) -> io::Result<()> {
        let meta = Meta::decode(payload).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad transfer description"))?;
        if meta.size > self.max_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} bytes is over the {} byte limit", meta.size, self.max_size)));
        }
        self.total = meta.blocks();
        self.have = vec![0; (self.total as usize).div_ceil(64)];
        self.sink = Some((self.factory)(&meta)?);
        self.meta = Some(meta);
        Ok(())
    }

    fn store(&mut self, index: u32, payload: &[u8]) -> io::Result<()> {
        let Some(meta) = &self.meta else { return Ok(()) };
        if index >= self.total || self.has(index) {
            return Ok(());
        }
        let offset = (index as u64 - 1) * BLOCK as u64;
        if payload.len() as u64 != (meta.size - offset).min(BLOCK as u64) {
            return Ok(()); // damaged or confused: ignore, the sender will resend
        }
        if let Some(sink) = &mut self.sink {
            sink.write_at(offset, payload)?;
        }
        self.mark(index);
        Ok(())
    }

    /// Whether an acknowledgement should go out now.
    pub fn wants_ack(&self, now: u64) -> bool {
        self.unacked >= ACK_EVERY || (self.unacked > 0 && now.saturating_sub(self.last_ack_ns) >= ACK_AFTER_NS)
    }

    /// What to acknowledge; call when sending one.
    pub fn ack(&mut self, now: u64) -> Ack {
        self.unacked = 0;
        self.last_ack_ns = now;
        let mut sack = 0u64;
        for bit in 0..64u32 {
            if self.has(self.cum.saturating_add(1).saturating_add(bit)) {
                sack |= 1 << bit;
            }
        }
        Ack { next: self.cum, sack }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn chance(&mut self, p: f64) -> bool {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
        }
    }

    #[derive(Default)]
    struct Mem {
        data: Vec<u8>,
        finished: bool,
        aborted: bool,
    }

    struct MemSink(Arc<Mutex<Mem>>);
    impl Sink for MemSink {
        fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
            let mut m = self.0.lock().unwrap();
            let end = offset as usize + data.len();
            if m.data.len() < end {
                m.data.resize(end, 0);
            }
            m.data[offset as usize..end].copy_from_slice(data);
            Ok(())
        }
        fn finish(self: Box<Self>) -> io::Result<()> {
            self.0.lock().unwrap().finished = true;
            Ok(())
        }
        fn abort(self: Box<Self>) {
            self.0.lock().unwrap().aborted = true;
        }
    }

    fn receiver(max: u64) -> (Receiver, Arc<Mutex<Mem>>) {
        let mem = Arc::new(Mutex::new(Mem::default()));
        let m = mem.clone();
        (Receiver::new(max, std::sync::Arc::new(move |_| Ok(Box::new(MemSink(m.clone())) as Box<dyn Sink>))), mem)
    }

    /// Runs a transfer over a simulated link; returns the milliseconds it took.
    fn simulate(data: Vec<u8>, rate: u64, one_way_ms: u64, loss: f64, seed: u64) -> (u64, Vec<u8>, bool) {
        let mut rng = Rng(seed);
        let meta = Meta { kind: Kind::File, size: data.len() as u64, name: "x".into() };
        let mut sender = Sender::new(meta, Bytes(data), rate);
        let (mut rx, mem) = receiver(u64::MAX);
        // (arrives_at_ms, block index, payload) and (arrives_at_ms, ack)
        let mut to_rx: VecDeque<(u64, u32, Vec<u8>)> = VecDeque::new();
        let mut to_tx: VecDeque<(u64, Ack)> = VecDeque::new();
        let mut ms = 0u64;
        while ms < 600_000 {
            let now = (ms + 1) * MS;
            let mut out = Vec::new();
            sender.poll(now, &mut |i, p| out.push((i, p.to_vec()))).unwrap();
            for (i, p) in out {
                if !rng.chance(loss) {
                    // Some jitter, so blocks arrive out of order.
                    to_rx.push_back((ms + one_way_ms + rng.next() % 4, i, p));
                }
            }
            to_rx.make_contiguous().sort_by_key(|e| e.0);
            while to_rx.front().is_some_and(|e| e.0 <= ms) {
                let (_, i, p) = to_rx.pop_front().unwrap();
                rx.on_block(now, i, &p).unwrap();
            }
            if rx.wants_ack(now) || (rx.is_done() && rx.unacked > 0) {
                let ack = rx.ack(now);
                if !rng.chance(loss) {
                    to_tx.push_back((ms + one_way_ms, ack));
                }
            }
            while to_tx.front().is_some_and(|e| e.0 <= ms) {
                let (_, a) = to_tx.pop_front().unwrap();
                sender.on_ack(now, a.next, a.sack);
            }
            if sender.is_done() && rx.is_done() {
                break;
            }
            ms += 1;
        }
        let m = mem.lock().unwrap();
        (ms, m.data.clone(), m.finished)
    }

    fn sample(len: usize, seed: u64) -> Vec<u8> {
        let mut r = Rng(seed | 1);
        (0..len).map(|_| r.next() as u8).collect()
    }

    #[test]
    fn meta_roundtrip() {
        let m = Meta { kind: Kind::File, size: 123_456_789_012, name: "résumé – final.pdf".into() };
        assert_eq!(Meta::decode(&m.encode()), Some(m));
        assert_eq!(Meta::decode(&[9, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(Meta::decode(&[0, 1, 2]), None);
        let long = Meta { kind: Kind::File, size: 1, name: "é".repeat(300) };
        assert!(Meta::decode(&long.encode()).unwrap().name.len() <= MAX_NAME);
        assert_eq!(Meta { kind: Kind::Clipboard, size: 0, name: String::new() }.blocks(), 1);
        assert_eq!(Meta { kind: Kind::File, size: BLOCK as u64 + 1, name: String::new() }.blocks(), 3);
    }

    #[test]
    fn clean_link_is_fast_and_exact() {
        let data = sample(2_000_000, 1);
        let (ms, got, finished) = simulate(data.clone(), 50_000_000, 5, 0.0, 7);
        assert!(finished && got == data);
        // 2 MB at 50 MB/s is 40 ms of sending; allow for the round trips.
        assert!(ms < 400, "took {ms} ms");
    }

    #[test]
    fn survives_heavy_loss_and_reordering() {
        for (loss, one_way, seed) in [(0.05, 6, 1), (0.2, 6, 2), (0.3, 25, 3)] {
            let data = sample(400_000 + seed as usize * 37, seed);
            let (ms, got, finished) = simulate(data.clone(), 20_000_000, one_way, loss, seed);
            assert!(finished && got == data, "loss {loss}: not delivered intact after {ms} ms");
        }
    }

    #[test]
    fn empty_and_tiny_transfers() {
        for len in [0usize, 1, BLOCK - 1, BLOCK, BLOCK + 1, 3 * BLOCK] {
            let data = sample(len, len as u64 + 1);
            let (_, got, finished) = simulate(data.clone(), 10_000_000, 4, 0.1, 9);
            assert!(finished && got == data, "len {len}");
        }
    }

    #[test]
    fn the_rate_cap_is_respected() {
        let data = sample(1_000_000, 4);
        // 1 MB at 2 MB/s can't finish much faster than half a second.
        let (ms, got, _) = simulate(data.clone(), 2_000_000, 2, 0.0, 5);
        assert_eq!(got, data);
        assert!((400..1500).contains(&ms), "took {ms} ms");
    }

    #[test]
    fn data_arriving_before_the_description_is_kept() {
        let (mut rx, mem) = receiver(u64::MAX);
        let data = sample(3 * BLOCK, 11);
        let meta = Meta { kind: Kind::File, size: data.len() as u64, name: "f".into() };
        for i in [3u32, 1, 2] {
            let off = (i as usize - 1) * BLOCK;
            assert!(rx.on_block(0, i, &data[off..off + BLOCK]).unwrap().is_none());
        }
        assert_eq!(rx.on_block(0, 0, &meta.encode()).unwrap(), Some(meta));
        assert_eq!(mem.lock().unwrap().data, data);
    }

    #[test]
    fn oversize_and_corrupt_transfers_are_refused() {
        let (mut rx, mem) = receiver(1000);
        let meta = Meta { kind: Kind::File, size: 5000, name: "big".into() };
        assert!(rx.on_block(0, 0, &meta.encode()).is_err());
        assert!(rx.meta().is_none() && mem.lock().unwrap().data.is_empty());
        let (mut rx, _) = receiver(u64::MAX);
        assert!(rx.on_block(0, 0, b"garbage").is_err());
        // A block of the wrong length is ignored, not stored.
        let (mut rx, mem) = receiver(u64::MAX);
        let meta = Meta { kind: Kind::File, size: 2000, name: "f".into() };
        rx.on_block(0, 0, &meta.encode()).unwrap();
        rx.on_block(0, 1, &[1, 2, 3]).unwrap();
        assert!(mem.lock().unwrap().data.is_empty());
        assert_eq!(rx.ack(0), Ack { next: 1, sack: 0 });
    }

    #[test]
    fn an_abandoned_transfer_is_aborted() {
        let (mut rx, mem) = receiver(u64::MAX);
        let meta = Meta { kind: Kind::File, size: 2000, name: "f".into() };
        rx.on_block(0, 0, &meta.encode()).unwrap();
        drop(rx);
        assert!(mem.lock().unwrap().aborted && !mem.lock().unwrap().finished);
    }

    #[test]
    fn a_dead_receiver_is_noticed() {
        let mut s = Sender::new(Meta { kind: Kind::File, size: 5000, name: "f".into() }, Bytes(vec![0; 5000]), 10_000_000);
        s.poll(MS, &mut |_, _| {}).unwrap();
        assert!(!s.is_stalled(MS + STALL_NS / 2));
        assert!(s.is_stalled(MS + STALL_NS + 1));
    }
}
