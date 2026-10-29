//! Clocks, precise waits and latency statistics.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Monotonic nanoseconds since the first call in this process.
/// Not comparable across machines without clock sync.
#[inline]
pub fn now_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Waits until `deadline_ns` (on the `now_ns` clock) with ~µs precision:
/// OS sleep for the bulk, then yield, then spin.
pub fn sleep_until(deadline_ns: u64) {
    loop {
        let now = now_ns();
        if now >= deadline_ns {
            return;
        }
        let rem = deadline_ns - now;
        // OS sleeps can overshoot by ~1 ms on both macOS and Windows.
        if rem > 2_500_000 {
            std::thread::sleep(Duration::from_nanos(rem - 2_000_000));
        } else if rem > 50_000 {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Collected samples (e.g. nanoseconds) with percentile summaries.
#[derive(Default, Clone)]
pub struct Samples {
    v: Vec<i64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub count: usize,
    pub min: i64,
    pub p50: i64,
    pub p99: i64,
    pub p999: i64,
    pub max: i64,
}

impl Samples {
    pub fn push(&mut self, x: i64) {
        self.v.push(x);
    }

    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    pub fn clear(&mut self) {
        self.v.clear();
    }

    pub fn summary(&mut self) -> Summary {
        if self.v.is_empty() {
            return Summary::default();
        }
        self.v.sort_unstable();
        let at = |p: f64| self.v[((self.v.len() - 1) as f64 * p).round() as usize];
        Summary {
            count: self.v.len(),
            min: self.v[0],
            p50: at(0.50),
            p99: at(0.99),
            p999: at(0.999),
            max: *self.v.last().unwrap(),
        }
    }
}

/// Nanoseconds as milliseconds for display.
pub fn ms(ns: i64) -> f64 {
    ns as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let mut s = Samples::default();
        for i in (1..=1000).rev() {
            s.push(i);
        }
        let sum = s.summary();
        assert_eq!((sum.count, sum.min, sum.max), (1000, 1, 1000));
        assert_eq!(sum.p50, 501);
        assert_eq!(sum.p99, 990);
    }

    #[test]
    fn sleep_until_is_precise() {
        let target = now_ns() + 3_000_000;
        sleep_until(target);
        let late = now_ns() - target;
        assert!(late < 500_000, "woke {late} ns late");
    }
}
