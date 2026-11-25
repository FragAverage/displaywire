//! VideoToolbox hardware HEVC encode for the Mac server: real time, no B-frames, no periodic
//! keyframes (the client asks for one when it needs it), Annex B output with an access unit
//! delimiter and, on keyframes, the parameter sets, the same shape NVENC's output has.

use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::{
    CFArrayGetCount, CFArrayGetValueAtIndex, CFDictionaryGetValue, CFDictionarySetValue, CFTypeRef, CMTime, Cf, OSStatus, check,
    dict, kCFBooleanFalse, kCFBooleanTrue, number_f64, number_i32,
};

/// The coarsest quantiser allowed: about where desktop text stops looking smeared.
const MAX_QP: i32 = 30;
const CODEC_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
/// An access unit delimiter (any slice types): lets the stream be split into frames.
const AUD: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: crate::Opaque;
    fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, count: isize, callbacks: *const crate::Opaque) -> CFTypeRef;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFTypeRef;
    fn CMSampleBufferGetDataBuffer(sbuf: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetFormatDescription(sbuf: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: CFTypeRef, create: u8) -> CFTypeRef;
    fn CMBlockBufferGetDataLength(bb: CFTypeRef) -> usize;
    fn CMBlockBufferCopyDataBytes(bb: CFTypeRef, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        fd: CFTypeRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFTypeRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFTypeRef;
    static kVTCompressionPropertyKey_RealTime: CFTypeRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFTypeRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFTypeRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFTypeRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFTypeRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFTypeRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFTypeRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFTypeRef;
    static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFTypeRef;
    static kVTCompressionPropertyKey_MaximizePowerEfficiency: CFTypeRef;
    static kVTCompressionPropertyKey_MaxAllowedFrameQP: CFTypeRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFTypeRef;
    fn VTCompressionSessionCreate(
        alloc: CFTypeRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFTypeRef,
        source_attrs: CFTypeRef,
        data_alloc: CFTypeRef,
        callback: OutputCallback,
        refcon: *mut c_void,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: CFTypeRef,
        image: CFTypeRef,
        pts: CMTime,
        duration: CMTime,
        props: CFTypeRef,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: CFTypeRef, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: CFTypeRef);
    fn VTSessionSetProperty(session: CFTypeRef, key: CFTypeRef, value: CFTypeRef) -> OSStatus;
}

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CFTypeRef);

/// One encoded frame, Annex B.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

type Sent = Result<Option<EncodedFrame>, String>;

extern "C" fn on_output(refcon: *mut c_void, _frame: *mut c_void, status: OSStatus, _flags: u32, sample: CFTypeRef) {
    let tx = unsafe { &*(refcon as *const Sender<Sent>) };
    let out = if status != 0 {
        Err(format!("encode failed: OSStatus {status}"))
    } else if sample.is_null() {
        Ok(None) // dropped by the encoder
    } else {
        unsafe { annex_b(sample) }.map(Some)
    };
    let _ = tx.send(out);
}

/// Whether a sample is a keyframe: it has no "not sync" attachment.
unsafe fn is_sync(sample: CFTypeRef) -> bool {
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
            return true;
        }
        let first = CFArrayGetValueAtIndex(attachments, 0);
        let not_sync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
        not_sync.is_null() || not_sync == kCFBooleanFalse
    }
}

/// Length-prefixed NAL units → Annex B, with an AUD first and the parameter sets on keyframes.
unsafe fn annex_b(sample: CFTypeRef) -> Result<EncodedFrame, String> {
    unsafe {
        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return Err("encoded sample has no data".into());
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        check(CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr().cast()), "CMBlockBufferCopyDataBytes")?;
        let keyframe = is_sync(sample);
        let mut out = Vec::with_capacity(len + 256);
        out.extend_from_slice(&AUD);
        let mut nal_len = 4usize;
        if keyframe {
            let format = CMSampleBufferGetFormatDescription(sample);
            let (mut count, mut header) = (0usize, 0i32);
            check(
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, 0, ptr::null_mut(), ptr::null_mut(), &mut count, &mut header),
                "CMVideoFormatDescriptionGetHEVCParameterSetAtIndex",
            )?;
            if header > 0 {
                nal_len = header as usize;
            }
            for i in 0..count {
                let (mut p, mut n) = (ptr::null(), 0usize);
                check(
                    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, i, &mut p, &mut n, ptr::null_mut(), ptr::null_mut()),
                    "CMVideoFormatDescriptionGetHEVCParameterSetAtIndex",
                )?;
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(std::slice::from_raw_parts(p, n));
            }
        }
        let mut i = 0;
        while i + nal_len <= avcc.len() {
            let n = avcc[i..i + nal_len].iter().fold(0usize, |a, &b| a << 8 | b as usize);
            i += nal_len;
            let end = (i + n).min(avcc.len());
            // The encoder's own AUDs (if any) would come after ours.
            if end > i && (avcc[i] >> 1) & 0x3f != 35 {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(&avcc[i..end]);
            }
            i = end;
        }
        Ok(EncodedFrame { data: out, keyframe })
    }
}

/// A hardware HEVC encoder for frames of one size.
pub struct Encoder {
    session: CFTypeRef,
    tx: Box<Sender<Sent>>,
    rx: Receiver<Sent>,
    fps: u32,
    pub name: String,
}

// The session is used from one thread at a time; the callback only touches the channel.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `bitrate` in bits per second.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Encoder, String> {
        match Self::create(width, height, fps, bitrate, true) {
            Ok(e) => Ok(e),
            // Low-latency rate control isn't offered for HEVC on every Mac or macOS version.
            Err(_) => Self::create(width, height, fps, bitrate, false),
        }
    }

    fn create(width: u32, height: u32, fps: u32, bitrate: u32, low_latency: bool) -> Result<Encoder, String> {
        let (tx, rx) = channel();
        let tx = Box::new(tx);
        unsafe {
            let spec = dict();
            CFDictionarySetValue(spec.0, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue);
            if low_latency {
                CFDictionarySetValue(spec.0, kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kCFBooleanTrue);
            }
            let mut session = ptr::null();
            check(
                VTCompressionSessionCreate(
                    ptr::null(),
                    width as i32,
                    height as i32,
                    CODEC_HEVC,
                    spec.0,
                    ptr::null(),
                    ptr::null(),
                    on_output,
                    &*tx as *const Sender<Sent> as *mut c_void,
                    &mut session,
                ),
                "VTCompressionSessionCreate",
            )?;
            let name = if low_latency { "VideoToolbox (low-latency rate control)" } else { "VideoToolbox" };
            let mut e = Encoder { session, tx, rx, fps: fps.max(1), name: name.into() };
            let set = |key: CFTypeRef, value: CFTypeRef, what: &str| check(VTSessionSetProperty(session, key, value), what);
            set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, "RealTime")?;
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel, "ProfileLevel")?;
            set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, "AllowFrameReordering")?;
            // No periodic keyframes: the client asks for one after a loss it can't repair.
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, number_i32(i32::MAX).0, "MaxKeyFrameInterval")?;
            set(kVTCompressionPropertyKey_ExpectedFrameRate, number_i32(fps as i32).0, "ExpectedFrameRate")?;
            // Optional: older macOS versions don't know these.
            let _ = set(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue, "PrioritizeEncodingSpeedOverQuality");
            let _ = set(kVTCompressionPropertyKey_MaximizePowerEfficiency, kCFBooleanFalse, "MaximizePowerEfficiency");
            // Low-latency rate control keeps every frame near the per-frame average, so a keyframe
            // or a page of new text comes out blurry. A quality floor lets those frames run larger.
            if set(kVTCompressionPropertyKey_MaxAllowedFrameQP, number_i32(MAX_QP).0, "MaxAllowedFrameQP").is_ok() {
                e.name.push_str(&format!(", QP ≤ {MAX_QP}"));
            }
            e.set_bitrate(bitrate);
            check(VTCompressionSessionPrepareToEncodeFrames(session), "VTCompressionSessionPrepareToEncodeFrames")?;
            Ok(e)
        }
    }

    /// Bits per second: the average, with no second more than half as much again.
    pub fn set_bitrate(&mut self, bps: u32) {
        unsafe {
            let _ = VTSessionSetProperty(self.session, kVTCompressionPropertyKey_AverageBitRate, number_i32(bps as i32).0);
            let bytes = number_f64(bps as f64 * 1.5 / 8.0);
            let seconds = number_f64(1.0);
            let values = [bytes.0, seconds.0];
            let limits = Cf(CFArrayCreate(ptr::null(), values.as_ptr(), 2, &kCFTypeArrayCallBacks));
            let _ = VTSessionSetProperty(self.session, kVTCompressionPropertyKey_DataRateLimits, limits.0);
        }
    }

    /// Encodes `image` (a CVPixelBuffer) and waits for it. `time_ns` must increase.
    ///
    /// # Safety
    /// `image` must be a valid CVPixelBuffer of this encoder's size.
    pub unsafe fn encode(&mut self, image: *const c_void, time_ns: u64, keyframe: bool) -> Result<Option<EncodedFrame>, String> {
        let pts = CMTime { value: time_ns as i64, timescale: 1_000_000_000, flags: 1, epoch: 0 };
        let duration = CMTime { value: 1_000_000_000 / self.fps as i64, timescale: 1_000_000_000, flags: 1, epoch: 0 };
        unsafe {
            let props = keyframe.then(|| {
                let d = dict();
                CFDictionarySetValue(d.0, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
                d
            });
            let mut info = 0;
            check(
                VTCompressionSessionEncodeFrame(
                    self.session,
                    image,
                    pts,
                    duration,
                    props.as_ref().map_or(ptr::null(), |d| d.0),
                    ptr::null_mut(),
                    &mut info,
                ),
                "VTCompressionSessionEncodeFrame",
            )?;
            // Real time with no reordering: this returns as soon as this frame is out.
            check(VTCompressionSessionCompleteFrames(self.session, pts), "VTCompressionSessionCompleteFrames")?;
        }
        let mut last = Ok(None);
        while let Ok(r) = self.rx.try_recv() {
            last = r;
        }
        last
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            crate::CFRelease(self.session);
        }
        // `tx` outlives every callback.
        let _ = &self.tx;
    }
}
