//! Minimal VideoToolbox hardware decode over raw FFI (macOS only).

#![cfg(target_os = "macos")]
#![allow(non_upper_case_globals)]

use std::ffi::{c_char, c_void};
use std::ptr;

type CFTypeRef = *const c_void;
type OSStatus = i32;

pub const CODEC_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
pub const CODEC_AV1: u32 = u32::from_be_bytes(*b"av01");
/// NV12, video range: what CoreAnimation and Metal take directly.
pub const PIXEL_FORMAT_NV12: u32 = u32::from_be_bytes(*b"420v");

const DECODE_ASYNC: u32 = 1;
const CF_NUMBER_SINT32: isize = 3;
const CF_STRING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
struct CMVideoDimensions {
    width: i32,
    height: i32,
}

#[repr(C)]
struct Opaque {
    _p: [u8; 0],
}

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CFTypeRef, CMTime, CMTime);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    static kCFBooleanTrue: CFTypeRef;
    static kCFAllocatorNull: CFTypeRef;
    fn CFDictionaryCreateMutable(alloc: CFTypeRef, cap: isize, k: *const Opaque, v: *const Opaque) -> CFTypeRef;
    fn CFDictionarySetValue(dict: CFTypeRef, key: CFTypeRef, value: CFTypeRef);
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: isize) -> CFTypeRef;
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFNumberCreate(alloc: CFTypeRef, ty: isize, value: *const c_void) -> CFTypeRef;
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFTypeRef;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        alloc: CFTypeRef,
        count: usize,
        ptrs: *const *const u8,
        sizes: *const usize,
        nal_len_size: i32,
        extensions: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreate(
        alloc: CFTypeRef,
        codec: u32,
        width: i32,
        height: i32,
        extensions: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetDimensions(format: CFTypeRef) -> CMVideoDimensions;
    fn CMBlockBufferCreateWithMemoryBlock(
        alloc: CFTypeRef,
        mem: *mut c_void,
        block_len: usize,
        block_alloc: CFTypeRef,
        custom_source: *const c_void,
        offset: usize,
        data_len: usize,
        flags: u32,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        alloc: CFTypeRef,
        data: CFTypeRef,
        format: CFTypeRef,
        num_samples: isize,
        num_timing: isize,
        timing: *const c_void,
        num_sizes: isize,
        sizes: *const usize,
        out: *mut CFTypeRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferIOSurfacePropertiesKey: CFTypeRef;
    static kCVPixelBufferMetalCompatibilityKey: CFTypeRef;
    static kCVPixelBufferPixelFormatTypeKey: CFTypeRef;
    fn CVPixelBufferGetPixelFormatType(pb: CFTypeRef) -> u32;
    fn CVPixelBufferGetIOSurface(pb: CFTypeRef) -> CFTypeRef;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTDecompressionPropertyKey_RealTime: CFTypeRef;
    static kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: CFTypeRef;
    fn VTDecompressionSessionCreate(
        alloc: CFTypeRef,
        format: CFTypeRef,
        decoder_spec: CFTypeRef,
        image_attrs: CFTypeRef,
        callback: *const OutputCallbackRecord,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: CFTypeRef,
        sample: CFTypeRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: CFTypeRef) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: CFTypeRef);
    fn VTSessionSetProperty(session: CFTypeRef, key: CFTypeRef, value: CFTypeRef) -> OSStatus;
    fn VTIsHardwareDecodeSupported(codec: u32) -> u8;
    fn VTRegisterSupplementalVideoDecoderIfAvailable(codec: u32);
}

/// Owned (retained) CoreFoundation object.
pub struct Cf(CFTypeRef);

// CF objects used here are immutable after creation and thread-safe to retain/release.
unsafe impl Send for Cf {}
unsafe impl Sync for Cf {}

impl Cf {
    pub fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

impl Clone for Cf {
    fn clone(&self) -> Self {
        Cf(unsafe { CFRetain(self.0) })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn check(status: OSStatus, what: &str) -> Result<(), String> {
    if status == 0 { Ok(()) } else { Err(format!("{what} failed: OSStatus {status}")) }
}

fn dict() -> Cf {
    Cf(unsafe { CFDictionaryCreateMutable(ptr::null(), 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks) })
}

fn fourcc(f: u32) -> String {
    let b = f.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) { String::from_utf8_lossy(&b).into_owned() } else { format!("{f:#x}") }
}

pub fn hardware_decode_supported(codec: u32) -> bool {
    unsafe {
        VTRegisterSupplementalVideoDecoderIfAvailable(codec);
        VTIsHardwareDecodeSupported(codec) != 0
    }
}

/// A video format description.
#[derive(Clone)]
pub struct Format(Cf);

impl Format {
    /// From HEVC VPS, SPS and PPS (without start codes).
    pub fn hevc(param_sets: &[Vec<u8>]) -> Result<Format, String> {
        let ptrs: Vec<*const u8> = param_sets.iter().map(|p| p.as_ptr()).collect();
        let sizes: Vec<usize> = param_sets.iter().map(|p| p.len()).collect();
        let mut fmt = ptr::null();
        check(
            unsafe {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    ptr::null(),
                    ptrs.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    ptr::null(),
                    &mut fmt,
                )
            },
            "CMVideoFormatDescriptionCreateFromHEVCParameterSets",
        )?;
        Ok(Format(Cf(fmt)))
    }

    /// From an `av1C` configuration record.
    pub fn av1(width: i32, height: i32, av1c: &[u8]) -> Result<Format, String> {
        unsafe {
            let atoms = dict();
            let key = Cf(CFStringCreateWithCString(ptr::null(), c"av1C".as_ptr(), CF_STRING_UTF8));
            let data = Cf(CFDataCreate(ptr::null(), av1c.as_ptr(), av1c.len() as isize));
            CFDictionarySetValue(atoms.0, key.0, data.0);
            let ext = dict();
            CFDictionarySetValue(ext.0, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, atoms.0);
            let mut fmt = ptr::null();
            check(
                CMVideoFormatDescriptionCreate(ptr::null(), CODEC_AV1, width, height, ext.0, &mut fmt),
                "CMVideoFormatDescriptionCreate",
            )?;
            Ok(Format(Cf(fmt)))
        }
    }

    pub fn dimensions(&self) -> (i32, i32) {
        let d = unsafe { CMVideoFormatDescriptionGetDimensions(self.0.0) };
        (d.width, d.height)
    }
}

/// A decoded frame (retained CVPixelBuffer).
#[derive(Clone)]
pub struct Image(Cf);

impl Image {
    /// The backing IOSurface (owned by the pixel buffer; valid while `self` lives).
    pub fn iosurface(&self) -> *const c_void {
        unsafe { CVPixelBufferGetIOSurface(self.0.0) }
    }

    pub fn pixel_format(&self) -> String {
        fourcc(unsafe { CVPixelBufferGetPixelFormatType(self.0.0) })
    }
}

pub struct Output {
    /// The `index` passed to `decode`.
    pub index: usize,
    /// Non-zero OSStatus on failure.
    pub status: i32,
    pub image: Option<Image>,
}

pub type Handler = Box<dyn Fn(Output) + Send + Sync>;

extern "C" fn on_output(
    refcon: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    _flags: u32,
    image: CFTypeRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    let handler = unsafe { &*(refcon as *const Handler) };
    let image = (!image.is_null()).then(|| Image(Cf(unsafe { CFRetain(image) })));
    handler(Output { index: frame as usize, status, image });
}

pub struct Session {
    session: CFTypeRef,
    format: Format,
    handler: Box<Handler>,
}

unsafe impl Send for Session {}

impl Session {
    /// Hardware-only, real-time session with IOSurface-backed, Metal-compatible
    /// output. `pixel_format` forces an output format (e.g. `PIXEL_FORMAT_NV12`);
    /// `None` lets the decoder pick its native one. `handler` may run on a
    /// VideoToolbox thread.
    pub fn new(format: Format, pixel_format: Option<u32>, handler: Handler) -> Result<Session, String> {
        let handler = Box::new(handler);
        unsafe {
            let spec = dict();
            CFDictionarySetValue(spec.0, kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder, kCFBooleanTrue);
            let attrs = dict();
            let iosurface = dict();
            CFDictionarySetValue(attrs.0, kCVPixelBufferIOSurfacePropertiesKey, iosurface.0);
            CFDictionarySetValue(attrs.0, kCVPixelBufferMetalCompatibilityKey, kCFBooleanTrue);
            if let Some(pf) = pixel_format {
                let n = Cf(CFNumberCreate(ptr::null(), CF_NUMBER_SINT32, &(pf as i32) as *const i32 as *const c_void));
                CFDictionarySetValue(attrs.0, kCVPixelBufferPixelFormatTypeKey, n.0);
            }
            let record = OutputCallbackRecord { callback: on_output, refcon: &*handler as *const Handler as *mut c_void };
            let mut session = ptr::null();
            check(
                VTDecompressionSessionCreate(ptr::null(), format.0.0, spec.0, attrs.0, &record, &mut session),
                "VTDecompressionSessionCreate",
            )?;
            let s = Session { session, format, handler };
            check(
                VTSessionSetProperty(s.session, kVTDecompressionPropertyKey_RealTime, kCFBooleanTrue),
                "set RealTime",
            )?;
            Ok(s)
        }
    }

    pub fn format(&self) -> &Format {
        &self.format
    }

    /// Submits one length-prefixed sample. The bytes are wrapped without
    /// copying, so for asynchronous decodes `data` must outlive the frame
    /// (`finish` waits for that). Synchronous decodes complete before returning.
    pub fn decode(&self, index: usize, data: &[u8], asynchronous: bool) -> Result<(), String> {
        unsafe {
            let mut block = ptr::null();
            check(
                CMBlockBufferCreateWithMemoryBlock(
                    ptr::null(),
                    data.as_ptr() as *mut c_void,
                    data.len(),
                    kCFAllocatorNull,
                    ptr::null(),
                    0,
                    data.len(),
                    0,
                    &mut block,
                ),
                "CMBlockBufferCreateWithMemoryBlock",
            )?;
            let block = Cf(block);
            let mut sample = ptr::null();
            let size = data.len();
            check(
                CMSampleBufferCreateReady(ptr::null(), block.0, self.format.0.0, 1, 0, ptr::null(), 1, &size, &mut sample),
                "CMSampleBufferCreateReady",
            )?;
            let sample = Cf(sample);
            let flags = if asynchronous { DECODE_ASYNC } else { 0 };
            let mut info = 0;
            let r = check(
                VTDecompressionSessionDecodeFrame(self.session, sample.0, flags, index as *mut c_void, &mut info),
                "VTDecompressionSessionDecodeFrame",
            );
            if !asynchronous {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            }
            r
        }
    }

    pub fn finish(&self) {
        unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session) };
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            VTDecompressionSessionInvalidate(self.session);
            CFRelease(self.session);
        }
        // `handler` is dropped after the session can no longer call it.
        let _ = &self.handler;
    }
}
