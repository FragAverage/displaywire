//! Splitting encoded frames into datagrams and putting them back together,
//! with Reed-Solomon repair packets so a few lost datagrams cost nothing.

use proto::stream::{Msg, VIDEO_PAYLOAD, Video};
use reed_solomon_simd as rs;

/// Parity is a `u8` in the header.
const MAX_PARITY: usize = 255;

/// Repair fragments for a frame of `data_frags` fragments at `percent` overhead:
/// at least one whenever FEC is on, so even a single-fragment frame is protected.
pub fn parity_count(data_frags: usize, percent: u32) -> usize {
    if percent == 0 { 0 } else { (data_frags * percent as usize).div_ceil(100).clamp(1, MAX_PARITY) }
}

/// Calls `send` with each datagram of the frame: the data fragments in order,
/// then `fec_percent`% (rounded up) repair fragments.
pub fn packetize(frame: u32, keyframe: bool, capture_ns: u64, data: &[u8], fec_percent: u32, mut send: impl FnMut(&[u8])) {
    let mut buf = [0u8; proto::MAX_DATAGRAM];
    let frags = data.len().div_ceil(VIDEO_PAYLOAD).max(1);
    assert!(frags <= u16::MAX as usize, "frame too large: {} bytes", data.len());
    let parity = parity_count(frags, fec_percent);
    let last_len = data.len() - (frags - 1) * VIDEO_PAYLOAD;
    let header = |frag: usize| Video { frame, frag: frag as u16, frags: frags as u16, parity: parity as u8, last_len: last_len as u16, keyframe, capture_ns };

    for i in 0..frags {
        let chunk = &data[(i * VIDEO_PAYLOAD).min(data.len())..((i + 1) * VIDEO_PAYLOAD).min(data.len())];
        let n = Msg::Video(header(i), chunk).encode(&mut buf);
        send(&buf[..n]);
    }
    if parity == 0 {
        return;
    }
    // The code works on equal-sized shards: pad the last data fragment with zeros.
    let mut padded = data[(frags - 1) * VIDEO_PAYLOAD..].to_vec();
    padded.resize(VIDEO_PAYLOAD, 0);
    let shards = (0..frags - 1).map(|i| &data[i * VIDEO_PAYLOAD..(i + 1) * VIDEO_PAYLOAD]).chain(std::iter::once(&padded[..]));
    let Ok(repair) = rs::encode(frags, parity, shards) else { return };
    for (j, shard) in repair.iter().enumerate() {
        let n = Msg::Video(header(frags + j), shard).encode(&mut buf);
        send(&buf[..n]);
    }
}

pub struct Frame {
    pub id: u32,
    pub keyframe: bool,
    pub capture_ns: u64,
    /// Local clock when the frame became complete.
    pub complete_ns: u64,
    pub data: Vec<u8>,
    /// Frames between the previous delivered frame and this one that will never arrive.
    pub skipped: u32,
    /// Data fragments that were lost and rebuilt from repair fragments.
    pub repaired: u32,
}

struct Partial {
    id: u32,
    keyframe: bool,
    capture_ns: u64,
    first_ns: u64,
    /// Data fragments, repair fragments, and bytes in the last data fragment.
    k: usize,
    m: usize,
    last_len: usize,
    /// Per fragment (data first, then repair): arrived?
    have: Vec<bool>,
    data_missing: usize,
    received: usize,
    /// One `VIDEO_PAYLOAD` slot per fragment, in the same order.
    buf: Vec<u8>,
}

impl Partial {
    /// Rebuilds the missing data fragments from the repair ones. Returns how many.
    fn repair(&mut self) -> Option<u32> {
        let shard = |i: usize| &self.buf[i * VIDEO_PAYLOAD..(i + 1) * VIDEO_PAYLOAD];
        let originals = (0..self.k).filter(|&i| self.have[i]).map(|i| (i, shard(i)));
        let recovery = (0..self.m).filter(|&j| self.have[self.k + j]).map(|j| (j, shard(self.k + j)));
        let restored = rs::decode(self.k, self.m, originals, recovery).ok()?;
        let n = restored.len() as u32;
        for (i, bytes) in restored {
            self.buf[i * VIDEO_PAYLOAD..(i + 1) * VIDEO_PAYLOAD].copy_from_slice(&bytes);
        }
        Some(n)
    }
}

/// Collects fragments into frames. Delivers frames in order; a frame that
/// completes after a newer one is dropped, as is anything still incomplete
/// when a newer frame completes.
#[derive(Default)]
pub struct Reassembler {
    partials: Vec<Partial>,
    delivered: Option<u32>,
}

const MAX_PARTIALS: usize = 8;
/// A frame id this far below the last delivered one means a new stream, not reordering.
const RESTART_GAP: u32 = 1024;

impl Reassembler {
    pub fn push(&mut self, v: Video, payload: &[u8], now_ns: u64) -> Option<Frame> {
        // Far behind what we've delivered: the server restarted and is counting from 0 again.
        if self.delivered.is_some_and(|d| v.frame.saturating_add(RESTART_GAP) < d) {
            *self = Reassembler::default();
        }
        if self.delivered.is_some_and(|d| v.frame <= d) {
            return None;
        }
        let (k, m, last_len) = (v.frags as usize, v.parity as usize, v.last_len as usize);
        let idx = match self.partials.iter().position(|p| p.id == v.frame) {
            Some(i) => i,
            None => {
                if self.partials.len() == MAX_PARTIALS {
                    let oldest = (0..self.partials.len()).min_by_key(|&i| self.partials[i].id).unwrap();
                    self.partials.swap_remove(oldest);
                }
                self.partials.push(Partial {
                    id: v.frame,
                    keyframe: v.keyframe,
                    capture_ns: v.capture_ns,
                    first_ns: now_ns,
                    k,
                    m,
                    last_len,
                    have: vec![false; k + m],
                    data_missing: k,
                    received: 0,
                    buf: vec![0; (k + m) * VIDEO_PAYLOAD],
                });
                self.partials.len() - 1
            }
        };
        let p = &mut self.partials[idx];
        let frag = v.frag as usize;
        // Every fragment of a frame must agree on its shape, and have the right size.
        let expect = if frag + 1 == p.k { p.last_len } else { VIDEO_PAYLOAD };
        if (k, m, last_len) != (p.k, p.m, p.last_len) || frag >= p.k + p.m || p.have[frag] || payload.len() != expect {
            return None;
        }
        p.have[frag] = true;
        p.received += 1;
        p.buf[frag * VIDEO_PAYLOAD..frag * VIDEO_PAYLOAD + payload.len()].copy_from_slice(payload);
        if frag < p.k {
            p.data_missing -= 1;
        }
        let mut repaired = 0;
        if p.data_missing > 0 {
            // Enough fragments of any kind: rebuild what's missing.
            if p.m == 0 || p.received < p.k {
                return None;
            }
            repaired = p.repair()?;
        }

        let mut p = self.partials.swap_remove(idx);
        self.partials.retain(|o| o.id > p.id);
        let skipped = self.delivered.map_or(0, |d| p.id - d - 1);
        self.delivered = Some(p.id);
        p.buf.truncate((p.k - 1) * VIDEO_PAYLOAD + p.last_len);
        Some(Frame { id: p.id, keyframe: p.keyframe, capture_ns: p.capture_ns, complete_ns: now_ns, data: p.buf, skipped, repaired })
    }

/// Drops incomplete frames older than `max_age_ns`; returns true if any were dropped.
    pub fn expire(&mut self, now_ns: u64, max_age_ns: u64) -> bool {
        let before = self.partials.len();
        self.partials.retain(|p| now_ns.saturating_sub(p.first_ns) <= max_age_ns);
        self.partials.len() < before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(frame: u32, data: &[u8], fec: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        packetize(frame, frame == 0, 100 + frame as u64, data, fec, |p| out.push(p.to_vec()));
        out
    }

    fn push(r: &mut Reassembler, pkt: &[u8]) -> Option<Frame> {
        let Some(Msg::Video(v, payload)) = Msg::decode(pkt) else { panic!("not video") };
        r.push(v, payload, 0)
    }

    /// Deterministic pseudo-random numbers, so a failure reproduces.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn sample(len: usize, seed: u64) -> Vec<u8> {
        let mut r = Rng(seed | 1);
        (0..len).map(|_| r.next() as u8).collect()
    }

    #[test]
    fn server_restart_resets() {
        let mut r = Reassembler::default();
        let push_frame = |r: &mut Reassembler, frame: u32| packets(frame, &[1, 2, 3], 0).iter().filter_map(|p| push(r, p)).next();
        assert!(push_frame(&mut r, 5000).is_some());
        assert!(push_frame(&mut r, 4999).is_none());
        let f = push_frame(&mut r, 0).expect("frame from restarted server");
        assert_eq!(f.skipped, 0);
    }

    #[test]
    fn reassembles_out_of_order() {
        let data = sample(VIDEO_PAYLOAD * 3 + 17, 1);
        for fec in [0, 10] {
            let mut pk = packets(0, &data, fec);
            pk.reverse();
            let mut r = Reassembler::default();
            let f = pk.iter().filter_map(|p| push(&mut r, p)).next().expect("frame");
            assert_eq!(f.data, data);
            assert!(f.keyframe);
            assert_eq!((f.id, f.capture_ns, f.skipped), (0, 100, 0));
        }
    }

    #[test]
    fn exact_multiple_and_empty() {
        for fec in [0, 10] {
            for len in [0, 1, VIDEO_PAYLOAD - 1, VIDEO_PAYLOAD, VIDEO_PAYLOAD + 1, VIDEO_PAYLOAD * 2] {
                let data = sample(len, len as u64);
                let mut r = Reassembler::default();
                let f = packets(0, &data, fec).iter().filter_map(|p| push(&mut r, p)).next().expect("frame");
                assert_eq!(f.data, data, "len {len} fec {fec}");
            }
        }
    }

    #[test]
    fn parity_counts() {
        assert_eq!(parity_count(40, 0), 0);
        assert_eq!(parity_count(1, 10), 1);
        assert_eq!(parity_count(10, 10), 1);
        assert_eq!(parity_count(11, 10), 2);
        assert_eq!(parity_count(500, 10), 50);
        assert_eq!(parity_count(5000, 10), 255);
    }

    #[test]
    fn data_only_delivery_does_not_wait_for_parity() {
        let data = sample(VIDEO_PAYLOAD * 5, 2);
        let pk = packets(0, &data, 20);
        let frags = 5;
        assert_eq!(pk.len(), frags + parity_count(frags, 20));
        let mut r = Reassembler::default();
        let f = pk[..frags].iter().filter_map(|p| push(&mut r, p)).next().expect("complete before any parity arrives");
        assert_eq!((f.data, f.repaired), (data, 0));
    }

    #[test]
    fn any_loss_up_to_the_parity_count_is_repaired() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for trial in 0..300u64 {
            let len = (rng.next() % (VIDEO_PAYLOAD as u64 * 60)) as usize;
            let data = sample(len, trial + 7);
            let pk = packets(0, &data, 10);
            let frags = len.div_ceil(VIDEO_PAYLOAD).max(1);
            let parity = parity_count(frags, 10);
            assert_eq!(pk.len(), frags + parity);
            // Drop `parity` packets chosen at random from all of them.
            let mut order: Vec<usize> = (0..pk.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, (rng.next() % (i as u64 + 1)) as usize);
            }
            let dropped: Vec<usize> = order[..parity].to_vec();
            let lost_data = dropped.iter().filter(|&&i| i < frags).count() as u32;
            let mut r = Reassembler::default();
            let f = pk
                .iter()
                .enumerate()
                .filter(|(i, _)| !dropped.contains(i))
                .filter_map(|(_, p)| push(&mut r, p))
                .next()
                .unwrap_or_else(|| panic!("trial {trial}: {len} bytes, dropped {dropped:?} of {}", pk.len()));
            assert_eq!(f.data, data, "trial {trial}");
            assert_eq!(f.repaired, lost_data, "trial {trial}");
        }
    }

    #[test]
    fn more_loss_than_parity_is_never_delivered_wrong() {
        let data = sample(VIDEO_PAYLOAD * 10, 3);
        let pk = packets(0, &data, 10); // 10 data + 1 repair
        let mut r = Reassembler::default();
        // Lose two data fragments: one repair packet can't fix that.
        assert!(pk.iter().enumerate().filter(|(i, _)| *i != 2 && *i != 7).filter_map(|(_, p)| push(&mut r, p)).next().is_none());
    }

    #[test]
    fn reports_skipped_and_drops_stale() {
        let data = vec![1u8; VIDEO_PAYLOAD * 2];
        let mut r = Reassembler::default();
        let f0: Vec<_> = packets(0, &data, 0).iter().filter_map(|p| push(&mut r, p)).collect();
        assert_eq!(f0.len(), 1);
        // Frame 1 loses a fragment; frame 2 arrives whole.
        let p1 = packets(1, &data, 0);
        assert!(push(&mut r, &p1[0]).is_none());
        let f2: Vec<_> = packets(2, &data, 0).iter().filter_map(|p| push(&mut r, p)).collect();
        assert_eq!(f2[0].skipped, 1);
        // The missing fragment of frame 1 arrives late and is ignored.
        assert!(push(&mut r, &p1[1]).is_none());
    }
}
