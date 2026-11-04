//! Hardware HEVC encoding with a VideoToolbox compression session, for the Mac server: real time,
//! no frame reordering, low-latency rate control where the encoder has it, keyframes only when
//! asked for. Output is Annex B (start codes), with VPS/SPS/PPS in front of every keyframe, the
//! way the NVENC path sends it.

use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};

use super::{CFTypeRef, CMTime, Cf, OSStatus, check, dict, number};

#[repr(C)]
struct Opaque {
    _p: [u8; 0],
}

type EncodeCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CFTypeRef);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, i: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFDictionarySetValue(dict: CFTypeRef, key: CFTypeRef, value: CFTypeRef);
    fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, count: isize, callbacks: *const Opaque) -> CFTypeRef;
    static kCFTypeArrayCallBacks: Opaque;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFTypeRef;
    fn CMSampleBufferGetDataBuffer(sample: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetFormatDescription(sample: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sample: CFTypeRef, create: u8) -> CFTypeRef;
    fn CMBlockBufferGetDataLength(block: CFTypeRef) -> usize;
    fn CMBlockBufferCopyDataBytes(block: CFTypeRef, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        format: CFTypeRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
    fn CMTimeMake(value: i64, timescale: i32) -> CMTime;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferIOSurfacePropertiesKey: CFTypeRef;
    fn CVPixelBufferCreate(alloc: CFTypeRef, w: usize, h: usize, format: u32, attrs: CFTypeRef, out: *mut CFTypeRef) -> i32;
    fn CVPixelBufferLockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: CFTypeRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: CFTypeRef, plane: usize) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFTypeRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFTypeRef;
    static kVTCompressionPropertyKey_RealTime: CFTypeRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFTypeRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFTypeRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFTypeRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFTypeRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFTypeRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFTypeRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFTypeRef;
    static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFTypeRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFTypeRef;
    fn VTCompressionSessionCreate(
        alloc: CFTypeRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFTypeRef,
        source_attrs: CFTypeRef,
        compressed_alloc: CFTypeRef,
        callback: EncodeCallback,
        refcon: *mut c_void,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: CFTypeRef,
        image: CFTypeRef,
        pts: CMTime,
        duration: CMTime,
        frame_props: CFTypeRef,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: CFTypeRef, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: CFTypeRef);
    fn VTSessionSetProperty(session: CFTypeRef, key: CFTypeRef, value: CFTypeRef) -> OSStatus;
}

const CODEC_HEVC_ENCODE: u32 = u32::from_be_bytes(*b"hvc1");
/// Timestamps are in microseconds.
const TIMESCALE: i32 = 1_000_000;

/// One encoded access unit.
pub struct Encoded {
    /// Annex B; a keyframe starts with its VPS, SPS and PPS.
    pub data: Vec<u8>,
    pub keyframe: bool,
}

type Outputs = Arc<Mutex<Vec<Result<Encoded, String>>>>;

extern "C" fn on_encoded(refcon: *mut c_void, _frame: *mut c_void, status: OSStatus, _flags: u32, sample: CFTypeRef) {
    let out = unsafe { &*(refcon as *const Mutex<Vec<Result<Encoded, String>>>) };
    let result = if status != 0 {
        Err(format!("encode failed: OSStatus {status}"))
    } else if sample.is_null() {
        // Dropped by the encoder (it may skip frames to keep the rate): nothing to send.
        return;
    } else {
        unsafe { annex_b(sample) }
    };
    out.lock().unwrap().push(result);
}

/// The sample's length-prefixed NAL units as Annex B, with the parameter sets in front of keyframes.
unsafe fn annex_b(sample: CFTypeRef) -> Result<Encoded, String> {
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        let not_sync = !attachments.is_null()
            && CFArrayGetCount(attachments) > 0
            && CFDictionaryGetValue(CFArrayGetValueAtIndex(attachments, 0), kCMSampleAttachmentKey_NotSync) == kCFBooleanTrue;
        let keyframe = !not_sync;
        let mut data = Vec::new();
        if keyframe {
            let format = CMSampleBufferGetFormatDescription(sample);
            for i in 0..3 {
                let (mut p, mut size, mut count, mut header) = (ptr::null(), 0usize, 0usize, 0i32);
                check(
                    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, i, &mut p, &mut size, &mut count, &mut header),
                    "parameter set",
                )?;
                data.extend_from_slice(&[0, 0, 0, 1]);
                data.extend_from_slice(std::slice::from_raw_parts(p, size));
            }
        }
        let block = CMSampleBufferGetDataBuffer(sample);
        let len = CMBlockBufferGetDataLength(block);
        let mut buf = vec![0u8; len];
        check(CMBlockBufferCopyDataBytes(block, 0, len, buf.as_mut_ptr().cast()), "copy encoded data")?;
        // 4-byte big-endian lengths → start codes.
        let mut at = 0;
        while at + 4 <= buf.len() {
            let n = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
            let end = (at + 4 + n).min(buf.len());
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(&buf[at + 4..end]);
            at = end;
        }
        Ok(Encoded { data, keyframe })
    }
}

pub struct Encoder {
    session: CFTypeRef,
    outputs: Outputs,
    fps: u32,
    /// Whether the low-latency rate control could be had (it has to be asked for at creation).
    pub low_latency: bool,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Encoder, String> {
        // Low-latency rate control first; some encoders (or OS versions) refuse it for HEVC.
        Self::create(width, height, fps, bitrate_bps, true).or_else(|_| Self::create(width, height, fps, bitrate_bps, false))
    }

    fn create(width: u32, height: u32, fps: u32, bitrate_bps: u32, low_latency: bool) -> Result<Encoder, String> {
        let outputs: Outputs = Arc::default();
        unsafe {
            let spec = dict();
            CFDictionarySetValue(spec.as_ptr(), kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue);
            if low_latency {
                CFDictionarySetValue(spec.as_ptr(), kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kCFBooleanTrue);
            }
            let mut session = ptr::null();
            check(
                VTCompressionSessionCreate(
                    ptr::null(),
                    width as i32,
                    height as i32,
                    CODEC_HEVC_ENCODE,
                    spec.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    on_encoded,
                    Arc::as_ptr(&outputs) as *mut c_void,
                    &mut session,
                ),
                "VTCompressionSessionCreate",
            )?;
            let e = Encoder { session, outputs, fps: fps.max(1), low_latency };
            let set = |key, value, what| check(VTSessionSetProperty(e.session, key, value), what);
            set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, "RealTime")?;
            set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, "AllowFrameReordering")?;
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel, "ProfileLevel")?;
            // Keyframes only on request (a lost frame is repaired by asking for one).
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, number(i32::MAX).as_ptr(), "MaxKeyFrameInterval")?;
            set(kVTCompressionPropertyKey_ExpectedFrameRate, number(fps as i32).as_ptr(), "ExpectedFrameRate")?;
            // Not every OS version knows it; speed matters more than the last bit of quality.
            let _ = VTSessionSetProperty(e.session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue);
            e.set_bitrate(bitrate_bps);
            check(VTCompressionSessionPrepareToEncodeFrames(e.session), "PrepareToEncodeFrames")?;
            Ok(e)
        }
    }

    /// Target bitrate, with a one-second cap at the same rate so bursts stay short.
    pub fn set_bitrate(&self, bps: u32) {
        unsafe {
            let _ = VTSessionSetProperty(self.session, kVTCompressionPropertyKey_AverageBitRate, number(bps as i32).as_ptr());
            let limits = [number((bps / 8) as i32), number(1)];
            let values: [CFTypeRef; 2] = [limits[0].as_ptr(), limits[1].as_ptr()];
            let array = Cf::from_create(CFArrayCreate(ptr::null(), values.as_ptr(), 2, &kCFTypeArrayCallBacks));
            let _ = VTSessionSetProperty(self.session, kVTCompressionPropertyKey_DataRateLimits, array.as_ptr());
        }
    }

    /// Encodes one frame (a `CVPixelBuffer`, NV12) and waits for it. None if the encoder dropped it.
    ///
    /// # Safety
    /// `pixel_buffer` must be a valid `CVPixelBufferRef` for the duration of the call.
    pub unsafe fn encode(&self, pixel_buffer: *const c_void, pts_us: i64, keyframe: bool) -> Result<Option<Encoded>, String> {
        unsafe {
            let props = keyframe.then(|| {
                let d = dict();
                CFDictionarySetValue(d.as_ptr(), kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
                d
            });
            let pts = CMTimeMake(pts_us, TIMESCALE);
            let duration = CMTimeMake(TIMESCALE as i64 / self.fps as i64, TIMESCALE);
            let mut info = 0;
            check(
                VTCompressionSessionEncodeFrame(
                    self.session,
                    pixel_buffer,
                    pts,
                    duration,
                    props.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
                    ptr::null_mut(),
                    &mut info,
                ),
                "VTCompressionSessionEncodeFrame",
            )?;
            // Everything up to this frame: with no reordering that is this frame, now.
            check(VTCompressionSessionCompleteFrames(self.session, pts), "CompleteFrames")?;
        }
        self.outputs.lock().unwrap().pop().transpose()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            super::release(self.session);
        }
    }
}

/// An NV12 (video range) `CVPixelBuffer` backed by an IOSurface, for test pictures.
pub struct PixelBuffer(Cf);

impl PixelBuffer {
    pub fn nv12(width: u32, height: u32) -> Result<PixelBuffer, String> {
        unsafe {
            let attrs = dict();
            let iosurface = dict();
            CFDictionarySetValue(attrs.as_ptr(), kCVPixelBufferIOSurfacePropertiesKey, iosurface.as_ptr());
            let mut pb = ptr::null();
            let r = CVPixelBufferCreate(ptr::null(), width as usize, height as usize, super::PIXEL_FORMAT_NV12, attrs.as_ptr(), &mut pb);
            if r != 0 {
                return Err(format!("CVPixelBufferCreate failed: {r}"));
            }
            Ok(PixelBuffer(Cf::from_create(pb)))
        }
    }

    pub fn as_ptr(&self) -> *const c_void {
        self.0.as_ptr()
    }

    /// Fills the luma plane (`y(x, y)`) and chroma plane (`uv(x, y)` per 2×2 block).
    pub fn fill(&self, width: u32, height: u32, y: impl Fn(u32, u32) -> u8, uv: impl Fn(u32, u32) -> (u8, u8)) {
        unsafe {
            let pb = self.0.as_ptr();
            CVPixelBufferLockBaseAddress(pb, 0);
            let (luma, luma_stride) = (CVPixelBufferGetBaseAddressOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 0));
            for row in 0..height {
                let line = std::slice::from_raw_parts_mut(luma.add(row as usize * luma_stride), width as usize);
                for (col, px) in line.iter_mut().enumerate() {
                    *px = y(col as u32, row);
                }
            }
            let (chroma, chroma_stride) = (CVPixelBufferGetBaseAddressOfPlane(pb, 1), CVPixelBufferGetBytesPerRowOfPlane(pb, 1));
            for row in 0..height / 2 {
                let line = std::slice::from_raw_parts_mut(chroma.add(row as usize * chroma_stride), width as usize);
                for col in 0..width as usize / 2 {
                    let (u, v) = uv(col as u32, row);
                    (line[col * 2], line[col * 2 + 1]) = (u, v);
                }
            }
            CVPixelBufferUnlockBaseAddress(pb, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_moving_picture_that_decodes() {
        let (w, h) = (1280, 720);
        let enc = Encoder::new(w, h, 60, 20_000_000).expect("hardware HEVC encoder");
        let pb = PixelBuffer::nv12(w, h).unwrap();
        let mut frames = Vec::new();
        for i in 0..30u32 {
            pb.fill(w, h, |x, y| ((x + y + i * 8) % 256) as u8, |x, _| ((x * 2 % 256) as u8, 128));
            if let Some(f) = unsafe { enc.encode(pb.as_ptr(), i as i64 * 16_667, i == 0) }.unwrap() {
                frames.push(f);
            }
        }
        assert!(frames.len() >= 25, "{} of 30 frames came out", frames.len());
        assert!(frames[0].keyframe && frames[1..].iter().all(|f| !f.keyframe), "a keyframe first, then only on request");

        // Our own decoder takes the stream, as the client would.
        let sample = hevc::to_sample(&frames[0].data);
        let format = crate::Format::hevc(&sample.param_sets.expect("parameter sets on the keyframe")).unwrap();
        assert_eq!(format.dimensions(), (w as i32, h as i32));
        let decoded = Arc::new(Mutex::new(0));
        let count = decoded.clone();
        let session = crate::Session::new(
            format,
            Some(crate::PIXEL_FORMAT_NV12),
            Box::new(move |o| {
                assert_eq!(o.status, 0);
                *count.lock().unwrap() += 1;
            }),
        )
        .unwrap();
        for (i, f) in frames.iter().enumerate() {
            session.decode(i, &hevc::to_sample(&f.data).data, false).unwrap();
        }
        assert_eq!(*decoded.lock().unwrap(), frames.len());
    }
}
