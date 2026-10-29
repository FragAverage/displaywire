//! Windows screen source: Desktop Duplication → video processor (NV12) →
//! hardware HEVC encoder.

mod capture;
mod convert;
mod encoder;
pub mod input;
pub mod service;

use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::w;

use crate::{Cli, EncodedFrame, Source};
use capture::Capture;
use convert::Converter;
use encoder::Encoder;

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// Names the step that failed, with the raw HRESULT, since Windows messages alone
/// ("not enough memory resources") rarely say where they came from.
pub trait Context<T> {
    fn ctx(self, what: &str) -> Res<T>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, what: &str) -> Res<T> {
        self.map_err(|e| format!("{what}: {} (0x{:08X})", e.message().trim(), e.code().0 as u32).into())
    }
}

pub struct ScreenSource {
    capture: Capture,
    converter: Converter,
    encoder: Encoder,
    /// Capture times of frames handed to the encoder but not yet returned.
    in_flight: std::collections::VecDeque<u64>,
    start_ns: u64,
    /// Pointer moved since the last encode; such frames are sent at most every `interval_ns`.
    pointer_dirty: bool,
    last_encode_ns: u64,
    interval_ns: u64,
}

impl ScreenSource {
    pub fn open(cli: &Cli) -> Result<ScreenSource, String> {
        Self::try_open(cli).map_err(|e| format!("screen capture: {e}"))
    }

    fn try_open(cli: &Cli) -> Res<ScreenSource> {
        unsafe {
            // Full-resolution capture on scaled displays.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            timeBeginPeriod(1);
            let mut task = 0;
            let _ = AvSetMmThreadCharacteristicsW(w!("Games"), &mut task);
        }
        let capture = Capture::new()?;
        // NV12 needs even dimensions.
        let (w, h) = (capture.width & !1, capture.height & !1);
        let converter =
            Converter::new(&capture.device, &capture.context, &capture.composed, (capture.width, capture.height), (w, h), cli.fps)?;
        let bitrate = (cli.mbps * 1e6) as u32;
        let encoder = Encoder::new(&capture.device, w, h, cli.fps, bitrate)?;
        println!("encoding {w}x{h} HEVC at {:.0} Mbps with {}", cli.mbps, encoder.name);
        Ok(ScreenSource {
            capture,
            converter,
            encoder,
            in_flight: Default::default(),
            start_ns: timing::now_ns(),
            pointer_dirty: false,
            last_encode_ns: 0,
            interval_ns: 1_000_000_000 / cli.fps.max(1) as u64,
        })
    }

    fn next(&mut self, force_keyframe: bool, timeout_ms: u32) -> Res<Option<EncodedFrame>> {
        let forced = force_keyframe && self.capture.has_frame;
        let pointer_due = self.last_encode_ns + self.interval_ns;
        let mut timeout = if forced { 0 } else { timeout_ms };
        if self.pointer_dirty {
            let ms = pointer_due.saturating_sub(timing::now_ns()).div_ceil(1_000_000);
            timeout = timeout.min(ms as u32);
        }
        let change = self.capture.next(timeout)?;
        self.pointer_dirty |= change.pointer;
        let capture_ns = timing::now_ns();
        let pointer_only = self.pointer_dirty && capture_ns >= pointer_due;
        if !self.capture.has_frame || !(change.desktop || forced || pointer_only) {
            return Ok(None);
        }
        self.pointer_dirty = false;
        self.last_encode_ns = capture_ns;
        self.capture.compose()?;
        let nv12 = self.converter.convert()?;
        let time_100ns = ((capture_ns - self.start_ns) / 100) as i64;
        self.in_flight.push_back(capture_ns);
        let Some(data) = self.encoder.encode(&nv12, time_100ns, force_keyframe)? else {
            return Ok(None);
        };
        let capture_ns = self.in_flight.pop_front().unwrap_or(capture_ns);
        let keyframe = hevc::is_keyframe(&data);
        Ok(Some(EncodedFrame { data, keyframe, capture_ns }))
    }
}

impl Source for ScreenSource {
    fn next_frame(&mut self, force_keyframe: bool, timeout_ms: u32) -> Result<Option<EncodedFrame>, String> {
        self.next(force_keyframe, timeout_ms).map_err(|e| e.to_string())
    }
}
