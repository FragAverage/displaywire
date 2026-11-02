//! Sample-rate conversion for audio devices that can't run at the stream's 48 kHz, such as a
//! Bluetooth headset in hands-free mode (16 kHz mono while its mic is in use).
//!
//! Linear interpolation, with a low-pass at the lower of the two rates so that going down doesn't
//! alias and going up doesn't leave images. Plenty for the outputs that need it; devices that can
//! run at 48 kHz never go through here. Works on stereo frames; callers map channels.

pub type Frame = [f32; 2];

/// Two cascaded Butterworth sections: 24 dB per octave.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn low_pass(cutoff: f32, rate: f32) -> Biquad {
        let w = std::f32::consts::TAU * cutoff / rate;
        let alpha = w.sin() / std::f32::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        let b1 = (1.0 - w.cos()) / a0;
        Biquad { b: [b1 / 2.0, b1, b1 / 2.0], a: [-2.0 * w.cos() / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Where the next output frame falls, in input frames after `prev` (0) and towards the next one (1).
    pos: f64,
    prev: Frame,
    /// Per channel; on the input when going down, on the output when going up.
    filter: [[Biquad; 2]; 2],
    down: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        let (lower, down) = (from.min(to) as f32, to < from);
        let section = Biquad::low_pass(0.4 * lower, from.max(to) as f32);
        Resampler { step: from as f64 / to as f64, pos: 0.0, prev: [0.0; 2], filter: [[section; 2]; 2], down }
    }

    fn filter(&mut self, mut f: Frame) -> Frame {
        for (s, sections) in f.iter_mut().zip(&mut self.filter) {
            *s = sections.iter_mut().fold(*s, |s, b| b.run(s));
        }
        f
    }

    /// Takes one input frame and hands `out` every output frame that is due.
    pub fn push(&mut self, frame: Frame, mut out: impl FnMut(Frame)) {
        let cur = if self.down { self.filter(frame) } else { frame };
        while self.pos < 1.0 {
            let t = self.pos as f32;
            let f = [self.prev[0] + (cur[0] - self.prev[0]) * t, self.prev[1] + (cur[1] - self.prev[1]) * t];
            out(if self.down { f } else { self.filter(f) });
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = cur;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(from: u32, to: u32, input: impl Iterator<Item = Frame>) -> Vec<Frame> {
        let mut r = Resampler::new(from, to);
        let mut out = Vec::new();
        for f in input {
            r.push(f, |o| out.push(o));
        }
        out
    }

    fn sine(hz: f32, rate: u32, secs: f32) -> impl Iterator<Item = Frame> {
        (0..(rate as f32 * secs) as usize).map(move |i| [(std::f32::consts::TAU * hz * i as f32 / rate as f32).sin(); 2])
    }

    /// Root mean square of the second half, once the filter has settled.
    fn rms(frames: &[Frame]) -> f32 {
        let tail = &frames[frames.len() / 2..];
        (tail.iter().map(|f| f[0] * f[0]).sum::<f32>() / tail.len() as f32).sqrt()
    }

    #[test]
    fn keeps_time() {
        assert_eq!(run(48_000, 16_000, sine(0.0, 48_000, 1.0)).len(), 16_000);
        assert_eq!(run(16_000, 48_000, sine(0.0, 16_000, 1.0)).len(), 48_000);
        assert!(run(48_000, 44_100, sine(0.0, 48_000, 1.0)).len().abs_diff(44_100) <= 1);
    }

    #[test]
    fn passes_speech_and_stops_aliases() {
        let speech = rms(&run(48_000, 16_000, sine(1_000.0, 48_000, 0.5)));
        assert!((speech - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.05, "1 kHz came out at {speech}");
        // 15 kHz can't exist at 16 kHz: without the filter it would fold down to 1 kHz at full level.
        let alias = rms(&run(48_000, 16_000, sine(15_000.0, 48_000, 0.5)));
        assert!(alias < 0.05, "15 kHz leaked through at {alias}");
    }

    #[test]
    fn upsampling_passes_speech() {
        let speech = rms(&run(16_000, 48_000, sine(1_000.0, 16_000, 0.5)));
        assert!((speech - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.05, "1 kHz came out at {speech}");
    }
}
