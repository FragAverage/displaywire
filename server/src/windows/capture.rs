//! Desktop Duplication capture of one monitor into a texture we own, with the
//! mouse pointer (which duplication reports separately) drawn on a copy.

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW, HDESK, OpenInputDesktop,
    SetThreadDesktop, UOI_NAME,
};
use windows::core::Interface;

use super::{Context, Res};
use crate::PointerImage;

/// How often a lost duplication is retried. Failed attempts are cheap, and
/// this bounds how long the picture freezes when a UAC prompt comes or goes.
const RETRY_NS: u64 = 100_000_000;
const GENERIC_ALL: u32 = 0x1000_0000;

/// Bumped whenever the captured monitor changes size, so input mapping can re-read its bounds.
pub static SCREEN_GENERATION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub struct Capture {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// None while the desktop is unavailable (lock screen, display off, mode change).
    dup: Option<IDXGIOutputDuplication>,
    next_retry_ns: u64,
    /// The input desktop this thread is attached to, once it has had to leave the default one.
    desktop: Option<(HDESK, String)>,
    /// Latest desktop image (BGRA), kept so a keyframe can be re-encoded when nothing changes.
    frame: ID3D11Texture2D,
    /// `frame` with the pointer drawn on: what gets encoded.
    pub composed: ID3D11Texture2D,
    /// CPU-readable copy of the desktop under the pointer, for blending.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    pointer: Pointer,
    pub width: u32,
    pub height: u32,
    /// The GDI name of the monitor captured.
    pub monitor: String,
    pub has_frame: bool,
    /// The size changed since `take_resized` was last called.
    resized: bool,
}

#[derive(Default)]
struct Pointer {
    visible: bool,
    /// Top-left of the shape on this monitor; may be negative near the edges.
    x: i32,
    y: i32,
    shape: Option<Shape>,
    /// Bumped with every new shape.
    shape_id: u32,
}

struct Shape {
    kind: u32,
    width: u32,
    /// In pixels; monochrome shapes hold an AND mask and an XOR mask, each this tall.
    height: u32,
    pitch: u32,
    /// The pixel that is the pointer's position.
    hot: (u32, u32),
    data: Vec<u8>,
}

/// Where the pointer is: its hotspot on this monitor, in pixels.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PointerAt {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub shape: u32,
}

impl Shape {
    /// RGBA for a client to draw. Pixels that invert the screen (the I-beam, some hands) can't be shown
    /// as such by an ordinary cursor: they become black with a white edge, visible on any background.
    fn rgba(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut out = vec![0u8; w * h * 4];
        let mut invert = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let px = |row: usize| {
                    let o = row * self.pitch as usize + x * 4;
                    u32::from_le_bytes(self.data[o..o + 4].try_into().unwrap())
                };
                let bgra = match self.kind {
                    k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 => px(y),
                    k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 => {
                        let s = px(y);
                        match (s >> 24, s & 0x00FF_FFFF) {
                            (0, rgb) => 0xFF00_0000 | rgb,
                            (_, 0) => 0,
                            _ => {
                                invert[i] = true;
                                0xFF00_0000
                            }
                        }
                    }
                    _ => {
                        let bit = |row: usize| self.data[row * self.pitch as usize + x / 8] & (0x80 >> (x % 8)) != 0;
                        match (bit(y), bit(y + h)) {
                            (true, false) => 0,
                            (false, false) => 0xFF00_0000,
                            (false, true) => 0xFFFF_FFFF,
                            (true, true) => {
                                invert[i] = true;
                                0xFF00_0000
                            }
                        }
                    }
                };
                out[i * 4..i * 4 + 4].copy_from_slice(&[(bgra >> 16) as u8, (bgra >> 8) as u8, bgra as u8, (bgra >> 24) as u8]);
            }
        }
        // White edge around inverting pixels, on transparent neighbours only.
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if out[i * 4 + 3] != 0 {
                    continue;
                }
                let near = (y.saturating_sub(1)..(y + 2).min(h)).any(|ny| (x.saturating_sub(1)..(x + 2).min(w)).any(|nx| invert[ny * w + nx]));
                if near {
                    out[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
        out
    }

    /// The pointer pixel at (`x`, `y`) drawn over desktop pixel `d` (BGRA).
    fn blend(&self, x: u32, y: u32, d: u32) -> u32 {
        let px = |row: u32| {
            let o = (row * self.pitch + x * 4) as usize;
            u32::from_le_bytes(self.data[o..o + 4].try_into().unwrap())
        };
        match self.kind {
            k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 => {
                let s = px(y);
                let a = s >> 24;
                let mix = |shift: u32| {
                    let (s, d) = ((s >> shift) & 0xFF, (d >> shift) & 0xFF);
                    ((s * a + d * (255 - a) + 127) / 255) << shift
                };
                0xFF00_0000 | mix(16) | mix(8) | mix(0)
            }
            k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 => {
                let s = px(y);
                // Alpha 0: replace; alpha 0xFF: XOR with the screen.
                if s >> 24 == 0 { 0xFF00_0000 | s } else { 0xFF00_0000 | (d ^ s) }
            }
            _ => {
                let bit = |row: u32| {
                    let byte = self.data[(row * self.pitch + x / 8) as usize];
                    if byte & (0x80 >> (x % 8)) != 0 { 0x00FF_FFFF } else { 0 }
                };
                0xFF00_0000 | ((d & bit(y)) ^ bit(y + self.height))
            }
        }
    }
}

/// What changed in one `Capture::next`.
#[derive(Default)]
pub struct Change {
    pub desktop: bool,
    pub pointer: bool,
}

/// Attaches the calling thread to the desktop that currently receives input.
fn attach_input_desktop() -> Option<(HDESK, String)> {
    let desk = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) }.ok()?;
    if unsafe { SetThreadDesktop(desk) }.is_err() {
        let _ = unsafe { CloseDesktop(desk) };
        return None;
    }
    let name = desktop_name(desk);
    Some((desk, name))
}

fn desktop_name(desk: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            size_of_val(&buf) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return "?".into();
    }
    String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string()
}

/// A monitor attached to the desktop, and the adapter it hangs off.
pub(super) struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub adapter_name: String,
    /// GDI name (`\\.\DISPLAY2`): stable while the monitor stays connected.
    pub device: String,
    pub rect: RECT,
}

impl Output {
    /// Windows' primary monitor is the one at the desktop's origin.
    pub fn primary(&self) -> bool {
        (self.rect.left, self.rect.top) == (0, 0)
    }
}

/// Every monitor attached to the desktop, in DXGI's order.
pub(super) fn outputs() -> Res<Vec<Output>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut found = Vec::new();
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(x) => x,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        let adapter_name = String::from_utf16_lossy(&unsafe { adapter.GetDesc1()? }.Description).trim_end_matches('\0').to_string();
        for o in 0.. {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(x) => x,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let desc = unsafe { output.GetDesc()? };
            if desc.AttachedToDesktop.as_bool() {
                let device = String::from_utf16_lossy(&desc.DeviceName).trim_end_matches('\0').to_string();
                found.push(Output { adapter: adapter.clone(), output, adapter_name: adapter_name.clone(), device, rect: desc.DesktopCoordinates });
            }
        }
    }
    Ok(found)
}

/// Which monitor each stream shows, by GDI name (for input to land on the right one).
static STREAMS: std::sync::Mutex<Option<std::collections::HashMap<u8, String>>> = std::sync::Mutex::new(None);

/// Records that `stream` shows `device` (None: it stopped).
pub(super) fn set_stream(stream: u8, device: Option<String>) {
    let mut streams = STREAMS.lock().unwrap();
    let map = streams.get_or_insert_with(Default::default);
    match device {
        Some(d) => map.insert(stream, d),
        None => map.remove(&stream),
    };
    SCREEN_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The monitor `stream` shows (the primary one if it isn't known).
pub(super) fn stream_output(stream: u8) -> Res<Output> {
    let device = STREAMS.lock().unwrap().as_ref().and_then(|m| m.get(&stream).cloned());
    output(device.as_deref())
}

/// Monitor `device` if it is attached, else the primary, else the first.
pub(super) fn output(device: Option<&str>) -> Res<Output> {
    let mut all = outputs()?;
    if all.is_empty() {
        return Err("no monitor attached to the desktop".into());
    }
    let at = device
        .and_then(|d| all.iter().position(|o| o.device == d))
        .or_else(|| all.iter().position(Output::primary))
        .unwrap_or(0);
    Ok(all.swap_remove(at))
}

impl Capture {
    /// Captures monitor `device` (by GDI name), or the primary one.
    pub fn new(device: Option<&str>) -> Res<Capture> {
        // Started while the lock screen or a UAC prompt is up: capture that.
        let desktop = attach_input_desktop().inspect(|(_, name)| println!("capture: input desktop is {name}"));
        let Output { adapter, output, adapter_name, device: monitor, .. } = output(device)?;
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .ctx("D3D11CreateDevice")?;
        }
        let device = device.ok_or("no D3D11 device")?;
        let context = context.ok_or("no D3D11 context")?;
        unsafe {
            // Media Foundation uses the device from its own threads.
            let _ = context.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
            // Highest GPU scheduling priority for our work.
            let _ = device.cast::<IDXGIDevice>()?.SetGPUThreadPriority(7);
        }
        let output: IDXGIOutput1 = output.cast()?;
        let dup = unsafe { output.DuplicateOutput(&device) }.ctx("DuplicateOutput")?;
        let desc = unsafe { dup.GetDesc() };
        let (width, height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        println!("capturing {monitor} ({width}x{height}) on {adapter_name}");
        let frame = texture(&device, width, height)?;
        let composed = texture(&device, width, height)?;
        println!("capture ready");
        Ok(Capture {
            device,
            context,
            output,
            dup: Some(dup),
            next_retry_ns: 0,
            desktop,
            frame,
            composed,
            staging: None,
            pointer: Pointer::default(),
            width,
            height,
            monitor,
            has_frame: false,
            resized: false,
        })
    }

    /// Waits up to `timeout_ms` for a new desktop image or pointer update.
    pub fn next(&mut self, timeout_ms: u32) -> Res<Change> {
        let Some(dup) = self.duplication(timeout_ms) else { return Ok(Change::default()) };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Change::default()),
            Err(e) => {
                // Mode change, UAC prompt, lock screen, display off...: start over once it's back.
                println!("desktop duplication lost ({}), retrying", e.message().trim());
                self.dup = None;
                return Ok(Change::default());
            }
        }
        let mut change = Change { desktop: info.LastPresentTime != 0, pointer: false };
        if change.desktop && let Some(resource) = resource {
            let tex: ID3D11Texture2D = resource.cast()?;
            unsafe { self.context.CopyResource(&self.frame, &tex) };
            self.has_frame = true;
        }
        let p = &mut self.pointer;
        if info.LastMouseUpdateTime != 0 {
            let pos = info.PointerPosition;
            let visible = pos.Visible.as_bool();
            change.pointer = visible != p.visible || (visible && (pos.Position.x, pos.Position.y) != (p.x, p.y));
            (p.visible, p.x, p.y) = (visible, pos.Position.x, pos.Position.y);
        }
        if info.PointerShapeBufferSize > 0 {
            let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut needed = 0;
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let r = unsafe {
                dup.GetFramePointerShape(data.len() as u32, data.as_mut_ptr().cast(), &mut needed, &mut si)
            };
            if r.is_ok() {
                let height = if si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 { si.Height / 2 } else { si.Height };
                p.shape = Some(Shape { kind: si.Type, width: si.Width, height, pitch: si.Pitch, hot: (si.HotSpot.x.max(0) as u32, si.HotSpot.y.max(0) as u32), data });
                p.shape_id = p.shape_id.wrapping_add(1);
                change.pointer |= p.visible;
            }
        }
        let _ = unsafe { dup.ReleaseFrame() };
        Ok(change)
    }

    /// New textures at the new desktop size; the encoder side rebuilds after `take_resized`.
    fn resize(&mut self, width: u32, height: u32) -> Res<()> {
        self.frame = texture(&self.device, width, height)?;
        self.composed = texture(&self.device, width, height)?;
        self.staging = None;
        (self.width, self.height) = (width, height);
        self.has_frame = false;
        self.resized = true;
        SCREEN_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Whether the size changed since the last call.
    pub fn take_resized(&mut self) -> bool {
        std::mem::take(&mut self.resized)
    }

    /// The current duplication, recreating it (at most every `RETRY_NS`) if it was lost.
    fn duplication(&mut self, timeout_ms: u32) -> Option<IDXGIOutputDuplication> {
        if self.dup.is_none() && timing::now_ns() >= self.next_retry_ns {
            self.follow_input_desktop();
            match unsafe { self.output.DuplicateOutput(&self.device) } {
                Ok(dup) => {
                    let mode = unsafe { dup.GetDesc() }.ModeDesc;
                    if (mode.Width, mode.Height) != (self.width, self.height) {
                        println!("desktop is now {}x{} (was {}x{})", mode.Width, mode.Height, self.width, self.height);
                        match self.resize(mode.Width, mode.Height) {
                            Ok(()) => self.dup = Some(dup),
                            Err(e) => println!("capture: can't resize: {e}"),
                        }
                    } else {
                        println!("desktop duplication restored");
                        self.dup = Some(dup);
                    }
                }
                Err(e) => println!("desktop unavailable ({}), retrying", e.message().trim()),
            }
            self.next_retry_ns = timing::now_ns() + RETRY_NS;
        }
        if self.dup.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms.min(100) as u64));
        }
        self.dup.clone()
    }

    /// Duplication only works from a thread on the desktop that currently
    /// receives input, and UAC prompts and the lock screen live on their own
    /// (Winlogon) desktop, which only SYSTEM may open. Without that right this
    /// stays on the user's desktop and those screens can't be captured.
    fn follow_input_desktop(&mut self) {
        let Ok(desk) = (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) })
        else {
            return;
        };
        let name = desktop_name(desk);
        if self.desktop.as_ref().is_some_and(|(_, n)| *n == name) {
            let _ = unsafe { CloseDesktop(desk) };
            return;
        }
        if unsafe { SetThreadDesktop(desk) }.is_err() {
            let _ = unsafe { CloseDesktop(desk) };
            return;
        }
        println!("capture: following the input desktop ({name})");
        // The output was enumerated on the previous desktop; take a fresh one.
        if let Ok(output) = output(Some(&self.monitor)).and_then(|o| Ok(o.output.cast::<IDXGIOutput1>()?)) {
            self.output = output;
        }
        if let Some((old, _)) = self.desktop.replace((desk, name)) {
            let _ = unsafe { CloseDesktop(old) };
        }
    }

    /// Where the pointer is, for clients that draw it themselves.
    pub fn pointer_at(&self) -> PointerAt {
        let p = &self.pointer;
        let hot = p.shape.as_ref().map_or((0, 0), |s| s.hot);
        PointerAt { visible: p.visible && p.shape.is_some(), x: p.x + hot.0 as i32, y: p.y + hot.1 as i32, shape: p.shape_id }
    }

    pub fn pointer_image(&self) -> Option<PointerImage> {
        let p = &self.pointer;
        let s = p.shape.as_ref()?;
        Some(PointerImage { id: p.shape_id, width: s.width, height: s.height, hot: s.hot, rgba: s.rgba() })
    }

    /// Fills `composed`: the desktop, with the pointer drawn on unless `with_pointer` is false
    /// (the client draws it).
    pub fn compose(&mut self, with_pointer: bool) -> Res<()> {
        unsafe { self.context.CopyResource(&self.composed, &self.frame) };
        let p = &self.pointer;
        let Some(shape) = p.shape.as_ref().filter(|_| p.visible && with_pointer) else { return Ok(()) };
        let (x0, y0) = (p.x.max(0) as u32, p.y.max(0) as u32);
        let x1 = (p.x + shape.width as i32).clamp(0, self.width as i32) as u32;
        let y1 = (p.y + shape.height as i32).clamp(0, self.height as i32) as u32;
        if x0 >= x1 || y0 >= y1 {
            return Ok(());
        }
        let (w, h) = (x1 - x0, y1 - y0);
        if self.staging.as_ref().is_none_or(|s| s.1 < w || s.2 < h) {
            let tex = new_texture(&self.device, w, h, D3D11_USAGE_STAGING, D3D11_BIND_FLAG(0), D3D11_CPU_ACCESS_READ.0 as u32)?;
            self.staging = Some((tex, w, h));
        }
        let staging = &self.staging.as_ref().unwrap().0;
        let area = D3D11_BOX { left: x0, top: y0, front: 0, right: x1, bottom: y1, back: 1 };
        let mut pixels = vec![0u32; (w * h) as usize];
        unsafe {
            self.context.CopySubresourceRegion(staging, 0, 0, 0, 0, &self.frame, 0, Some(&area));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ctx("map pointer area")?;
            for row in 0..h {
                let src = mapped.pData.cast::<u8>().add((row * mapped.RowPitch) as usize).cast::<u32>();
                std::ptr::copy_nonoverlapping(src, pixels[(row * w) as usize..].as_mut_ptr(), w as usize);
            }
            self.context.Unmap(staging, 0);
        }
        let (sx0, sy0) = ((x0 as i32 - p.x) as u32, (y0 as i32 - p.y) as u32);
        for row in 0..h {
            for col in 0..w {
                let d = &mut pixels[(row * w + col) as usize];
                *d = shape.blend(sx0 + col, sy0 + row, *d);
            }
        }
        unsafe { self.context.UpdateSubresource(&self.composed, 0, Some(&area), pixels.as_ptr().cast(), w * 4, 0) };
        Ok(())
    }
}

fn texture(device: &ID3D11Device, width: u32, height: u32) -> Res<ID3D11Texture2D> {
    let bind = D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0);
    new_texture(device, width, height, D3D11_USAGE_DEFAULT, bind, 0)
}

fn new_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    bind: D3D11_BIND_FLAG,
    cpu: u32,
) -> Res<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.ctx("create capture texture")?;
    Ok(tex.ok_or("CreateTexture2D returned nothing")?)
}
