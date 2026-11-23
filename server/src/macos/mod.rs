//! Mac screen source: ScreenCaptureKit → VideoToolbox HEVC, plus the Mac's side of everything
//! else the server does (system audio, input, clipboard, the microphone, staying awake, the
//! menu-bar item and starting at login). macOS 13 or newer.

mod capture;
mod cursor;
mod ffi;
pub mod agent;
pub mod input;
pub mod menubar;
pub mod micout;
pub mod shell;
pub mod virtual_display;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE, MAX_MONITORS, Monitor, Monitors};
use videotoolbox::Encoder;

use crate::audio::{AudioSource, CHUNK_FRAMES, CHUNK_SAMPLES};
use crate::{Cli, EncodedFrame, Shared, Source};
use capture::{AudioCapture, Capture};
use ffi::CGDirectDisplayID;

const SEC: u64 = 1_000_000_000;
/// The screen has been still this long: the last image is encoded once more, which sharpens what the
/// rate controller had to send coarsely while things were moving.
const REFINE_AFTER_NS: u64 = 250_000_000;

/// Which display each stream shows, for input (absolute positions are across the stream's display).
static STREAMS: Mutex<Option<HashMap<u8, CGDirectDisplayID>>> = Mutex::new(None);

fn set_stream(stream: u8, display: Option<CGDirectDisplayID>) {
    let mut s = STREAMS.lock().unwrap();
    let map = s.get_or_insert_with(HashMap::new);
    match display {
        Some(d) => map.insert(stream, d),
        None => map.remove(&stream),
    };
}

/// The display stream `stream` shows (the main display if it isn't known).
pub fn stream_display(stream: u8) -> CGDirectDisplayID {
    STREAMS.lock().unwrap().as_ref().and_then(|m| m.get(&stream).copied()).unwrap_or_else(|| unsafe { ffi::CGMainDisplayID() })
}

/// What the client lists: every active display, the main one first.
fn monitor_list() -> Vec<(CGDirectDisplayID, Monitor)> {
    let main = unsafe { ffi::CGMainDisplayID() };
    let mut external = 0;
    ffi::displays()
        .into_iter()
        .take(MAX_MONITORS)
        .filter_map(|d| {
            let (w, h, hz) = ffi::display_mode(d)?;
            let name = if unsafe { ffi::CGDisplayIsBuiltin(d) } != 0 {
                "Built-in Display".to_string()
            } else if virtual_display::current() == Some(d) {
                "Virtual Display".to_string()
            } else {
                external += 1;
                format!("Display {external}")
            };
            Some((d, Monitor::new(w.min(u16::MAX as u32) as u16, h.min(u16::MAX as u32) as u16, hz as u16, d == main, &name)))
        })
        .collect()
}

/// Whether this process may record the screen; asks (once, with the system's prompt) if not.
pub fn screen_recording_allowed() -> bool {
    unsafe { ffi::CGPreflightScreenCaptureAccess() || ffi::CGRequestScreenCaptureAccess() }
}

/// Whether this process may record the screen, without asking.
pub fn screen_recording_allowed_quiet() -> bool {
    unsafe { ffi::CGPreflightScreenCaptureAccess() }
}

/// Whether this process may control the Mac; with `prompt`, the system asks the user to allow it.
pub fn accessibility_allowed(prompt: bool) -> bool {
    unsafe {
        if !prompt {
            return ffi::AXIsProcessTrusted();
        }
        let options = ffi::Cf(ffi::CFDictionaryCreateMutable(
            std::ptr::null(),
            1,
            &ffi::kCFTypeDictionaryKeyCallBacks,
            &ffi::kCFTypeDictionaryValueCallBacks,
        ));
        ffi::CFDictionarySetValue(options.0, ffi::kAXTrustedCheckOptionPrompt, ffi::kCFBooleanTrue);
        ffi::AXIsProcessTrustedWithOptions(options.0)
    }
}

pub struct ScreenSource {
    capture: Capture,
    encoder: Encoder,
    stream: u8,
    fps: u32,
    bitrate: u32,
    start_ns: u64,
    repaint: bool,
    /// When the last frame went out, and whether a still screen has been sent again since (see `REFINE_AFTER_NS`).
    last_encode_ns: u64,
    refined: bool,
    mode_checked_ns: u64,
    cursor: cursor::Tracker,
    profile: Option<Profile>,
}

#[derive(Default)]
struct Profile {
    encode: timing::Samples,
    wait: timing::Samples,
    since_ns: u64,
}

impl ScreenSource {
    /// Stream `stream` of monitor `monitor` (its place in the list the client sees), or the main display.
    pub fn open(cli: &Cli, stream: u8, monitor: Option<u8>) -> Result<ScreenSource, String> {
        if !screen_recording_allowed() {
            return Err("not allowed to record the screen: allow DisplayWire Server (or the terminal running it) in System Settings › Privacy & Security › Screen & System Audio Recording, then restart it".into());
        }
        let display = match monitor {
            Some(m) => monitor_list().get(m as usize).map(|(d, _)| *d).ok_or("no such monitor")?,
            None => unsafe { ffi::CGMainDisplayID() },
        };
        let capture = Capture::new(display, cli.fps, true).map_err(|e| format!("screen capture: {e}"))?;
        let bitrate = (cli.mbps * 1e6) as u32;
        let encoder = Encoder::new(capture.width, capture.height, cli.fps, bitrate)?;
        println!("encoding {}x{} HEVC at {:.0} Mbps with {}", capture.width, capture.height, cli.mbps, encoder.name);
        set_stream(stream, Some(display));
        Ok(ScreenSource {
            capture,
            encoder,
            stream,
            fps: cli.fps,
            bitrate,
            start_ns: timing::now_ns(),
            repaint: false,
            last_encode_ns: 0,
            refined: true,
            mode_checked_ns: timing::now_ns(),
            cursor: cursor::Tracker::new(),
            profile: cli.profile.then(|| Profile { since_ns: timing::now_ns(), ..Default::default() }),
        })
    }

    fn rebuild_encoder(&mut self) -> Result<(), String> {
        self.encoder = Encoder::new(self.capture.width, self.capture.height, self.fps, self.bitrate)?;
        println!("encoding {}x{} HEVC with {}", self.capture.width, self.capture.height, self.encoder.name);
        Ok(())
    }
}

impl Drop for ScreenSource {
    fn drop(&mut self) {
        if self.stream != 0 {
            set_stream(self.stream, None);
        }
    }
}

impl Source for ScreenSource {
    fn next_frame(&mut self, force_keyframe: bool, timeout_ms: u32) -> Result<Option<EncodedFrame>, String> {
        let now = timing::now_ns();
        if now - self.mode_checked_ns > SEC {
            self.mode_checked_ns = now;
            // Its display went away (the virtual one, or one unplugged): show the main one.
            if !ffi::displays().contains(&self.capture.display) {
                let main = unsafe { ffi::CGMainDisplayID() };
                println!("display {} is gone; streaming the main display", self.capture.display);
                let cursor = self.cursor.in_video();
                self.capture = Capture::new(main, self.fps, cursor).map_err(|e| format!("capture display {main}: {e}"))?;
                set_stream(self.stream, Some(main));
                self.rebuild_encoder()?;
            }
            // A resolution change: the new encoder starts with a keyframe at the new size.
            if self.capture.follow_mode()? {
                self.rebuild_encoder()?;
            }
        }
        let forced = (force_keyframe || self.repaint) && self.capture.last.is_some();
        let waited = timing::now_ns();
        let mut timeout = if forced { 0 } else { timeout_ms };
        if !self.refined {
            let due = self.last_encode_ns + REFINE_AFTER_NS;
            timeout = timeout.min(due.saturating_sub(waited).div_ceil(1_000_000) as u32);
        }
        let frame = match self.capture.next(timeout) {
            Some(f) => {
                self.refined = false;
                f
            }
            None if forced => self.capture.again().ok_or("no frame")?,
            None if !self.refined && timing::now_ns() >= self.last_encode_ns + REFINE_AFTER_NS && self.capture.last.is_some() => {
                self.refined = true;
                self.capture.again().ok_or("no frame")?
            }
            None => return Ok(None),
        };
        let got = timing::now_ns();
        // Frames still on their way from before a size change.
        let size = unsafe { (ffi::CVPixelBufferGetWidth(frame.buffer.0) as u32, ffi::CVPixelBufferGetHeight(frame.buffer.0) as u32) };
        if size != (self.capture.width, self.capture.height) {
            return Ok(None);
        }
        self.repaint = false;
        self.last_encode_ns = got;
        // SAFETY: a retained pixel buffer from ScreenCaptureKit, of the encoder's size (checked above).
        let Some(out) = (unsafe { self.encoder.encode(frame.buffer.0, got - self.start_ns, force_keyframe) })? else {
            return Ok(None);
        };
        if let Some(p) = &mut self.profile {
            let done = timing::now_ns();
            p.wait.push((got - waited) as i64);
            p.encode.push((done - got) as i64);
            if done - p.since_ns > SEC {
                let line = |s: &mut timing::Samples| {
                    let m = s.summary();
                    format!("{:.2}/{:.2}", timing::ms(m.p50), timing::ms(m.p99))
                };
                println!("profile (ms p50/p99): waiting for a frame {}  encode {}", line(&mut p.wait), line(&mut p.encode));
                p.wait.clear();
                p.encode.clear();
                p.since_ns = done;
            }
        }
        if let Some(path) = std::env::var_os("DISPLAYWIRE_DUMP") {
            use std::io::Write;
            let _ = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(&out.data));
        }
        Ok(Some(EncodedFrame { data: out.data, keyframe: out.keyframe, capture_ns: frame.at_ns }))
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.encoder.set_bitrate(bps);
    }

    fn repaint(&mut self) {
        self.repaint = true;
    }

    fn monitors(&self) -> Option<Monitors> {
        let list = monitor_list();
        let mut m = Monitors { count: list.len() as u8, ..Default::default() };
        for (i, (d, mon)) in list.iter().enumerate() {
            m.list[i] = *mon;
            if *d == self.capture.display {
                m.streaming = i as u8;
            }
        }
        Some(m)
    }

    fn choose_monitor(&mut self, index: u8) -> Result<(), String> {
        let display = monitor_list().get(index as usize).map(|(d, _)| *d).ok_or("no such monitor")?;
        if display == self.capture.display {
            return Ok(());
        }
        let cursor = self.cursor.in_video();
        self.capture = Capture::new(display, self.fps, cursor).map_err(|e| format!("capture display {display}: {e}"))?;
        set_stream(self.stream, Some(display));
        self.rebuild_encoder()
    }

    fn set_pointer_in_video(&mut self, on: bool) {
        if self.cursor.in_video() != on {
            self.cursor.set_in_video(on);
            if let Err(e) = self.capture.set_cursor(on) {
                println!("capture: {e}");
            }
            // A still screen otherwise keeps showing (or lacking) the pointer.
            self.repaint = true;
        }
    }

    fn pointer(&self) -> Option<crate::PointerState> {
        self.cursor.state(self.capture.display)
    }

    fn pointer_image(&self) -> Option<crate::PointerImage> {
        self.cursor.image()
    }
}

/// The system's audio from ScreenCaptureKit, in the 5 ms chunks the stream sends.
pub struct SystemAudio {
    _capture: AudioCapture,
    queue: Arc<(Mutex<AudioQueue>, Condvar)>,
}

#[derive(Default)]
struct AudioQueue {
    samples: VecDeque<i16>,
    /// Server time of the front sample.
    front_ns: u64,
}

const AUDIO_FRAME_NS: u64 = SEC / AUDIO_RATE as u64;

impl SystemAudio {
    pub fn open() -> Result<SystemAudio, String> {
        if !screen_recording_allowed() {
            return Err("not allowed to record system audio (Screen & System Audio Recording)".into());
        }
        let queue = Arc::new((Mutex::new(AudioQueue::default()), Condvar::new()));
        let q = queue.clone();
        let capture = AudioCapture::start(move |samples| {
            let now = timing::now_ns();
            let mut a = q.0.lock().unwrap();
            if a.samples.is_empty() {
                a.front_ns = now.saturating_sub((samples.len() / AUDIO_CHANNELS) as u64 * AUDIO_FRAME_NS);
            }
            a.samples.extend(samples.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
            // More than 200 ms behind: the sender stalled; drop the oldest.
            let max = AUDIO_RATE as usize / 5 * AUDIO_CHANNELS;
            if a.samples.len() > max {
                let drop = (a.samples.len() - max) / AUDIO_CHANNELS * AUDIO_CHANNELS;
                a.samples.drain(..drop);
                a.front_ns += (drop / AUDIO_CHANNELS) as u64 * AUDIO_FRAME_NS;
            }
            q.1.notify_one();
        })?;
        Ok(SystemAudio { _capture: capture, queue })
    }
}

impl AudioSource for SystemAudio {
    fn next_chunk(&mut self, out: &mut [i16; CHUNK_SAMPLES]) -> Option<u64> {
        let (lock, ready) = &*self.queue;
        let mut a = lock.lock().unwrap();
        while a.samples.len() < CHUNK_SAMPLES {
            let (guard, timeout) = ready.wait_timeout(a, Duration::from_millis(100)).unwrap();
            a = guard;
            if timeout.timed_out() && a.samples.len() < CHUNK_SAMPLES {
                return None;
            }
        }
        for (o, s) in out.iter_mut().zip(a.samples.drain(..CHUNK_SAMPLES)) {
            *o = s;
        }
        let at = a.front_ns;
        a.front_ns += CHUNK_FRAMES as u64 * AUDIO_FRAME_NS;
        Some(at)
    }
}

/// Keeps the display awake while a client is connected (the stream stops when it sleeps), and
/// says how to fix missing permissions once at start.
pub fn start_background(shared: Arc<Shared>) {
    if !accessibility_allowed(true) {
        println!(
            "input: not allowed to control this Mac yet; allow DisplayWire Server (or the terminal running it) in System Settings › Privacy & Security › Accessibility"
        );
    }
    virtual_display::load_setting();
    input::load_pc_shortcuts();
    {
        let shared = shared.clone();
        let _ = std::thread::Builder::new().name("virtual-display".into()).spawn(move || {
            let mut connected_ns = 0;
            loop {
                let now = timing::now_ns();
                if shared.link.lock().unwrap().is_some() {
                    connected_ns = now;
                }
                let want = virtual_display::enabled() && connected_ns != 0 && now - connected_ns < virtual_display::LINGER_NS;
                match (want, virtual_display::current().is_some()) {
                    (true, false) => {
                        if let Err(e) = virtual_display::create() {
                            println!("virtual display: {e}");
                            virtual_display::set_enabled(false);
                        }
                    }
                    (false, true) => virtual_display::remove(),
                    _ => {}
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        });
    }
    let _ = std::thread::Builder::new().name("awake".into()).spawn(move || {
        let mut assertion: Option<u32> = None;
        let reason = ffi::Cf::string("DisplayWire is streaming this screen");
        let kind = ffi::Cf::string("PreventUserIdleDisplaySleep");
        loop {
            let connected = shared.link.lock().unwrap().is_some();
            match (connected, assertion) {
                (true, None) => {
                    let mut id = 0;
                    if unsafe { ffi::IOPMAssertionCreateWithName(kind.0, 255, reason.0, &mut id) } == 0 {
                        assertion = Some(id);
                    }
                }
                (false, Some(id)) => {
                    unsafe { ffi::IOPMAssertionRelease(id) };
                    assertion = None;
                }
                _ => {}
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// This Mac's name as the user set it (System Settings › General › About).
pub fn computer_name() -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}
