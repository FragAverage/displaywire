//! Showing video on the TV: an SDL window for the app, NDL for the picture.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use transport::video::Frame;

use crate::ndl::Ndl;
use crate::screen::{Input, Screen};
use crate::session::VideoSink;

/// The app id NDL is told: the launcher's `APPID` when started as an app, else ours.
fn app_id() -> String {
    std::env::var("APPID").unwrap_or_else(|_| "com.displaywire.tv".into())
}

/// Picture size from the SPS in an access unit, if it has one.
fn size_of(au: &[u8]) -> Option<(i32, i32)> {
    let sps = hevc::nals(au).into_iter().find(|n| hevc::nal_type(n) == hevc::SPS)?;
    hevc::sps_size(sps).map(|(w, h)| (w as i32, h as i32))
}

pub struct NdlSink {
    ndl: Ndl,
    source: i32,
}

impl VideoSink for NdlSink {
    fn frame(&mut self, frame: &Frame) -> bool {
        // DW_NDL_SIZE=WxH opens the decoder at that size instead (a debugging aid).
        let forced = std::env::var("DW_NDL_SIZE").ok().and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        });
        if frame.keyframe
            && let Some((w, h)) = forced.or_else(|| size_of(&frame.data))
            && let Err(e) = self.ndl.open_video(w, h, self.source)
        {
            eprintln!("{e}");
            return false;
        }
        self.ndl.play(&frame.data).map_err(|e| eprintln!("{e}")).is_ok()
    }
}

/// Runs `stream` on another thread with a sink that shows video, while this thread keeps the
/// window up and watches the remote. Closing the app, or `stop`, ends both.
pub fn show(
    source: i32,
    stop: Arc<AtomicBool>,
    stream: impl FnOnce(Box<dyn VideoSink + Send>, &AtomicBool) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let ndl = Ndl::init(&app_id())?;
    // DW_NO_WINDOW=1: another process (the app, in `window` mode) holds the window. A debugging
    // aid: on k3lp/k5lp TVs the Developer Mode app jail lacks /dev/rtkmem, which NDL needs.
    let screen = if std::env::var_os("DW_NO_WINDOW").is_some() { None } else { Some(Screen::open()?) };
    // Let the window come up and the app become the foreground one before asking for the decoder.
    for _ in 0..150 {
        let _ = screen.as_ref().and_then(Screen::poll);
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Err(e) = ndl.set_foreground(true) {
        eprintln!("{e}");
    }
    let worker = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let r = stream(Box::new(NdlSink { ndl, source }), &stop);
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

/// Plays a dumped stream (`connect --dump`) in a loop at `fps`, to try NDL without a PC.
pub fn play(path: &Path, source: i32, fps: u32) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let aus = access_units(&data);
    println!("{}: {} access units", path.display(), aus.len());
    show(source, Arc::default(), move |mut sink, stop| {
        let interval = Duration::from_secs(1) / fps.max(1);
        while !stop.load(Relaxed) {
            for (n, au) in aus.iter().enumerate() {
                if stop.load(Relaxed) {
                    break;
                }
                let frame = Frame { id: n as u32, keyframe: n == 0, capture_ns: 0, complete_ns: 0, data: au.clone(), skipped: 0, repaired: 0 };
                if !sink.frame(&frame) && n == 0 {
                    return Err("the first frame was refused".into());
                }
                std::thread::sleep(interval);
            }
        }
        Ok(())
    })
}

/// Splits an Annex B stream without access unit delimiters (NVENC sends none): a new access unit
/// starts at a parameter set or at a slice with `first_slice_segment_in_pic_flag`, once the
/// current one has a slice.
fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let (mut au, mut has_slice) = (Vec::new(), false);
    for nal in hevc::nals(data) {
        let t = hevc::nal_type(nal);
        let first_slice = t < 32 && nal.get(2).is_some_and(|b| b & 0x80 != 0);
        if has_slice && ((hevc::VPS..=hevc::AUD).contains(&t) || first_slice) {
            out.push(std::mem::take(&mut au));
            has_slice = false;
        }
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(nal);
        has_slice |= t < 32;
    }
    if has_slice {
        out.push(au);
    }
    out
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
