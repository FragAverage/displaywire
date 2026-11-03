//! Windows screen source: Desktop Duplication → video processor (NV12) →
//! hardware HEVC encoder.

mod capture;
mod convert;
mod encoder;
pub mod gamepad;
mod nvenc;
pub mod input;
pub mod loopback;
pub mod micout;
pub mod service;
pub mod setup;
pub mod shell;
pub mod tray;
pub mod updater;

use windows::Win32::Graphics::Direct3D11::{D3D11_QUERY_DESC, D3D11_QUERY_EVENT, ID3D11Device, ID3D11DeviceContext, ID3D11Query};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, GetCurrentProcess};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::{BOOL, s, w};

use crate::{Cli, EncodedFrame, Source};
use capture::Capture;
use convert::Converter;
use encoder::Encoder;
use nvenc::NvEncoder;

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

/// The video encoder in use.
enum AnyEncoder {
    Mft(Encoder),
    Nvenc(Box<NvEncoder>),
}

impl AnyEncoder {
    fn name(&self) -> &str {
        match self {
            AnyEncoder::Mft(e) => &e.name,
            AnyEncoder::Nvenc(e) => &e.name,
        }
    }

    /// See `NvEncoder::invalidate`; false if this encoder can't.
    fn invalidate(&mut self, time_100ns: i64) -> bool {
        match self {
            AnyEncoder::Mft(_) => false,
            AnyEncoder::Nvenc(e) => e.invalidate(time_100ns).map_err(|err| println!("encoder: {err}")).is_ok(),
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        match self {
            AnyEncoder::Mft(e) => e.set_bitrate(bps),
            AnyEncoder::Nvenc(e) => {
                if let Err(err) = e.set_bitrate(bps) {
                    println!("encoder: can't change the bitrate: {err}");
                }
            }
        }
    }

    fn encode(&mut self, frame: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, time_100ns: i64, keyframe: bool) -> Res<Option<Vec<u8>>> {
        match self {
            AnyEncoder::Mft(e) => e.encode(frame, time_100ns, keyframe),
            AnyEncoder::Nvenc(e) => e.encode(frame, time_100ns, keyframe),
        }
    }
}

/// `name` (e.g. "sc.exe") in the real System32, never found through PATH or the current folder:
/// the service and the installer run these as SYSTEM or an administrator.
pub fn system_tool(name: &str) -> std::path::PathBuf {
    let mut buf = [0u16; 260];
    let n = unsafe { windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut buf)) } as usize;
    let dir = if n > 0 && n < buf.len() { String::from_utf16_lossy(&buf[..n]) } else { r"C:\Windows\System32".into() };
    std::path::Path::new(&dir).join(name)
}

/// The encoder `kind` (`--encoder`) asks for, at `w`×`h`.
fn make_encoder(kind: &str, device: &ID3D11Device, w: u32, h: u32, fps: u32, bitrate: u32) -> Res<AnyEncoder> {
    Ok(match kind {
        "mft" => AnyEncoder::Mft(Encoder::new(device, w, h, fps, bitrate)?),
        "nvenc" => AnyEncoder::Nvenc(Box::new(NvEncoder::new(device, w, h, fps, bitrate)?)),
        "auto" => match NvEncoder::new(device, w, h, fps, bitrate) {
            Ok(e) => AnyEncoder::Nvenc(Box::new(e)),
            Err(e) => {
                println!("encoder: direct NVENC unavailable ({e}); using Media Foundation");
                AnyEncoder::Mft(Encoder::new(device, w, h, fps, bitrate)?)
            }
        },
        other => return Err(format!("unknown --encoder {other} (use auto, nvenc or mft)").into()),
    })
}

/// `--profile`: per-stage timings. The GPU wait makes the conversion's real
/// cost visible, since the calls themselves only queue work.
struct Profile {
    query: ID3D11Query,
    compose: timing::Samples,
    convert: timing::Samples,
    encode: timing::Samples,
    since_ns: u64,
}

impl Profile {
    fn new(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) -> Res<Profile> {
        let mut query = None;
        unsafe { device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut query)) }
            .ctx("CreateQuery")?;
        Ok(Profile {
            query: query.ok_or("no query")?,
            compose: Default::default(),
            convert: Default::default(),
            encode: Default::default(),
            since_ns: timing::now_ns(),
        })
    }

    /// Blocks until the GPU has finished everything queued so far.
    fn wait_for_gpu(&self, context: &ID3D11DeviceContext) {
        unsafe {
            context.End(&self.query);
            context.Flush();
            let mut done = BOOL(0);
            while !done.as_bool() {
                let _ = context.GetData(&self.query, Some((&raw mut done).cast()), size_of::<BOOL>() as u32, 0);
                std::hint::spin_loop();
            }
        }
    }

    fn report(&mut self, now: u64) {
        if now - self.since_ns < 1_000_000_000 {
            return;
        }
        let line = |s: &mut timing::Samples| {
            let m = s.summary();
            format!("{:.2}/{:.2}", timing::ms(m.p50), timing::ms(m.p99))
        };
        println!(
            "profile (ms p50/p99): compose {}  convert+gpu {}  encode {}",
            line(&mut self.compose),
            line(&mut self.convert),
            line(&mut self.encode)
        );
        self.compose.clear();
        self.convert.clear();
        self.encode.clear();
        self.since_ns = now;
    }
}

/// GPU scheduling priority class for this process, the way Sunshine does it:
/// our capture, convert and encode work then preempts a game's. Needs SYSTEM or
/// administrator rights with the right privilege; without them it's skipped.
fn raise_gpu_priority() {
    const D3DKMT_SCHEDULINGPRIORITYCLASS_REALTIME: i32 = 5;
    type SetClass = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, i32) -> i32;
    unsafe {
        let Ok(gdi) = LoadLibraryW(w!("gdi32.dll")) else { return };
        let Some(f) = GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass")) else { return };
        let f: SetClass = std::mem::transmute(f);
        let status = f(GetCurrentProcess(), D3DKMT_SCHEDULINGPRIORITYCLASS_REALTIME);
        if status == 0 {
            println!("gpu: real-time scheduling priority");
        } else {
            println!("gpu: couldn't raise scheduling priority (0x{status:08X}); run as SYSTEM or administrator");
        }
    }
}

pub struct ScreenSource {
    profile: Option<Profile>,
    capture: Capture,
    converter: Converter,
    encoder: AnyEncoder,
    /// Capture times of frames handed to the encoder but not yet returned.
    in_flight: std::collections::VecDeque<u64>,
    start_ns: u64,
    /// Pointer moved since the last encode; such frames are sent at most every `interval_ns`.
    pointer_dirty: bool,
    last_encode_ns: u64,
    interval_ns: u64,
    /// Whether the pointer is drawn into the video (the client draws it otherwise).
    pointer_in_video: bool,
    /// Encode the current image again even if nothing changed.
    repaint: bool,
    /// `--encoder`, the frame rate and the current bitrate, to rebuild the encoder at a new size.
    encoder_kind: String,
    fps: u32,
    bitrate: u32,
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
            raise_gpu_priority();
            let mut task = 0;
            let _ = AvSetMmThreadCharacteristicsW(w!("Games"), &mut task);
        }
        let capture = Capture::new()?;
        // NV12 needs even dimensions.
        let (w, h) = (capture.width & !1, capture.height & !1);
        let converter =
            Converter::new(&capture.device, &capture.context, &capture.composed, (capture.width, capture.height), (w, h), cli.fps)?;
        let bitrate = (cli.mbps * 1e6) as u32;
        let encoder = make_encoder(&cli.encoder, &capture.device, w, h, cli.fps, bitrate)?;
        let profile = cli.profile.then(|| Profile::new(&capture.device)).transpose()?;
        println!("encoding {w}x{h} HEVC at {:.0} Mbps with {}", cli.mbps, encoder.name());
        Ok(ScreenSource {
            profile,
            capture,
            converter,
            encoder,
            in_flight: Default::default(),
            start_ns: timing::now_ns(),
            pointer_dirty: false,
            last_encode_ns: 0,
            interval_ns: 1_000_000_000 / cli.fps.max(1) as u64,
            pointer_in_video: true,
            repaint: false,
            encoder_kind: cli.encoder.clone(),
            fps: cli.fps,
            bitrate,
        })
    }

    /// The desktop changed size (a resolution change, or a game switching mode): a converter and
    /// encoder for the new size. The new encoder starts with a keyframe carrying the new size,
    /// which is all a client needs to follow.
    fn rebuild(&mut self) -> Res<()> {
        let c = &self.capture;
        let (w, h) = (c.width & !1, c.height & !1);
        self.converter = Converter::new(&c.device, &c.context, &c.composed, (c.width, c.height), (w, h), self.fps)?;
        self.encoder = make_encoder(&self.encoder_kind, &c.device, w, h, self.fps, self.bitrate)?;
        self.in_flight.clear();
        println!("encoding {w}x{h} HEVC with {}", self.encoder.name());
        Ok(())
    }

    fn next(&mut self, force_keyframe: bool, timeout_ms: u32) -> Res<Option<EncodedFrame>> {
        let forced = (force_keyframe || self.repaint) && self.capture.has_frame;
        let pointer_due = self.last_encode_ns + self.interval_ns;
        let mut timeout = if forced { 0 } else { timeout_ms };
        if self.pointer_dirty {
            let ms = pointer_due.saturating_sub(timing::now_ns()).div_ceil(1_000_000);
            timeout = timeout.min(ms as u32);
        }
        let change = self.capture.next(timeout)?;
        if self.capture.take_resized() {
            self.rebuild()?;
        }
        // A client that draws the pointer gets it from `pointer()`; no frame needed for it.
        self.pointer_dirty |= change.pointer && self.pointer_in_video;
        let capture_ns = timing::now_ns();
        let pointer_only = self.pointer_dirty && capture_ns >= pointer_due;
        if !self.capture.has_frame || !(change.desktop || forced || pointer_only) {
            return Ok(None);
        }
        self.pointer_dirty = false;
        self.repaint = false;
        self.last_encode_ns = capture_ns;
        self.capture.compose(self.pointer_in_video)?;
        let composed_ns = timing::now_ns();
        let nv12 = self.converter.convert()?;
        if let Some(p) = &self.profile {
            p.wait_for_gpu(&self.capture.context);
        }
        let converted_ns = timing::now_ns();
        let time_100ns = ((capture_ns - self.start_ns) / 100) as i64;
        self.in_flight.push_back(capture_ns);
        let Some(data) = self.encoder.encode(&nv12, time_100ns, force_keyframe)? else {
            return Ok(None);
        };
        if let Some(p) = &mut self.profile {
            let now = timing::now_ns();
            p.compose.push((composed_ns - capture_ns) as i64);
            p.convert.push((converted_ns - composed_ns) as i64);
            p.encode.push((now - converted_ns) as i64);
            p.report(now);
        }
        let capture_ns = self.in_flight.pop_front().unwrap_or(capture_ns);
        let keyframe = hevc::is_keyframe(&data);
        Ok(Some(EncodedFrame { data, keyframe, capture_ns }))
    }
}

impl Source for ScreenSource {
    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.encoder.set_bitrate(bps);
    }

    fn invalidate(&mut self, capture_ns: &[u64]) -> bool {
        // The encoder knows frames by the timestamp `next` gave them.
        capture_ns.len() <= nvenc::REF_FRAMES as usize
            && capture_ns.iter().all(|&c| c >= self.start_ns && self.encoder.invalidate(((c - self.start_ns) / 100) as i64))
    }

    fn repaint(&mut self) {
        self.repaint = true;
    }

    fn set_pointer_in_video(&mut self, on: bool) {
        // Repaint so a still screen doesn't keep showing (or lack) the pointer.
        self.repaint |= on != self.pointer_in_video;
        self.pointer_in_video = on;
    }

    fn pointer(&self) -> Option<crate::PointerState> {
        let p = self.capture.pointer_at();
        let c = &self.capture;
        let norm = |v: i32, size: u32| (v.clamp(0, size as i32 - 1) as u64 * 65535 / (size.max(2) - 1) as u64) as u16;
        Some(crate::PointerState { visible: p.visible, x: norm(p.x, c.width), y: norm(p.y, c.height), shape: p.shape })
    }

    fn pointer_image(&self) -> Option<crate::PointerImage> {
        self.capture.pointer_image()
    }

    fn next_frame(&mut self, force_keyframe: bool, timeout_ms: u32) -> Result<Option<EncodedFrame>, String> {
        self.next(force_keyframe, timeout_ms).map_err(|e| e.to_string())
    }
}
