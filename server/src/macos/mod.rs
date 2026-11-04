//! The server on a Mac: the screen through ScreenCaptureKit (`capture`), encoded by VideoToolbox,
//! input through CoreGraphics events (`input`), system sound through ScreenCaptureKit (`audio`),
//! the clipboard through AppKit (`shell`), the menu-bar icon (`menubar`) and starting at login
//! (`agent`).

pub mod agent;
pub mod audio;
mod capture;
pub mod cg;
pub mod input;
pub mod menubar;
pub mod pattern;
mod pointer;
pub mod shell;

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use objc2_foundation::NSString;
use proto::stream::{MAX_MONITORS, Monitor, Monitors};
use timing::now_ns;
use videotoolbox::encode::Encoder;

use crate::{Cli, EncodedFrame, PointerImage, PointerState, Shared, Source};

/// This Mac's name as Sharing shows it ("Jamie's MacBook Pro"), for discovery and pairing.
pub fn computer_name() -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// `open` until it works: the screen and its sound need the Screen Recording permission, which the
/// user may grant after the server has started. Says why once.
pub fn when_allowed<T>(what: &str, mut open: impl FnMut() -> Result<T, String>) -> T {
    let mut said = false;
    loop {
        match open() {
            Ok(t) => return t,
            Err(e) if !said => {
                println!("{what}: {e} (trying again every few seconds)");
                said = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: *const c_void, kind: u32, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

/// While a device is connected the display doesn't sleep (it's what the device is looking at), and
/// a connection wakes it.
pub fn keep_awake(shared: Arc<Shared>) {
    let spawned = std::thread::Builder::new().name("keep-awake".into()).spawn(move || {
        let kind = NSString::from_str("PreventUserIdleDisplaySleep");
        let name = NSString::from_str("DisplayWire: a device is connected");
        let (kind, name) = (&*kind as *const NSString as *const c_void, &*name as *const NSString as *const c_void);
        let mut held: Option<u32> = None;
        loop {
            let connected = shared.link.lock().unwrap().is_some();
            unsafe {
                match (connected, held) {
                    (true, None) => {
                        let mut wake = 0;
                        if IOPMAssertionDeclareUserActivity(name, 0, &mut wake) == 0 {
                            IOPMAssertionRelease(wake);
                        }
                        let mut id = 0;
                        if IOPMAssertionCreateWithName(kind, 255, name, &mut id) == 0 {
                            held = Some(id);
                        }
                    }
                    (false, Some(id)) => {
                        IOPMAssertionRelease(id);
                        held = None;
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    if let Err(e) = spawned {
        println!("keep-awake: {e}");
    }
}

/// `--test-pattern`: a moving picture instead of the screen, through the real encoder (no
/// permissions needed), for trying the Mac server without Screen Recording.
pub fn open_pattern(cli: &Cli) -> Result<Box<dyn Source>, String> {
    Ok(Box::new(pattern::Pattern::open(1920, 1080, cli.fps, (cli.mbps * 1e6) as u32)?))
}

/// An encoded frame from VideoToolbox, as the stream loop wants it.
fn frame(e: videotoolbox::encode::Encoded, capture_ns: u64) -> EncodedFrame {
    EncodedFrame { data: e.data, keyframe: e.keyframe, capture_ns }
}

/// A display, captured and encoded.
pub struct ScreenSource {
    /// Which stream this is (input finds its display by it).
    stream: u8,
    capture: capture::Capture,
    encoder: Encoder,
    fps: u32,
    bitrate: u32,
    start_ns: u64,
    pointer_in_video: bool,
    pointer: pointer::Pointer,
    /// Encode the last picture again (a keyframe asked for on a still screen, or a repaint).
    repaint: bool,
}

impl ScreenSource {
    /// Display `monitor` (its place in the list `monitors` gives), or the main one.
    pub fn open(cli: &Cli, stream: u8, monitor: Option<u8>) -> Result<ScreenSource, String> {
        let fps = cli.fps;
        let bitrate = (cli.mbps * 1e6) as u32;
        let (capture, encoder) = Self::open_display(monitor, fps, bitrate, true)?;
        input::set_stream_display(stream, Some(capture.display));
        Ok(ScreenSource { stream, capture, encoder, fps, bitrate, start_ns: now_ns(), pointer_in_video: true, pointer: Default::default(), repaint: false })
    }

    fn open_display(monitor: Option<u8>, fps: u32, bitrate: u32, with_pointer: bool) -> Result<(capture::Capture, Encoder), String> {
        let displays = capture::displays()?;
        let main = cg::main_display();
        let pick = monitor
            .and_then(|m| displays.iter().nth(m as usize))
            .or_else(|| displays.iter().find(|d| unsafe { d.displayID() } == main))
            .or_else(|| displays.iter().next())
            .ok_or("no display to capture")?;
        let capture = capture::Capture::new(&pick, fps, with_pointer)?;
        let encoder = Encoder::new(capture.width, capture.height, fps, bitrate)?;
        println!(
            "encoding {}x{} HEVC with VideoToolbox{}",
            capture.width,
            capture.height,
            if encoder.low_latency { " (low-latency rate control)" } else { "" }
        );
        Ok((capture, encoder))
    }

    fn encode(&mut self, picture: &capture::Picture, capture_ns: u64, keyframe: bool) -> Result<Option<EncodedFrame>, String> {
        let pts_us = (capture_ns.saturating_sub(self.start_ns) / 1000) as i64;
        let encoded = unsafe { self.encoder.encode(picture.as_ptr(), pts_us, keyframe) }?;
        Ok(encoded.map(|e| frame(e, capture_ns)))
    }
}

impl Source for ScreenSource {
    fn next_frame(&mut self, force_keyframe: bool, timeout_ms: u32) -> Result<Option<EncodedFrame>, String> {
        let wait = if force_keyframe || self.repaint { 0 } else { timeout_ms };
        let picture = match self.capture.next(wait) {
            Some(p) => Some(p),
            // Nothing new: the last picture again, if a keyframe or repaint is due.
            None if force_keyframe || self.repaint => self.capture.last().map(|(p, _)| (p, now_ns())),
            None => None,
        };
        let Some((picture, capture_ns)) = picture else { return Ok(None) };
        self.repaint = false;
        self.encode(&picture, capture_ns, force_keyframe)
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.encoder.set_bitrate(bps);
    }

    fn repaint(&mut self) {
        self.repaint = true;
    }

    fn set_pointer_in_video(&mut self, on: bool) {
        self.pointer_in_video = on;
        self.capture.set_pointer(on);
    }

    fn pointer(&self) -> Option<PointerState> {
        Some(self.pointer.state(self.capture.display))
    }

    fn pointer_image(&self) -> Option<PointerImage> {
        self.pointer.image()
    }

    fn monitors(&self) -> Option<Monitors> {
        let displays = capture::displays().ok()?;
        let main = cg::main_display();
        let mut m = Monitors::default();
        for (i, d) in displays.iter().take(MAX_MONITORS).enumerate() {
            let id = unsafe { d.displayID() };
            let (w, h, hz) = cg::pixel_size(id);
            let name = if cg::is_builtin(id) { "Built-in display".to_string() } else { format!("Display {}", i + 1) };
            m.list[i] = Monitor::new(w as u16, h as u16, hz as u16, id == main, &name);
            if id == self.capture.display {
                m.streaming = i as u8;
            }
            m.count += 1;
        }
        Some(m)
    }

    fn choose_monitor(&mut self, index: u8) -> Result<(), String> {
        let (capture, encoder) = Self::open_display(Some(index), self.fps, self.bitrate, self.pointer_in_video)?;
        (self.capture, self.encoder) = (capture, encoder);
        input::set_stream_display(self.stream, Some(self.capture.display));
        Ok(())
    }
}

impl Drop for ScreenSource {
    fn drop(&mut self) {
        if self.stream != 0 {
            input::set_stream_display(self.stream, None);
        }
    }
}
