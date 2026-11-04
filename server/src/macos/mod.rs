//! The server on a Mac: the screen through ScreenCaptureKit (`capture`), encoded by VideoToolbox,
//! input through CoreGraphics events (`input`), system sound through ScreenCaptureKit (`audio`),
//! and the clipboard through AppKit (`shell`).

pub mod pattern;

use crate::{Cli, EncodedFrame, Source};

/// `--test-pattern`: a moving picture instead of the screen, through the real encoder (no
/// permissions needed), for trying the Mac server without Screen Recording.
pub fn open_pattern(cli: &Cli) -> Result<Box<dyn Source>, String> {
    Ok(Box::new(pattern::Pattern::open(1920, 1080, cli.fps, (cli.mbps * 1e6) as u32)?))
}

/// An encoded frame from VideoToolbox, as the stream loop wants it.
fn frame(e: videotoolbox::encode::Encoded, capture_ns: u64) -> EncodedFrame {
    EncodedFrame { data: e.data, keyframe: e.keyframe, capture_ns }
}
