//! Showing video on the TV: an SDL window for the app, and LG's decoder for the picture, through
//! LGNC (H.264; works in the Developer Mode jail everywhere) or NDL (needs a sane jail on k3lp/k5lp).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use clap::ValueEnum;
use transport::video::Frame;

use crate::lgnc::Lgnc;
use crate::ndl::Ndl;
use crate::screen::{Input, Screen};
use crate::session::VideoSink;

#[derive(Clone, Copy, ValueEnum)]
pub enum Backend {
    Lgnc,
    Ndl,
}

impl Backend {
    /// What to ask the server for: both of LG's decoders here only take H.264.
    pub fn caps(self) -> u8 {
        proto::stream::CAP_H264
    }
}

/// The app id LG's libraries are told: the launcher's `APPID` when started as an app, else ours.
fn app_id() -> String {
    std::env::var("APPID").unwrap_or_else(|_| "com.displaywire.tv".into())
}

enum Decoder {
    Lgnc(Lgnc),
    /// With the NDL source type to open with.
    Ndl(Ndl, i32),
}

impl Decoder {
    fn new(backend: Backend, source: i32) -> Result<Decoder, String> {
        Ok(match backend {
            Backend::Lgnc => Decoder::Lgnc(Lgnc::init(&app_id())?),
            Backend::Ndl => Decoder::Ndl(Ndl::init(&app_id())?, source),
        })
    }

    fn open(&mut self, width: i32, height: i32) -> Result<(), String> {
        match self {
            Decoder::Lgnc(l) => l.open_video(width, height),
            Decoder::Ndl(n, source) => n.open_video(width, height, *source),
        }
    }

    fn play(&self, au: &[u8]) -> Result<(), String> {
        match self {
            Decoder::Lgnc(l) => l.play(au),
            Decoder::Ndl(n, _) => n.play(au),
        }
    }

    fn set_foreground(&self) {
        if let Decoder::Ndl(n, _) = self
            && let Err(e) = n.set_foreground(true)
        {
            eprintln!("{e}");
        }
    }
}

/// Picture size from the SPS in an access unit (HEVC or H.264), if it has one.
fn sps_size(au: &[u8]) -> Option<(i32, i32)> {
    let nals = hevc::nals(au);
    let size = match nals.iter().find(|n| hevc::h264::nal_type(n) == hevc::h264::SPS) {
        Some(sps) => hevc::h264::sps_size(sps),
        None => hevc::sps_size(nals.into_iter().find(|n| hevc::nal_type(n) == hevc::SPS)?),
    };
    size.map(|(w, h)| (w as i32, h as i32))
}

/// `DW_VIDEO_SIZE=WxH`: the size to open the decoder at when the stream can't tell us (H.264).
fn size_override() -> Option<(i32, i32)> {
    let s = std::env::var("DW_VIDEO_SIZE").ok()?;
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

struct TvSink(Decoder);

impl VideoSink for TvSink {
    fn frame(&mut self, frame: &Frame) -> bool {
        if frame.keyframe
            && let Some((w, h)) = size_override().or_else(|| sps_size(&frame.data))
            && let Err(e) = self.0.open(w, h)
        {
            eprintln!("{e}");
            return false;
        }
        self.0.play(&frame.data).map_err(|e| eprintln!("{e}")).is_ok()
    }
}

/// Runs `stream` on another thread with a sink that shows video, while this thread keeps the
/// window up and watches the remote. Closing the app, or `stop`, ends both.
pub fn show(
    backend: Backend,
    source: i32,
    stop: Arc<AtomicBool>,
    stream: impl FnOnce(Box<dyn VideoSink + Send>, &AtomicBool) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    // LG's libraries first: LGNC sets up its own display connection and crashes in libwayland if
    // SDL's video is already up (moonlight-tv: "DO not init video subsystem before NDL/LGNC").
    let decoder = Decoder::new(backend, source)?;
    // DW_NO_WINDOW=1: another process (the app, in `window` mode) holds the window.
    let screen = if std::env::var_os("DW_NO_WINDOW").is_some() { None } else { Some(Screen::open()?) };
    // Let the window come up and the app become the foreground one before opening the decoder.
    for _ in 0..150 {
        let _ = screen.as_ref().and_then(Screen::poll);
        std::thread::sleep(Duration::from_millis(10));
    }
    decoder.set_foreground();
    let worker = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let r = stream(Box::new(TvSink(decoder)), &stop);
            stop.store(true, Relaxed);
            r
        })
    };
    while !stop.load(Relaxed) {
        match screen.as_ref().and_then(Screen::poll) {
            Some(Input::Quit) => stop.store(true, Relaxed),
            Some(Input::Key(code)) => println!("key {code}"),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    worker.join().unwrap_or_else(|_| Err("stream thread panicked".into()))
}

/// Plays an Annex B file (HEVC from `connect --dump`, or H.264) in a loop at `fps`, to try a
/// decoder without a PC.
pub fn play(path: &Path, backend: Backend, source: i32, fps: u32) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let aus = access_units(&data);
    println!("{}: {} access units", path.display(), aus.len());
    show(backend, source, Arc::default(), move |mut sink, stop| {
        let interval = Duration::from_secs(1) / fps.max(1);
        let mut shown = 0u64;
        while !stop.load(Relaxed) {
            for (n, au) in aus.iter().enumerate() {
                if stop.load(Relaxed) {
                    break;
                }
                let frame = Frame { id: n as u32, keyframe: n == 0, capture_ns: 0, complete_ns: 0, data: au.clone(), skipped: 0, repaired: 0 };
                if !sink.frame(&frame) && n == 0 {
                    return Err("the first frame was refused".into());
                }
                shown += 1;
                if shown % 300 == 0 {
                    println!("played {shown} frames");
                }
                std::thread::sleep(interval);
            }
        }
        Ok(())
    })
}

/// Just the app's transparent window, until the app is closed (see `DW_NO_WINDOW`).
pub fn window() -> Result<(), String> {
    let screen = Screen::open()?;
    loop {
        match screen.poll() {
            Some(Input::Quit) => return Ok(()),
            Some(Input::Key(code)) => println!("key {code}"),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Splits an Annex B stream without access unit delimiters (NVENC and x264 here send none): a new
/// access unit starts at a parameter set, SEI or delimiter, or at a picture's first slice, once
/// the current one has a slice. Works for HEVC and H.264, told apart by the first NAL.
fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let nals = hevc::nals(data);
    // HEVC NAL headers are two bytes with the type in bits 1..7; H.264's first NAL here is an SPS (7).
    let is_h264 = nals.first().is_some_and(|n| n[0] & 0x1f == 7);
    let mut out = Vec::new();
    let (mut au, mut has_slice) = (Vec::new(), false);
    for nal in nals {
        let (starts_au, is_slice) = if is_h264 {
            let t = nal[0] & 0x1f;
            let slice = t == 1 || t == 5;
            // first_mb_in_slice == 0 is a single 1 bit (Exp-Golomb) after the 1-byte header.
            ((6..=9).contains(&t) || slice && nal.get(1).is_some_and(|b| b & 0x80 != 0), slice)
        } else {
            let t = hevc::nal_type(nal);
            let slice = t < 32;
            ((hevc::VPS..=hevc::AUD).contains(&t) || slice && nal.get(2).is_some_and(|b| b & 0x80 != 0), slice)
        };
        if has_slice && starts_au {
            out.push(std::mem::take(&mut au));
            has_slice = false;
        }
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(nal);
        has_slice |= is_slice;
    }
    if has_slice {
        out.push(au);
    }
    out
}
