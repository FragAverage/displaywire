//! All the transfers of one side of the link, in both directions.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use crate::{Factory, Kind, Meta, Receiver, Sender, Sink, Source, STALL_NS};

const MS: u64 = 1_000_000;
/// A finished receiver is kept this long to answer the sender's repeats.
const LINGER_NS: u64 = 10_000 * MS;
/// A receiver that hears nothing for this long is dropped.
const IDLE_NS: u64 = 20_000 * MS;
/// Transfers received at once. With the per-file limit this bounds the disk and memory one peer
/// can tie up; further ones are refused until one finishes.
pub const MAX_CONCURRENT_RECEIVES: usize = 4;

/// Something to send to the other side.
#[derive(Debug, PartialEq, Eq)]
pub enum Outgoing {
    Block { id: u32, index: u32, payload: Vec<u8> },
    Ack { id: u32, next: u32, sack: u64 },
    /// Stop sending this transfer to us.
    Cancel { id: u32, reason: u8 },
}

/// `Outgoing::Cancel` reasons.
pub mod reason {
    pub const REFUSED: u8 = 1;
    pub const FAILED: u8 = 2;
    pub const USER: u8 = 3;
}

#[derive(Debug)]
pub enum Event {
    /// A transfer from the other side completed.
    Received { id: u32, meta: Meta },
    /// One of ours was fully acknowledged.
    Sent { id: u32, meta: Meta },
    SendFailed { id: u32, meta: Meta, why: String },
    ReceiveFailed { id: u32, why: String },
}

struct Incoming {
    receiver: Receiver,
    last_heard_ns: u64,
    done_ns: Option<u64>,
}

pub struct Endpoint {
    max_receive: u64,
    send_rate: u64,
    factory: Factory,
    next_id: u32,
    senders: HashMap<u32, Sender<Box<dyn Source + Send>>>,
    incoming: HashMap<u32, Incoming>,
    events: Vec<Event>,
}

impl Endpoint {
    /// `max_receive`: the largest transfer accepted. `send_rate`: bytes per second
    /// uploads are held to. `make_sink` creates where an incoming transfer goes.
    pub fn new(max_receive: u64, send_rate: u64, make_sink: impl Fn(&Meta) -> io::Result<Box<dyn Sink>> + Send + Sync + 'static) -> Endpoint {
        Endpoint { max_receive, send_rate, factory: Arc::new(make_sink), next_id: 1, senders: HashMap::new(), incoming: HashMap::new(), events: Vec::new() }
    }

    /// Starts sending; returns the transfer's id.
    pub fn send(&mut self, meta: Meta, source: Box<dyn Source + Send>) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.senders.insert(id, Sender::new(meta, source, self.send_rate));
        id
    }

    /// Stops sending a transfer (the other side is told when it next hears from us).
    pub fn cancel_send(&mut self, id: u32) {
        self.senders.remove(&id);
    }

    pub fn on_block(&mut self, now: u64, id: u32, index: u32, payload: &[u8]) -> Vec<Outgoing> {
        let receiving = self.incoming.values().filter(|i| i.done_ns.is_none()).count();
        if !self.incoming.contains_key(&id) && receiving >= MAX_CONCURRENT_RECEIVES {
            return vec![Outgoing::Cancel { id, reason: reason::REFUSED }];
        }
        let max = self.max_receive;
        let factory = self.factory.clone();
        let inc = self.incoming.entry(id).or_insert_with(|| Incoming { receiver: Receiver::new(max, factory), last_heard_ns: now, done_ns: None });
        inc.last_heard_ns = now;
        match inc.receiver.on_block(now, index, payload) {
            Ok(Some(meta)) => {
                inc.done_ns = Some(now);
                self.events.push(Event::Received { id, meta });
                Vec::new()
            }
            Ok(None) => Vec::new(),
            Err(e) => {
                let refused = e.kind() == io::ErrorKind::InvalidInput || e.kind() == io::ErrorKind::InvalidData;
                self.incoming.remove(&id);
                self.events.push(Event::ReceiveFailed { id, why: e.to_string() });
                vec![Outgoing::Cancel { id, reason: if refused { reason::REFUSED } else { reason::FAILED } }]
            }
        }
    }

    pub fn on_ack(&mut self, now: u64, id: u32, next: u32, sack: u64) {
        if let Some(s) = self.senders.get_mut(&id) {
            s.on_ack(now, next, sack);
        }
    }

    pub fn on_cancel(&mut self, id: u32, why: u8) {
        if let Some(s) = self.senders.remove(&id) {
            let why = match why {
                reason::REFUSED => "the other side refused it (too big, or not allowed)",
                reason::USER => "cancelled on the other side",
                _ => "the other side couldn't store it",
            };
            self.events.push(Event::SendFailed { id, meta: s.meta().clone(), why: why.into() });
        }
    }

    /// Everything due to be sent now, and what has happened since the last call.
    pub fn poll(&mut self, now: u64, out: &mut Vec<Outgoing>) -> Vec<Event> {
        let mut failed: Vec<(u32, String)> = Vec::new();
        for (&id, s) in &mut self.senders {
            if let Err(e) = s.poll(now, &mut |index, payload| out.push(Outgoing::Block { id, index, payload: payload.to_vec() })) {
                failed.push((id, e.to_string()));
            } else if s.is_stalled(now) {
                failed.push((id, "the other side stopped answering".into()));
            }
        }
        for (id, why) in failed {
            if let Some(s) = self.senders.remove(&id) {
                self.events.push(Event::SendFailed { id, meta: s.meta().clone(), why });
            }
        }
        let finished: Vec<u32> = self.senders.iter().filter(|(_, s)| s.is_done()).map(|(&i, _)| i).collect();
        for id in finished {
            if let Some(s) = self.senders.remove(&id) {
                self.events.push(Event::Sent { id, meta: s.meta().clone() });
            }
        }
        for (&id, inc) in &mut self.incoming {
            if inc.receiver.wants_ack(now) {
                let a = inc.receiver.ack(now);
                out.push(Outgoing::Ack { id, next: a.next, sack: a.sack });
            }
        }
        let mut gone = Vec::new();
        self.incoming.retain(|&id, inc| {
            let expired = match inc.done_ns {
                Some(t) => now.saturating_sub(t) > LINGER_NS,
                None => now.saturating_sub(inc.last_heard_ns) > IDLE_NS.max(STALL_NS),
            };
            if expired && inc.done_ns.is_none() {
                gone.push(id);
            }
            !expired
        });
        for id in gone {
            self.events.push(Event::ReceiveFailed { id, why: "the other side stopped sending".into() });
        }
        std::mem::take(&mut self.events)
    }

    /// Transfers going on now: (id, outgoing?, kind, bytes done, bytes in all, name).
    pub fn active(&self) -> Vec<(u32, bool, Kind, u64, u64, String)> {
        let mut v: Vec<_> = self
            .senders
            .iter()
            .map(|(&id, s)| {
                let (d, t) = s.progress();
                (id, true, s.meta().kind, d, t, s.meta().name.clone())
            })
            .collect();
        v.extend(self.incoming.iter().filter(|(_, i)| i.done_ns.is_none()).filter_map(|(&id, i)| {
            let m = i.receiver.meta()?;
            let (d, t) = i.receiver.progress();
            Some((id, false, m.kind, d, t, m.name.clone()))
        }));
        v
    }

    pub fn is_idle(&self) -> bool {
        self.senders.is_empty() && self.incoming.values().all(|i| i.done_ns.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Bytes;
    use std::collections::VecDeque;
    use std::sync::Mutex;

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

    type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    struct Collect(Store, String);
    impl Sink for Collect {
        fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
            let mut s = self.0.lock().unwrap();
            let v = s.entry(self.1.clone()).or_default();
            let end = offset as usize + data.len();
            if v.len() < end {
                v.resize(end, 0);
            }
            v[offset as usize..end].copy_from_slice(data);
            Ok(())
        }
        fn finish(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    fn endpoint(max: u64, store: &Store) -> Endpoint {
        let s = store.clone();
        Endpoint::new(max, 30_000_000, move |m| {
            let name = if m.kind == Kind::Clipboard { "<clipboard>".to_string() } else { m.name.clone() };
            s.lock().unwrap().insert(name.clone(), Vec::new());
            Ok(Box::new(Collect(s.clone(), name)) as Box<dyn Sink>)
        })
    }

    /// Hands one message to an endpoint; returns what it sends back.
    fn deliver(to: &mut Endpoint, now: u64, o: Outgoing) -> Vec<Outgoing> {
        match o {
            Outgoing::Block { id, index, payload } => to.on_block(now, id, index, &payload),
            Outgoing::Ack { id, next, sack } => {
                to.on_ack(now, id, next, sack);
                Vec::new()
            }
            Outgoing::Cancel { id, reason } => {
                to.on_cancel(id, reason);
                Vec::new()
            }
        }
    }

    /// Two endpoints joined by a lossy link; runs until both have nothing to do.
    fn run(a: &mut Endpoint, b: &mut Endpoint, loss: f64, seed: u64) -> (Vec<Event>, Vec<Event>) {
        let mut rng = Rng(seed);
        let mut a_to_b: VecDeque<(u64, Outgoing)> = VecDeque::new();
        let mut b_to_a: VecDeque<(u64, Outgoing)> = VecDeque::new();
        let (mut ea, mut eb) = (Vec::new(), Vec::new());
        for ms in 0..120_000u64 {
            let now = (ms + 1) * MS;
            let send = |q: &mut VecDeque<(u64, Outgoing)>, out: Vec<Outgoing>, rng: &mut Rng| {
                for o in out {
                    if !rng.chance(loss) {
                        q.push_back((ms + 5 + rng.next() % 3, o));
                    }
                }
            };
            let mut out = Vec::new();
            ea.extend(a.poll(now, &mut out));
            send(&mut a_to_b, out, &mut rng);
            let mut out = Vec::new();
            eb.extend(b.poll(now, &mut out));
            send(&mut b_to_a, out, &mut rng);
            a_to_b.make_contiguous().sort_by_key(|e| e.0);
            while a_to_b.front().is_some_and(|e| e.0 <= ms) {
                let (_, o) = a_to_b.pop_front().unwrap();
                let replies = deliver(b, now, o);
                send(&mut b_to_a, replies, &mut rng);
            }
            b_to_a.make_contiguous().sort_by_key(|e| e.0);
            while b_to_a.front().is_some_and(|e| e.0 <= ms) {
                let (_, o) = b_to_a.pop_front().unwrap();
                let replies = deliver(a, now, o);
                send(&mut a_to_b, replies, &mut rng);
            }
            if a.is_idle() && b.is_idle() && ms > 50 {
                break;
            }
        }
        (ea, eb)
    }

    #[test]
    fn clipboard_and_a_file_go_each_way_at_once_over_a_lossy_link() {
        let (sa, sb) = (Store::default(), Store::default());
        let (mut a, mut b) = (endpoint(10 << 20, &sa), endpoint(10 << 20, &sb));
        let mut rng = Rng(99);
        let file: Vec<u8> = (0..300_000).map(|_| rng.next() as u8).collect();
        a.send(Meta { kind: Kind::File, size: file.len() as u64, name: "report.pdf".into() }, Box::new(Bytes(file.clone())));
        let clip = "héllo\nwörld ✓".as_bytes().to_vec();
        a.send(Meta { kind: Kind::Clipboard, size: clip.len() as u64, name: String::new() }, Box::new(Bytes(clip.clone())));
        let back = b"from the other side".to_vec();
        b.send(Meta { kind: Kind::Clipboard, size: back.len() as u64, name: String::new() }, Box::new(Bytes(back.clone())));
        let (ea, eb) = run(&mut a, &mut b, 0.1, 5);
        assert_eq!(sb.lock().unwrap().get("report.pdf"), Some(&file));
        assert_eq!(sb.lock().unwrap().get("<clipboard>"), Some(&clip));
        assert_eq!(sa.lock().unwrap().get("<clipboard>"), Some(&back));
        assert_eq!(ea.iter().filter(|e| matches!(e, Event::Sent { .. })).count(), 2);
        assert_eq!(eb.iter().filter(|e| matches!(e, Event::Received { .. })).count(), 2);
        assert!(ea.iter().any(|e| matches!(e, Event::Received { .. })));
    }

    #[test]
    fn a_refused_transfer_fails_the_sender_instead_of_hanging() {
        let (sa, sb) = (Store::default(), Store::default());
        let (mut a, mut b) = (endpoint(1 << 20, &sa), endpoint(1000, &sb));
        a.send(Meta { kind: Kind::File, size: 5000, name: "big".into() }, Box::new(Bytes(vec![1; 5000])));
        // Deliver by hand: block 0 goes over, the refusal comes back.
        let mut out = Vec::new();
        a.poll(MS, &mut out);
        let mut replies = Vec::new();
        for o in out {
            if let Outgoing::Block { id, index, payload } = o {
                replies.extend(b.on_block(2 * MS, id, index, &payload));
            }
        }
        assert!(replies.iter().any(|r| matches!(r, Outgoing::Cancel { reason: reason::REFUSED, .. })));
        for r in replies {
            if let Outgoing::Cancel { id, reason } = r {
                a.on_cancel(id, reason);
            }
        }
        let events = a.poll(3 * MS, &mut Vec::new());
        assert!(events.iter().any(|e| matches!(e, Event::SendFailed { .. })));
        assert!(a.is_idle());
    }

    #[test]
    fn too_many_receives_at_once_are_refused() {
        let store = Store::default();
        let mut rx = endpoint(1 << 20, &store);
        for id in 1..=MAX_CONCURRENT_RECEIVES as u32 {
            assert!(rx.on_block(0, id, 1, &[0; 16]).is_empty(), "transfer {id} should be accepted");
        }
        let extra = MAX_CONCURRENT_RECEIVES as u32 + 1;
        let out = rx.on_block(0, extra, 1, &[0; 16]);
        assert!(matches!(out.as_slice(), [Outgoing::Cancel { id, reason: reason::REFUSED }] if *id == extra));
        // Ones already under way carry on.
        assert!(rx.on_block(0, 1, 2, &[0; 16]).is_empty());
    }
}
