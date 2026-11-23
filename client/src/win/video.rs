//! Windows video: HEVC access units → Media Foundation's decoder (hardware, through D3D11) → the
//! GPU's video processor → a flip-model swap chain on the stream window. Decoded pictures stay on
//! the GPU from the decoder to the screen. The banner, the "connecting" placeholder and the stats
//! graph are drawn over the picture with Direct2D, only while one of them shows.
//!
//! Everything here but `Layer::attach` and the `OverlayHandle` runs on the receiver thread: the
//! D3D objects are made there on first use and never leave it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES, D2D1_ROUNDED_RECT, D2D1CreateFactory,
    ID2D1Factory, ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::{GUID, Interface, w};
use windows_numerics::Vector2;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// What is drawn over the picture. Set by the UI thread, drawn by the receiver thread.
#[derive(Clone, Default, PartialEq)]
pub struct Overlay {
    /// Fills the window until the first picture: the PC's name and what is happening.
    pub placeholder: Option<(String, String)>,
    /// A line at the top: connection state, transfer progress.
    pub banner: Option<String>,
    /// The stats graph: a line of text and a minute of values per row.
    pub graph: Option<Vec<(String, Vec<f32>)>>,
}

#[derive(Default)]
struct Shared {
    overlay: Mutex<Overlay>,
    version: AtomicU64,
}

/// The UI thread's end of a window's overlay.
#[derive(Clone)]
pub struct OverlayHandle(Arc<Shared>);

impl OverlayHandle {
    pub fn update(&self, change: impl FnOnce(&mut Overlay)) {
        let mut o = self.0.overlay.lock().unwrap();
        let before = o.clone();
        change(&mut o);
        if *o != before {
            self.0.version.fetch_add(1, Relaxed);
        }
    }
}

/// What `Layer::decode` did with an access unit.
#[derive(Default)]
pub struct Shown {
    /// A picture came out and is on the screen.
    pub presented: bool,
    /// The video's size, when this is the first picture or it changed.
    pub size: Option<(u32, u32)>,
}

/// A stream window's video output.
pub struct Layer {
    hwnd: isize,
    shared: Arc<Shared>,
    renderer: Option<Renderer>,
    /// Why there is no renderer, said once.
    failed: bool,
}

// The window handle is only a number here, and the renderer (COM objects) is made, used and dropped
// on the receiver thread alone.
unsafe impl Send for Layer {}

impl Layer {
    /// The video output for `window`. UI thread.
    pub fn attach(window: &Window) -> Layer {
        let RawWindowHandle::Win32(handle) = window.window_handle().expect("window handle").as_raw() else {
            panic!("not a Win32 window");
        };
        Layer { hwnd: handle.hwnd.get(), shared: Arc::default(), renderer: None, failed: false }
    }

    pub fn overlay(&self) -> OverlayHandle {
        OverlayHandle(self.shared.clone())
    }

    fn renderer(&mut self) -> Result<&mut Renderer, String> {
        if self.renderer.is_none() {
            if self.failed {
                return Err("no video output".into());
            }
            match Renderer::new(HWND(self.hwnd as *mut _), self.shared.clone()) {
                Ok(r) => self.renderer = Some(r),
                Err(e) => {
                    self.failed = true;
                    return Err(format!("can't set up video output: {e}"));
                }
            }
        }
        Ok(self.renderer.as_mut().unwrap())
    }

    /// Decodes one access unit (Annex B) and shows the picture that comes out.
    pub fn decode(&mut self, au: &[u8], keyframe: bool) -> Result<Shown, String> {
        self.renderer()?.decode(au, keyframe)
    }

    /// Repaints if the window changed size or the overlay changed. Called every few milliseconds.
    pub fn tick(&mut self) {
        if let Ok(r) = self.renderer()
            && let Err(e) = r.tick()
        {
            eprintln!("video output: {e}");
        }
    }
}

fn err(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

fn start_media_foundation() -> Result<(), String> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED.get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(err("Media Foundation"))).clone()
}

struct Decoder {
    transform: IMFTransform,
    /// The SPS it was made for: another one means another decoder.
    sps: Vec<u8>,
    /// Whether the decoder makes its own output samples (it does when it decodes on the GPU).
    provides_samples: bool,
    /// The visible size, once the first picture said.
    size: Option<(u32, u32)>,
    time: i64,
}

/// A decoded picture: the sample keeps the texture from being reused while it shows.
struct Picture {
    _sample: IMFSample,
    texture: ID3D11Texture2D,
    index: u32,
}

struct Renderer {
    hwnd: HWND,
    shared: Arc<Shared>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    swap: IDXGISwapChain1,
    tearing: bool,
    /// The swap chain's size.
    size: (u32, u32),
    manager: IMFDXGIDeviceManager,
    decoder: Option<Decoder>,
    picture: Option<Picture>,
    /// The video processor, for one video size and one window size.
    processor: Option<Processor>,
    /// Input views by texture and array slice (the decoder cycles through a small pool).
    inputs: HashMap<(usize, u32), ID3D11VideoProcessorInputView>,
    rtv: Option<ID3D11RenderTargetView>,
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: Option<ID2D1RenderTarget>,
    seen_version: u64,
}

struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output: ID3D11VideoProcessorOutputView,
    video: (u32, u32),
    window: (u32, u32),
}

impl Renderer {
    fn new(hwnd: HWND, shared: Arc<Shared>) -> Result<Renderer, String> {
        unsafe {
            // Whatever mode the thread is already in (the audio output may have set one) is fine.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            start_media_foundation()?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(err("Direct3D 11"))?;
            let (device, context) = (device.ok_or("no Direct3D device")?, context.ok_or("no Direct3D context")?);
            // The decoder works on this device from threads of its own.
            let protect: ID3D11Multithread = context.cast().map_err(err("ID3D11Multithread"))?;
            let _ = protect.SetMultithreadProtected(true);
            let video_device: ID3D11VideoDevice = device.cast().map_err(err("this GPU has no video support"))?;
            let video_context: ID3D11VideoContext1 = context.cast().map_err(err("this GPU has no video support"))?;

            let dxgi: IDXGIDevice = device.cast().map_err(err("IDXGIDevice"))?;
            let adapter = dxgi.GetAdapter().map_err(err("adapter"))?;
            let factory: IDXGIFactory2 = adapter.GetParent().map_err(err("DXGI factory"))?;
            let mut allow = windows::core::BOOL(0);
            let tearing = factory
                .cast::<IDXGIFactory5>()
                .and_then(|f| f.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &mut allow as *mut _ as *mut _, size_of_val(&allow) as u32))
                .is_ok()
                && allow.as_bool();
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 },
                ..Default::default()
            };
            let swap = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None).map_err(err("swap chain"))?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let got = swap.GetDesc1().map_err(err("swap chain"))?;

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(err("device manager"))?;
            let manager = manager.ok_or("no device manager")?;
            manager.ResetDevice(&device, token).map_err(err("device manager"))?;

            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).map_err(err("Direct2D"))?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(err("DirectWrite"))?;
            let mut r = Renderer {
                hwnd,
                shared,
                device,
                context,
                video_device,
                video_context,
                swap,
                tearing,
                size: (got.Width, got.Height),
                manager,
                decoder: None,
                picture: None,
                processor: None,
                inputs: HashMap::new(),
                rtv: None,
                d2d,
                dwrite,
                target: None,
                seen_version: u64::MAX,
            };
            r.present()?;
            Ok(r)
        }
    }

    fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut rect) };
        ((rect.right - rect.left).max(0) as u32, (rect.bottom - rect.top).max(0) as u32)
    }

    fn tick(&mut self) -> Result<(), String> {
        let size = self.client_size();
        if (size != self.size && size.0 > 0 && size.1 > 0) || self.shared.version.load(Relaxed) != self.seen_version {
            self.present()?;
        }
        Ok(())
    }

    /// Makes the swap chain the window's size. Everything that points at its buffers goes first.
    fn fit_window(&mut self) -> Result<(), String> {
        let size = self.client_size();
        if size == self.size || size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        self.processor = None;
        self.rtv = None;
        self.target = None;
        unsafe {
            self.context.ClearState();
            let flags = if self.tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { DXGI_SWAP_CHAIN_FLAG(0) };
            self.swap.ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, flags).map_err(err("resize"))?;
        }
        self.size = size;
        Ok(())
    }

    fn decoder_for(&mut self, sps: &[u8]) -> Result<(), String> {
        if self.decoder.as_ref().is_some_and(|d| d.sps == sps) {
            return Ok(());
        }
        // A new stream (or a new size): start again, so nothing of the old one is left in the decoder.
        self.decoder = None;
        self.picture = None;
        self.inputs.clear();
        let (width, height) = hevc::sps_dimensions(sps).unwrap_or((1920, 1080));
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_HEVC };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), None, &mut list, &mut count)
                .map_err(err("looking for an HEVC decoder"))?;
            let mut transform: Option<IMFTransform> = None;
            for i in 0..count as usize {
                if let Some(activate) = (*list.add(i)).take()
                    && transform.is_none()
                {
                    transform = activate.ActivateObject::<IMFTransform>().ok();
                }
            }
            CoTaskMemFree(Some(list as *const _));
            let transform = transform.ok_or(
                "Windows has no HEVC decoder. Install \"HEVC Video Extensions\" from the Microsoft Store (search for it there), then connect again.",
            )?;

            let mut on_gpu = false;
            if let Ok(attributes) = transform.GetAttributes() {
                let _ = attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
                if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0 {
                    on_gpu = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, self.manager.as_raw() as usize).is_ok();
                }
            }
            if !on_gpu {
                return Err("the HEVC decoder can't use this graphics card".into());
            }
            let ty = MFCreateMediaType().map_err(err("media type"))?;
            ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(err("media type"))?;
            ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC).map_err(err("media type"))?;
            ty.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64).map_err(err("media type"))?;
            ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(err("media type"))?;
            transform.SetInputType(0, &ty, 0).map_err(err("the decoder refused HEVC"))?;
            set_nv12_output(&transform)?;
            let info = transform.GetOutputStreamInfo(0).map_err(err("decoder output"))?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            self.decoder = Some(Decoder { transform, sps: sps.to_vec(), provides_samples, size: None, time: 0 });
        }
        Ok(())
    }

    fn decode(&mut self, au: &[u8], keyframe: bool) -> Result<Shown, String> {
        if keyframe && let Some(sps) = hevc::nals(au).into_iter().find(|n| hevc::nal_type(n) == hevc::SPS) {
            self.decoder_for(sps)?;
        }
        let Some(d) = &mut self.decoder else { return Err("no parameter sets yet".into()) };
        let mut shown = Shown::default();
        let mut newest = None;
        unsafe {
            let buffer = MFCreateMemoryBuffer(au.len() as u32).map_err(err("buffer"))?;
            let mut at = std::ptr::null_mut();
            buffer.Lock(&mut at, None, None).map_err(err("buffer"))?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), at, au.len());
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(au.len() as u32).map_err(err("buffer"))?;
            let sample = MFCreateSample().map_err(err("sample"))?;
            sample.AddBuffer(&buffer).map_err(err("sample"))?;
            d.time += 1;
            let _ = sample.SetSampleTime(d.time * 10_000);
            let _ = sample.SetSampleDuration(10_000);

            let mut fed = false;
            // Feed, then take everything that is ready; if the decoder is full, take first and feed again.
            for _ in 0..4 {
                if !fed {
                    match d.transform.ProcessInput(0, &sample, 0) {
                        Ok(()) => fed = true,
                        Err(e) if e.code() == MF_E_NOTACCEPTING => {}
                        Err(e) => return Err(format!("decoder input: {e}")),
                    }
                }
                loop {
                    let mut out = [MFT_OUTPUT_DATA_BUFFER::default()];
                    if !d.provides_samples {
                        return Err("the decoder isn't decoding on the graphics card".into());
                    }
                    let mut status = 0u32;
                    let result = d.transform.ProcessOutput(0, &mut out, &mut status);
                    let got = std::mem::ManuallyDrop::take(&mut out[0].pSample);
                    std::mem::ManuallyDrop::drop(&mut out[0].pEvents);
                    match result {
                        Ok(()) => {
                            if let Some(s) = got {
                                newest = Some(s);
                            }
                        }
                        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                            set_nv12_output(&d.transform)?;
                            d.size = None;
                        }
                        Err(e) => return Err(format!("decoder output: {e}")),
                    }
                }
                if fed {
                    break;
                }
            }
            if !fed {
                return Err("the decoder took no input".into());
            }
            if let Some(sample) = newest {
                if d.size.is_none() {
                    let size = output_size(&d.transform)?;
                    d.size = Some(size);
                    shown.size = Some(size);
                    self.processor = None;
                }
                let buffer = sample.GetBufferByIndex(0).map_err(err("decoded buffer"))?;
                let dxgi: IMFDXGIBuffer = buffer.cast().map_err(err("the decoder gave no GPU picture"))?;
                let mut texture: Option<ID3D11Texture2D> = None;
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut _).map_err(err("decoded texture"))?;
                let index = dxgi.GetSubresourceIndex().map_err(err("decoded texture"))?;
                self.picture = Some(Picture { _sample: sample, texture: texture.ok_or("no decoded texture")?, index });
                shown.presented = true;
            }
        }
        if shown.presented {
            self.present()?;
        }
        Ok(shown)
    }

    fn processor_for(&mut self, video: (u32, u32)) -> Result<(), String> {
        if self.processor.as_ref().is_some_and(|p| p.video == video && p.window == self.size) {
            return Ok(());
        }
        self.processor = None;
        self.inputs.clear();
        unsafe {
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: video.0,
                InputHeight: video.1,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: self.size.0,
                OutputHeight: self.size.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = self.video_device.CreateVideoProcessorEnumerator(&desc).map_err(err("video processor"))?;
            let processor = self.video_device.CreateVideoProcessor(&enumerator, 0).map_err(err("video processor"))?;
            let back: ID3D11Texture2D = self.swap.GetBuffer(0).map_err(err("back buffer"))?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC { ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D, ..Default::default() };
            let mut output = None;
            self.video_device.CreateVideoProcessorOutputView(&back, &enumerator, &view_desc, Some(&mut output)).map_err(err("output view"))?;
            let output = output.ok_or("no output view")?;

            // Letterboxed: the whole picture, as large as fits, centred on black.
            let scale = (self.size.0 as f64 / video.0 as f64).min(self.size.1 as f64 / video.1 as f64);
            let (w, h) = ((video.0 as f64 * scale).round() as i32, (video.1 as f64 * scale).round() as i32);
            let (x, y) = ((self.size.0 as i32 - w) / 2, (self.size.1 as i32 - h) / 2);
            let source = RECT { left: 0, top: 0, right: video.0 as i32, bottom: video.1 as i32 };
            let dest = RECT { left: x, top: y, right: x + w, bottom: y + h };
            let whole = RECT { left: 0, top: 0, right: self.size.0 as i32, bottom: self.size.1 as i32 };
            let c = &self.video_context;
            c.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            c.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
            c.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest));
            c.VideoProcessorSetOutputTargetRect(&processor, true, Some(&whole));
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            c.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            // The server encodes BT.709, limited range. And the picture as it is: none of the
            // driver's "enhancements".
            c.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            c.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            c.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            self.processor = Some(Processor { enumerator, processor, output, video, window: self.size });
        }
        Ok(())
    }

    /// Draws the current picture (or black) and the overlay, and shows it at once.
    fn present(&mut self) -> Result<(), String> {
        self.fit_window()?;
        if self.size.0 == 0 || self.size.1 == 0 {
            return Ok(());
        }
        let video = self.decoder.as_ref().and_then(|d| d.size);
        let mut drew = false;
        if let (Some(video), Some(picture)) = (video, &self.picture) {
            let (texture, index) = (picture.texture.clone(), picture.index);
            self.processor_for(video)?;
            let p = self.processor.as_ref().unwrap();
            let key = (texture.as_raw() as usize, index);
            if !self.inputs.contains_key(&key) {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: index } },
                };
                let mut view = None;
                unsafe { self.video_device.CreateVideoProcessorInputView(&texture, &p.enumerator, &desc, Some(&mut view)) }.map_err(err("input view"))?;
                if self.inputs.len() > 64 {
                    self.inputs.clear();
                }
                self.inputs.insert(key, view.ok_or("no input view")?);
            }
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(self.inputs[&key].clone())),
                ..Default::default()
            };
            let blt = unsafe { self.video_context.VideoProcessorBlt(&p.processor, &p.output, 0, std::slice::from_ref(&stream)) };
            drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
            blt.map_err(err("drawing the picture"))?;
            drew = true;
        }
        if !drew {
            unsafe {
                if self.rtv.is_none() {
                    let back: ID3D11Texture2D = self.swap.GetBuffer(0).map_err(err("back buffer"))?;
                    let mut rtv = None;
                    self.device.CreateRenderTargetView(&back, None, Some(&mut rtv)).map_err(err("render target"))?;
                    self.rtv = rtv;
                }
                if let Some(rtv) = &self.rtv {
                    self.context.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
                }
            }
        }
        self.seen_version = self.shared.version.load(Relaxed);
        let overlay = self.shared.overlay.lock().unwrap().clone();
        if overlay != Overlay::default()
            && let Err(e) = self.draw_overlay(&overlay)
        {
            // The picture matters more than what is written over it.
            eprintln!("overlay: {e}");
            self.target = None;
        }
        let flags = if self.tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };
        unsafe { self.swap.Present(0, flags) }.ok().map_err(err("present"))
    }

    fn text_format(&self, size: f32, weight: i32, align: DWRITE_TEXT_ALIGNMENT) -> Result<IDWriteTextFormat, String> {
        unsafe {
            let f = self
                .dwrite
                .CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT(weight), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))
                .map_err(err("font"))?;
            let _ = f.SetTextAlignment(align);
            Ok(f)
        }
    }

    fn draw_overlay(&mut self, o: &Overlay) -> Result<(), String> {
        unsafe {
            if self.target.is_none() {
                let surface: IDXGISurface = self.swap.GetBuffer(0).map_err(err("back buffer"))?;
                let props = D2D1_RENDER_TARGET_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    ..Default::default()
                };
                self.target = Some(self.d2d.CreateDxgiSurfaceRenderTarget(&surface, &props).map_err(err("Direct2D target"))?);
            }
            let t = self.target.clone().unwrap();
            let s = (GetDpiForWindow(self.hwnd).max(96) as f32) / 96.0;
            let (w, h) = (self.size.0 as f32, self.size.1 as f32);
            let brush = |r: f32, g: f32, b: f32, a: f32| -> Result<ID2D1SolidColorBrush, String> {
                t.CreateSolidColorBrush(&D2D1_COLOR_F { r, g, b, a }, None).map_err(err("brush"))
            };
            let (white, grey, panel) = (brush(1.0, 1.0, 1.0, 1.0)?, brush(0.72, 0.74, 0.78, 1.0)?, brush(0.06, 0.07, 0.09, 0.82)?);
            let wide = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
            t.BeginDraw();
            if let Some((title, detail)) = &o.placeholder {
                t.Clear(Some(&D2D1_COLOR_F { r: 0.06, g: 0.07, b: 0.09, a: 1.0 }));
                let big = self.text_format(26.0 * s, 600, DWRITE_TEXT_ALIGNMENT_CENTER)?;
                let small = self.text_format(15.0 * s, 400, DWRITE_TEXT_ALIGNMENT_CENTER)?;
                let mid = h / 2.0;
                let rect = D2D_RECT_F { left: 0.0, top: mid - 44.0 * s, right: w, bottom: mid };
                t.DrawText(&wide(title), &big, &rect, &white, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
                let rect = D2D_RECT_F { left: 20.0 * s, top: mid + 4.0 * s, right: w - 20.0 * s, bottom: mid + 80.0 * s };
                t.DrawText(&wide(detail), &small, &rect, &grey, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
            } else if let Some(text) = &o.banner {
                let format = self.text_format(14.0 * s, 500, DWRITE_TEXT_ALIGNMENT_CENTER)?;
                let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                let text = wide(text);
                // As wide as the text needs, up to the window.
                let max = (w - 40.0 * s).max(50.0);
                let mut metrics = DWRITE_TEXT_METRICS::default();
                if let Ok(layout) = self.dwrite.CreateTextLayout(&text, &format, max, 200.0) {
                    let _ = layout.GetMetrics(&mut metrics);
                }
                let (bw, bh) = ((metrics.width + 36.0 * s).min(w), (metrics.height + 16.0 * s).max(34.0 * s));
                let rect = D2D_RECT_F { left: (w - bw) / 2.0, top: 14.0 * s, right: (w + bw) / 2.0, bottom: 14.0 * s + bh };
                t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect, radiusX: 9.0 * s, radiusY: 9.0 * s }, &panel);
                t.DrawText(&text, &format, &rect, &white, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
            }
            if let Some(rows) = &o.graph
                && o.placeholder.is_none()
            {
                let format = self.text_format(12.5 * s, 500, DWRITE_TEXT_ALIGNMENT_LEADING)?;
                let line = brush(0.35, 0.78, 1.0, 1.0)?;
                let (pad, row_h, pw) = (10.0 * s, 46.0 * s, 330.0 * s);
                let (x0, y0) = (12.0 * s, if o.banner.is_some() { 64.0 * s } else { 12.0 * s });
                let rect = D2D_RECT_F { left: x0, top: y0, right: x0 + pw, bottom: y0 + pad * 2.0 + row_h * rows.len() as f32 };
                t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect, radiusX: 8.0 * s, radiusY: 8.0 * s }, &panel);
                for (i, (text, values)) in rows.iter().enumerate() {
                    let top = y0 + pad + row_h * i as f32;
                    let rect = D2D_RECT_F { left: x0 + pad, top, right: x0 + pw - pad, bottom: top + 18.0 * s };
                    t.DrawText(&wide(text), &format, &rect, &white, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
                    // The last minute, scaled to the row's own largest value.
                    let top_value = values.iter().copied().fold(0.0f32, f32::max).max(1e-3);
                    let (gx, gy, gw, gh) = (x0 + pad, top + 20.0 * s, pw - pad * 2.0, 20.0 * s);
                    let point = |n: usize, v: f32| Vector2 { X: gx + gw * n as f32 / (values.len().max(2) - 1) as f32, Y: gy + gh * (1.0 - (v / top_value).clamp(0.0, 1.0)) };
                    for (n, pair) in values.windows(2).enumerate() {
                        t.DrawLine(point(n, pair[0]), point(n + 1, pair[1]), &line, 1.2 * s, None);
                    }
                }
            }
            t.EndDraw(None, None).map_err(err("Direct2D"))
        }
    }
}

/// Picks NV12 from the output types the decoder offers.
fn set_nv12_output(transform: &IMFTransform) -> Result<(), String> {
    unsafe {
        for i in 0.. {
            let Ok(ty) = transform.GetOutputAvailableType(0, i) else { break };
            if ty.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|g: GUID| g == MFVideoFormat_NV12) {
                return transform.SetOutputType(0, &ty, 0).map_err(err("decoder output type"));
            }
        }
    }
    Err("the HEVC decoder offers no NV12 output (is this a 10-bit stream?)".into())
}

/// The part of the decoded picture that is meant to be seen.
fn output_size(transform: &IMFTransform) -> Result<(u32, u32), String> {
    unsafe {
        let ty = transform.GetOutputCurrentType(0).map_err(err("decoder output type"))?;
        let mut area = MFVideoArea::default();
        let bytes = std::slice::from_raw_parts_mut(&mut area as *mut _ as *mut u8, size_of::<MFVideoArea>());
        if ty.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None).is_ok() && area.Area.cx > 0 && area.Area.cy > 0 {
            return Ok((area.Area.cx as u32, area.Area.cy as u32));
        }
        let size = ty.GetUINT64(&MF_MT_FRAME_SIZE).map_err(err("decoder frame size"))?;
        Ok(((size >> 32) as u32, size as u32))
    }
}
