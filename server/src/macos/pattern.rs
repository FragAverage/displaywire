//! A moving test picture (bars sliding across a gradient) at the requested frame rate, encoded
//! like the screen would be.

use timing::{now_ns, sleep_until};
use videotoolbox::encode::{Codec as VtCodec, Encoder, PixelBuffer};

use crate::{Codec, EncodedFrame, Source};

pub struct Pattern {
    encoder: Encoder,
    fps: u32,
    bitrate: u32,
    codec: Codec,
    picture: PixelBuffer,
    size: (u32, u32),
    interval_ns: u64,
    next_ns: u64,
    start_ns: u64,
    n: u32,
}

impl Pattern {
    pub fn open(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Pattern, String> {
        let encoder = Encoder::new(width, height, fps, bitrate, VtCodec::Hevc)?;
        println!(
            "test pattern {width}x{height} at {fps} fps, HEVC by VideoToolbox{}",
            if encoder.low_latency { " (low-latency rate control)" } else { "" }
        );
        let now = now_ns();
        Ok(Pattern {
            encoder,
            fps,
            bitrate,
            codec: Codec::Hevc,
            picture: PixelBuffer::nv12(width, height)?,
            size: (width, height),
            interval_ns: 1_000_000_000 / fps.max(1) as u64,
            next_ns: now,
            start_ns: now,
            n: 0,
        })
    }
}

impl Source for Pattern {
    fn next_frame(&mut self, force_keyframe: bool, timeout_ms: u32) -> Result<Option<EncodedFrame>, String> {
        let now = now_ns();
        if self.next_ns > now + timeout_ms as u64 * 1_000_000 {
            sleep_until(now + timeout_ms as u64 * 1_000_000);
            return Ok(None);
        }
        sleep_until(self.next_ns);
        self.next_ns += self.interval_ns;
        let capture_ns = now_ns();
        let (w, h) = self.size;
        let t = self.n;
        self.n += 1;
        self.picture.fill(
            w,
            h,
            |x, y| if (x + t * 6) % 240 < 40 { 235 } else { (16 + (y * 200 / h)) as u8 },
            |x, y| ((x * 255 / (w / 2)) as u8, (y * 255 / (h / 2)) as u8),
        );
        let pts_us = ((capture_ns - self.start_ns) / 1000) as i64;
        let encoded = unsafe { self.encoder.encode(self.picture.as_ptr(), pts_us, force_keyframe || t == 0) }?;
        Ok(encoded.map(|e| super::frame(e, capture_ns)))
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.encoder.set_bitrate(bps);
    }

    fn set_format(&mut self, codec: Codec, max_size: Option<(u32, u32)>) -> Result<(), String> {
        // The picture is 1920x1080 already: it fits whatever the client allows (or it can't be made smaller).
        if max_size.is_some_and(|(w, h)| w < self.size.0 || h < self.size.1) {
            return Err("the test pattern is 1920x1080".into());
        }
        if codec != self.codec {
            self.encoder = Encoder::new(self.size.0, self.size.1, self.fps, self.bitrate, match codec {
                Codec::Hevc => VtCodec::Hevc,
                Codec::H264 => VtCodec::H264,
            })?;
            println!("test pattern: {codec:?}");
            self.codec = codec;
        }
        Ok(())
    }
}
