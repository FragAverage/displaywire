//! Rate control from the client's `Feedback`: the video bitrate drops when the network struggles
//! and climbs back to the user's setting (the ceiling) when it is clean, and repair packets rise
//! with loss from the user's setting.
//!
//! The early warning is delay: when a queue builds somewhere on the path, frames take longer
//! to arrive than the best seen lately, before anything is lost. That, or loss, cuts the
//! bitrate by a share; a clean link adds back a slice of the ceiling every report. Wi-Fi's loss
//! comes in bursts, so loss is smoothed and repair packets stay raised for a while.

use std::collections::VecDeque;

use proto::stream::Feedback;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
/// The lowest delay over this long is the path's own; anything above it is queueing.
const BASELINE_WINDOW_NS: u64 = 10 * SEC;
/// Queueing this long (ms) means a queue is building: back off.
const QUEUE_HIGH_US: i64 = 25_000;
/// Below this the link has room: climb.
const QUEUE_LOW_US: i64 = 8_000;
/// Smoothed loss above which to back off, and below which the link counts as clean.
const LOSS_HIGH: f64 = 0.05;
const LOSS_CLEAN: f64 = 0.01;
/// Cuts are at least this far apart, so one burst isn't counted twice.
const CUT_EVERY_NS: u64 = 400 * MS;
/// Climbing waits this long after a cut.
const HOLD_AFTER_CUT_NS: u64 = SEC;
/// Each clean report adds this share of the ceiling (reports come every 50 ms: about 10 % a second).
const CLIMB_SHARE: f64 = 0.005;
/// Floor, in kbit/s.
const MIN_KBPS: u32 = 3_000;
const MAX_FEC_PERCENT: u8 = 30;

pub struct RateControl {
    adaptive: bool,
    ceiling_kbps: u32,
    base_fec: u8,
    target_kbps: f64,
    fec: u8,
    loss: f64,
    /// (when, delay) of recent reports, for the baseline.
    delays: VecDeque<(u64, i64)>,
    last_cut_ns: u64,
    /// The queue when delay last caused a cut: it takes a while to drain, so only a longer one
    /// cuts again (loss still does).
    queue_at_cut_us: i64,
}

impl RateControl {
    pub fn new(ceiling_kbps: u32, base_fec: u8, adaptive: bool) -> RateControl {
        RateControl {
            adaptive,
            ceiling_kbps,
            base_fec,
            target_kbps: ceiling_kbps as f64,
            fec: base_fec,
            loss: 0.0,
            delays: VecDeque::new(),
            last_cut_ns: 0,
            queue_at_cut_us: 0,
        }
    }

    /// The user's settings changed: a new ceiling, base repair level and mode. A higher ceiling is
    /// climbed to like any other; a lower one applies at once.
    pub fn set(&mut self, ceiling_kbps: u32, base_fec: u8, adaptive: bool) {
        (self.ceiling_kbps, self.base_fec, self.adaptive) = (ceiling_kbps, base_fec, adaptive);
        if !adaptive {
            self.target_kbps = ceiling_kbps as f64;
            self.fec = base_fec;
        }
        self.target_kbps = self.target_kbps.min(ceiling_kbps as f64);
        self.fec = self.fec.max(base_fec);
    }

    /// A new session: start again from the ceiling, forgetting the old path's delays and loss.
    pub fn reset(&mut self) {
        *self = RateControl::new(self.ceiling_kbps, self.base_fec, self.adaptive);
    }

    /// What to send at now: (kbit/s, repair %).
    pub fn current(&self) -> (u32, u8) {
        (self.target_kbps.round() as u32, self.fec)
    }

    pub fn on_feedback(&mut self, now_ns: u64, f: &Feedback) {
        if !self.adaptive || f.expected == 0 && f.frames_lost == 0 {
            return;
        }
        // A whole frame gone counts heavily: it is usually a burst that took several with it.
        let lost = f.expected.saturating_sub(f.received) as f64 + f.frames_lost as f64 * 10.0;
        let sent = f.expected as f64 + f.frames_lost as f64 * 10.0;
        let loss = (lost / sent.max(1.0)).min(1.0);
        self.loss = if loss > self.loss { 0.5 * self.loss + 0.5 * loss } else { 0.9 * self.loss + 0.1 * loss };

        let mut queue_us = 0;
        if f.delay_us >= 0 {
            self.delays.push_back((now_ns, f.delay_us as i64));
            while self.delays.front().is_some_and(|&(t, _)| now_ns - t > BASELINE_WINDOW_NS) {
                self.delays.pop_front();
            }
            let base = self.delays.iter().map(|&(_, d)| d).min().unwrap_or(0);
            queue_us = f.delay_us as i64 - base;
        }

        let ceiling = self.ceiling_kbps as f64;
        let floor = (MIN_KBPS as f64).min(ceiling);
        if queue_us < QUEUE_LOW_US {
            self.queue_at_cut_us = 0;
        }
        let queueing = queue_us > QUEUE_HIGH_US && queue_us > self.queue_at_cut_us;
        if (queueing || self.loss > LOSS_HIGH) && now_ns.saturating_sub(self.last_cut_ns) >= CUT_EVERY_NS {
            let factor = if self.loss > 2.0 * LOSS_HIGH { 0.7 } else { 0.85 };
            self.target_kbps = (self.target_kbps * factor).max(floor);
            self.last_cut_ns = now_ns;
            if queueing {
                self.queue_at_cut_us = queue_us;
            }
        } else if queue_us < QUEUE_LOW_US && self.loss < LOSS_CLEAN && now_ns.saturating_sub(self.last_cut_ns) >= HOLD_AFTER_CUT_NS {
            self.target_kbps = (self.target_kbps + ceiling * CLIMB_SHARE).min(ceiling);
        }

        // Repair packets follow loss: +3 % per 1 % lost, in steps of 5, back down as it clears.
        let wanted = self.base_fec as f64 + self.loss * 300.0;
        let stepped = ((wanted / 5.0).ceil() * 5.0) as u8;
        self.fec = stepped.clamp(self.base_fec, MAX_FEC_PERCENT.max(self.base_fec));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(delay_ms: i32, lost: u16) -> Feedback {
        Feedback { received: 1000 - lost, expected: 1000, frames_lost: 0, delay_us: delay_ms * 1000 }
    }

    /// Feeds `n` reports 50 ms apart from `*t`.
    fn run(rc: &mut RateControl, t: &mut u64, n: usize, f: Feedback) {
        for _ in 0..n {
            *t += 50 * MS;
            rc.on_feedback(*t, &f);
        }
    }

    #[test]
    fn a_clean_link_stays_at_the_ceiling() {
        let mut rc = RateControl::new(50_000, 10, true);
        let mut t = 0;
        run(&mut rc, &mut t, 200, report(10, 0));
        assert_eq!(rc.current(), (50_000, 10));
    }

    #[test]
    fn rising_delay_cuts_before_any_loss_then_climbs_back() {
        let mut rc = RateControl::new(50_000, 10, true);
        let mut t = 0;
        run(&mut rc, &mut t, 20, report(10, 0));
        // A queue builds: 40 ms over the 10 ms baseline. One cut while it drains...
        run(&mut rc, &mut t, 20, report(50, 0));
        assert_eq!(rc.current().0, 42_500);
        // ...and another only if it keeps growing.
        run(&mut rc, &mut t, 20, report(80, 0));
        let (cut, _) = rc.current();
        assert!(cut < 37_000, "cut to {cut}");
        // Clean again: back to the ceiling within ~10 s.
        run(&mut rc, &mut t, 220, report(10, 0));
        assert_eq!(rc.current().0, 50_000);
    }

    #[test]
    fn loss_cuts_and_raises_repair_packets_then_they_relax() {
        let mut rc = RateControl::new(50_000, 10, true);
        let mut t = 0;
        run(&mut rc, &mut t, 20, report(10, 80));
        let (kbps, fec) = rc.current();
        assert!(kbps < 40_000 && fec >= 25, "{kbps} kbit/s, {fec} %");
        run(&mut rc, &mut t, 400, report(10, 0));
        assert_eq!(rc.current(), (50_000, 10));
    }

    #[test]
    fn never_below_the_floor_nor_above_the_ceiling() {
        let mut rc = RateControl::new(50_000, 10, true);
        let mut t = 0;
        run(&mut rc, &mut t, 400, report(10, 500));
        assert_eq!(rc.current(), (MIN_KBPS, MAX_FEC_PERCENT));
        rc.set(2_000, 10, true);
        assert_eq!(rc.current().0, 2_000);
    }

    #[test]
    fn fixed_mode_ignores_the_network() {
        let mut rc = RateControl::new(50_000, 10, false);
        let mut t = 0;
        run(&mut rc, &mut t, 100, report(80, 300));
        assert_eq!(rc.current(), (50_000, 10));
    }
}
