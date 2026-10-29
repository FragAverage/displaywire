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
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW, HDESK, OpenInputDesktop,
    SetThreadDesktop, UOI_NAME,
};
use windows::core::Interface;

use super::{Context, Res};

/// How often a lost duplication is retried. Failed attempts are cheap, and
/// this bounds how long the picture freezes when a UAC prompt comes or goes.
const RETRY_NS: u64 = 100_000_000;
const GENERIC_ALL: u32 = 0x1000_0000;

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
    pub has_frame: bool,
}

#[derive(Default)]
struct Pointer {
    visible: bool,
    /// Top-left of the shape on this monitor; may be negative near the edges.
    x: i32,
    y: i32,
    shape: Option<Shape>,
}

struct Shape {
    kind: u32,
    width: u32,
    /// In pixels; monochrome shapes hold an AND mask and an XOR mask, each this tall.
    height: u32,
    pitch: u32,
    data: Vec<u8>,
}

impl Shape {
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

/// First monitor attached to the desktop, and its adapter.
pub(super) fn primary_output() -> Res<(IDXGIAdapter1, IDXGIOutput, String)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(x) => x,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        for o in 0.. {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(x) => x,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let desc = unsafe { output.GetDesc()? };
            if desc.AttachedToDesktop.as_bool() {
                let adapter_name = unsafe { adapter.GetDesc1()? }.Description;
                let name = String::from_utf16_lossy(&adapter_name);
                return Ok((adapter, output, name.trim_end_matches('\0').to_string()));
            }
        }
    }
    Err("no monitor attached to the desktop".into())
}

impl Capture {
    pub fn new() -> Res<Capture> {
        // Started while the lock screen or a UAC prompt is up: capture that.
        let desktop = attach_input_desktop().inspect(|(_, name)| println!("capture: input desktop is {name}"));
        let (adapter, output, adapter_name) = primary_output()?;
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
        println!("capturing {width}x{height} on {adapter_name}");
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
            has_frame: false,
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
                p.shape = Some(Shape { kind: si.Type, width: si.Width, height, pitch: si.Pitch, data });
                change.pointer |= p.visible;
            }
        }
        let _ = unsafe { dup.ReleaseFrame() };
        Ok(change)
    }

    /// The current duplication, recreating it (at most every `RETRY_NS`) if it was lost.
    fn duplication(&mut self, timeout_ms: u32) -> Option<IDXGIOutputDuplication> {
        if self.dup.is_none() && timing::now_ns() >= self.next_retry_ns {
            self.follow_input_desktop();
            match unsafe { self.output.DuplicateOutput(&self.device) } {
                Ok(dup) => {
                    let mode = unsafe { dup.GetDesc() }.ModeDesc;
                    if (mode.Width, mode.Height) != (self.width, self.height) {
                        // TODO: rebuild the pipeline at the new size; for now keep waiting.
                        println!("desktop is now {}x{}, expected {}x{}", mode.Width, mode.Height, self.width, self.height);
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
        if let Ok(output) = primary_output().and_then(|(_, o, _)| Ok(o.cast::<IDXGIOutput1>()?)) {
            self.output = output;
        }
        if let Some((old, _)) = self.desktop.replace((desk, name)) {
            let _ = unsafe { CloseDesktop(old) };
        }
    }

    /// Fills `composed`: the desktop with the pointer drawn on it.
    pub fn compose(&mut self) -> Res<()> {
        unsafe { self.context.CopyResource(&self.composed, &self.frame) };
        let p = &self.pointer;
        let Some(shape) = p.shape.as_ref().filter(|_| p.visible) else { return Ok(()) };
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
