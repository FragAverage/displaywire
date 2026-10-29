//! Splitting encoded frames into datagrams and putting them back together.

use proto::stream::{Msg, VIDEO_PAYLOAD, Video};

/// Calls `send` with each datagram of the frame, in order.
pub fn packetize(frame: u32, keyframe: bool, capture_ns: u64, data: &[u8], mut send: impl FnMut(&[u8])) {
    let mut buf = [0u8; proto::MAX_DATAGRAM];
    let frags = data.len().div_ceil(VIDEO_PAYLOAD).max(1);
    assert!(frags <= u16::MAX as usize, "frame too large: {} bytes", data.len());
    for (i, chunk) in data.chunks(VIDEO_PAYLOAD).chain(data.is_empty().then_some(&[][..])).enumerate() {
        let v = Video { frame, frag: i as u16, frags: frags as u16, keyframe, capture_ns };
        let n = Msg::Video(v, chunk).encode(&mut buf);
        send(&buf[..n]);
    }
}

pub struct Frame {
    pub id: u32,
    pub keyframe: bool,
    pub capture_ns: u64,
    /// Local clock when the last fragment arrived.
    pub complete_ns: u64,
    pub data: Vec<u8>,
    /// Frames between the previous delivered frame and this one that will never arrive.
    pub skipped: u32,
}

struct Partial {
    id: u32,
    keyframe: bool,
    capture_ns: u64,
    first_ns: u64,
    have: Vec<bool>,
    missing: usize,
    data: Vec<u8>,
    len: usize,
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
        let idx = match self.partials.iter().position(|p| p.id == v.frame) {
            Some(i) => i,
            None => {
                if self.partials.len() == MAX_PARTIALS {
                    let oldest = (0..self.partials.len()).min_by_key(|&i| self.partials[i].id).unwrap();
                    self.partials.swap_remove(oldest);
                }
                let frags = v.frags as usize;
                self.partials.push(Partial {
                    id: v.frame,
                    keyframe: v.keyframe,
                    capture_ns: v.capture_ns,
                    first_ns: now_ns,
                    have: vec![false; frags],
                    missing: frags,
                    data: vec![0; frags * VIDEO_PAYLOAD],
                    len: (frags - 1) * VIDEO_PAYLOAD,
                });
                self.partials.len() - 1
            }
        };
        let p = &mut self.partials[idx];
        let frag = v.frag as usize;
        if frag >= p.have.len() || p.have[frag] || payload.len() > VIDEO_PAYLOAD {
            return None;
        }
        if frag + 1 < p.have.len() && payload.len() != VIDEO_PAYLOAD {
            return None;
        }
        p.have[frag] = true;
        p.missing -= 1;
        let off = frag * VIDEO_PAYLOAD;
        p.data[off..off + payload.len()].copy_from_slice(payload);
        if frag + 1 == p.have.len() {
            p.len = off + payload.len();
        }
        if p.missing > 0 {
            return None;
        }

        let mut p = self.partials.swap_remove(idx);
        self.partials.retain(|o| o.id > p.id);
        let skipped = self.delivered.map_or(0, |d| p.id - d - 1);
        self.delivered = Some(p.id);
        p.data.truncate(p.len);
        Some(Frame { id: p.id, keyframe: p.keyframe, capture_ns: p.capture_ns, complete_ns: now_ns, data: p.data, skipped })
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

    #[test]
    fn server_restart_resets() {
        let mut r = Reassembler::default();
        let push = |r: &mut Reassembler, frame: u32| {
            let pkts = packets(frame, &[1, 2, 3]);
            let Some(Msg::Video(v, payload)) = Msg::decode(&pkts[0]) else { panic!() };
            r.push(v, payload, 0)
        };
        assert!(push(&mut r, 5000).is_some());
        assert!(push(&mut r, 4999).is_none());
        let f = push(&mut r, 0).expect("frame from restarted server");
        assert_eq!(f.skipped, 0);
    }

    fn packets(frame: u32, data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        packetize(frame, frame == 0, 100 + frame as u64, data, |p| out.push(p.to_vec()));
        out
    }

    fn push(r: &mut Reassembler, pkt: &[u8]) -> Option<Frame> {
        let Some(Msg::Video(v, payload)) = Msg::decode(pkt) else { panic!("not video") };
        r.push(v, payload, 0)
    }

    #[test]
    fn reassembles_out_of_order() {
        let data: Vec<u8> = (0..VIDEO_PAYLOAD * 3 + 17).map(|i| i as u8).collect();
        let mut pk = packets(0, &data);
        pk.reverse();
        let mut r = Reassembler::default();
        let mut got = None;
        for p in &pk {
            got = push(&mut r, p).or(got);
        }
        let f = got.expect("frame");
        assert_eq!(f.data, data);
        assert!(f.keyframe);
        assert_eq!((f.id, f.capture_ns, f.skipped), (0, 100, 0));
    }

    #[test]
    fn exact_multiple_and_empty() {
        for len in [0, VIDEO_PAYLOAD, VIDEO_PAYLOAD * 2] {
            let data = vec![9u8; len];
            let mut r = Reassembler::default();
            let f = packets(0, &data).iter().filter_map(|p| push(&mut r, p)).next().expect("frame");
            assert_eq!(f.data.len(), len);
        }
    }

    #[test]
    fn reports_skipped_and_drops_stale() {
        let data = vec![1u8; VIDEO_PAYLOAD * 2];
        let mut r = Reassembler::default();
        let f0: Vec<_> = packets(0, &data).iter().filter_map(|p| push(&mut r, p)).collect();
        assert_eq!(f0.len(), 1);
        // Frame 1 loses a fragment; frame 2 arrives whole.
        let p1 = packets(1, &data);
        assert!(push(&mut r, &p1[0]).is_none());
        let f2: Vec<_> = packets(2, &data).iter().filter_map(|p| push(&mut r, p)).collect();
        assert_eq!(f2[0].skipped, 1);
        // The missing fragment of frame 1 arrives late and is ignored.
        assert!(push(&mut r, &p1[1]).is_none());
    }
}
