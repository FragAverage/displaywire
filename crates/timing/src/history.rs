//! Rolling per-bin history of stream health, for the on-screen graph.
//!
//! The receiver thread feeds a `Binner` per event (cheap, thread-local) and,
//! once per bin, pushes the finished `Bin` into a shared `History`; the UI
//! only ever touches the `History`, a few times a second.

use std::collections::VecDeque;

use crate::{Samples, ms};

/// One bin's worth of measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bin {
    /// Frames presented per second, extrapolated from this bin.
    pub fps: f32,
    pub mbps: f32,
    pub rtt_ms: f32,
    /// Capture → decoded, 0 until the clocks are synced.
    pub latency_p50_ms: f32,
    pub latency_p99_ms: f32,
    pub decode_ms: f32,
    /// Frames lost or dropped in this bin.
    pub lost: u32,
    /// Desktop audio: age on arrival plus what is queued for playback, and the target cushion. 0 without audio.
    pub audio_ms: f32,
    pub audio_cushion_ms: f32,
}

/// The last `capacity` bins, oldest first.
#[derive(Clone, Debug)]
pub struct History {
    bins: VecDeque<Bin>,
    capacity: usize,
}

impl History {
    pub fn new(capacity: usize) -> History {
        History { bins: VecDeque::with_capacity(capacity), capacity }
    }

    pub fn push(&mut self, bin: Bin) {
        if self.bins.len() == self.capacity {
            self.bins.pop_front();
        }
        self.bins.push_back(bin);
    }

    pub fn len(&self) -> usize {
        self.bins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bins.is_empty()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Bin> + ExactSizeIterator {
        self.bins.iter()
    }
}

/// Accumulates events and closes a `Bin` every `width_ns`.
pub struct Binner {
    width_ns: u64,
    start_ns: u64,
    frames: u32,
    bytes: u64,
    lost: u32,
    rtt_ns: Option<u64>,
    latency: Samples,
    decode: Samples,
    /// Latest audio delay and cushion, in ms.
    audio: (f32, f32),
}

impl Binner {
    pub fn new(now_ns: u64, width_ns: u64) -> Binner {
        Binner {
            width_ns,
            start_ns: now_ns,
            frames: 0,
            bytes: 0,
            lost: 0,
            rtt_ns: None,
            latency: Samples::default(),
            decode: Samples::default(),
            audio: (0.0, 0.0),
        }
    }

    /// A frame was decoded and shown. `latency_ns` is capture → decoded, once known.
    pub fn frame(&mut self, decode_ns: i64, latency_ns: Option<i64>) {
        self.frames += 1;
        self.decode.push(decode_ns);
        if let Some(l) = latency_ns {
            self.latency.push(l);
        }
    }

    pub fn bytes(&mut self, n: usize) {
        self.bytes += n as u64;
    }

    pub fn lost(&mut self, n: u32) {
        self.lost += n;
    }

    /// Latest audio delay and playback cushion; reported by the bin it was set in and kept after.
    pub fn audio(&mut self, delay_ms: f32, cushion_ms: f32) {
        self.audio = (delay_ms, cushion_ms);
    }

    /// Latest round-trip time; reported by the bin it was set in and kept after.
    pub fn rtt(&mut self, ns: u64) {
        self.rtt_ns = Some(ns);
    }

    /// Returns the finished bin once `width_ns` has elapsed, and starts the next.
    pub fn tick(&mut self, now_ns: u64) -> Option<Bin> {
        let elapsed = now_ns.saturating_sub(self.start_ns);
        if elapsed < self.width_ns {
            return None;
        }
        let secs = elapsed as f64 / 1e9;
        let (lat, dec) = (self.latency.summary(), self.decode.summary());
        let bin = Bin {
            fps: (self.frames as f64 / secs) as f32,
            mbps: (self.bytes as f64 * 8.0 / secs / 1e6) as f32,
            rtt_ms: self.rtt_ns.map_or(0.0, |r| ms(r as i64) as f32),
            latency_p50_ms: ms(lat.p50) as f32,
            latency_p99_ms: ms(lat.p99) as f32,
            decode_ms: ms(dec.p50) as f32,
            lost: self.lost,
            audio_ms: self.audio.0,
            audio_cushion_ms: self.audio.1,
        };
        self.start_ns = now_ns;
        self.frames = 0;
        self.bytes = 0;
        self.lost = 0;
        self.latency.clear();
        self.decode.clear();
        Some(bin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn closes_a_bin_per_width() {
        let mut b = Binner::new(0, 100 * MS);
        b.rtt(10 * MS);
        for _ in 0..12 {
            b.frame(2 * MS as i64, Some(10 * MS as i64));
            b.bytes(125_000);
        }
        b.lost(3);
        assert_eq!(b.tick(99 * MS), None);
        let bin = b.tick(100 * MS).unwrap();
        // 12 frames and 1.5 MB in 100 ms.
        assert_eq!((bin.fps, bin.mbps, bin.lost), (120.0, 120.0, 3));
        assert_eq!((bin.rtt_ms, bin.latency_p50_ms, bin.decode_ms), (10.0, 10.0, 2.0));
        // Counters reset, the RTT carries over.
        let next = b.tick(200 * MS).unwrap();
        assert_eq!((next.fps, next.mbps, next.rtt_ms, next.lost), (0.0, 0.0, 10.0, 0));
    }

    #[test]
    fn history_keeps_the_newest() {
        let mut h = History::new(3);
        for i in 0..5 {
            h.push(Bin { lost: i, ..Default::default() });
        }
        assert_eq!(h.iter().map(|b| b.lost).collect::<Vec<_>>(), [2, 3, 4]);
    }
}
