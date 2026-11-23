//! ScreenCaptureKit: one display at its native pixel size into IOSurface-backed NV12 buffers
//! (what VideoToolbox encodes directly), and the system's audio. Frames arrive on a dispatch
//! queue; the stream thread takes the newest.

use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};

use super::ffi::{self, CFTypeRef, Cf};

const NV12: u32 = u32::from_be_bytes(*b"420v");

type Handler = Box<dyn Fn(CFTypeRef, SCStreamOutputType) + Send + Sync>;

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the ivar is plain Rust data.
    #[unsafe(super(NSObject))]
    #[name = "DisplayWireStreamOutput"]
    #[ivars = Handler]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            (self.ivars())(sample as *const CMSampleBuffer as CFTypeRef, kind);
        }
    }
);

impl Output {
    fn new(handler: Handler) -> Retained<Output> {
        let this = Output::alloc().set_ivars(handler);
        unsafe { msg_send![super(this), init] }
    }
}

/// Retained Objective-C objects handed from a completion handler to the waiting thread.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

fn error_text(e: *mut NSError) -> Option<String> {
    (!e.is_null()).then(|| unsafe { (*e).localizedDescription() }.to_string())
}

/// Runs an asynchronous ScreenCaptureKit call and waits for its completion handler.
fn wait_for(what: &str, call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), String> {
    let (tx, rx) = channel();
    let block = RcBlock::new(move |e: *mut NSError| {
        let _ = tx.send(error_text(e));
    });
    call(&block);
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(format!("{what}: {e}")),
        Err(_) => Err(format!("{what}: no answer from ScreenCaptureKit")),
    }
}

/// The SCDisplay for a display id.
fn sc_display(id: ffi::CGDirectDisplayID) -> Result<Retained<SCDisplay>, String> {
    let (tx, rx) = channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, e: *mut NSError| {
        let found = unsafe { content.as_ref() }.and_then(|c| unsafe { c.displays() }.iter().find(|d| unsafe { d.displayID() } == id));
        let _ = tx.send(SendBox(found.ok_or_else(|| error_text(e))));
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(SendBox(Ok(d))) => Ok(d),
        Ok(SendBox(Err(Some(e)))) => Err(format!(
            "{e} (allow DisplayWire, or the terminal running it, in System Settings › Privacy & Security › Screen & System Audio Recording)"
        )),
        Ok(SendBox(Err(None))) => Err(format!("display {id} isn't available to capture")),
        Err(_) => Err("no answer from ScreenCaptureKit".into()),
    }
}

/// A frame from the display: a retained CVPixelBuffer and when it arrived.
#[derive(Clone)]
pub struct Frame {
    pub buffer: Cf,
    pub at_ns: u64,
}

#[derive(Default)]
struct Latest {
    frame: Mutex<Option<Frame>>,
    ready: Condvar,
}

/// Whether a screen sample carries a new image (not an idle or blank one).
unsafe fn complete(sample: CFTypeRef) -> bool {
    unsafe {
        let attachments = ffi::CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || ffi::CFArrayGetCount(attachments) == 0 {
            return false;
        }
        let info = ffi::CFArrayGetValueAtIndex(attachments, 0);
        let key = SCStreamFrameInfoStatus as *const _ as CFTypeRef;
        let status = ffi::CFDictionaryGetValue(info, key);
        let mut v = -1i64;
        !status.is_null() && ffi::CFNumberGetValue(status, ffi::kCFNumberSInt64Type, (&raw mut v).cast()) != 0 && v == SCFrameStatus::Complete.0 as i64
    }
}

fn configuration(width: u32, height: u32, fps: u32, cursor: bool) -> Retained<SCStreamConfiguration> {
    unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(width as usize);
        c.setHeight(height as usize);
        c.setMinimumFrameInterval(CMTime { value: 1, timescale: fps.max(1) as i32, flags: CMTimeFlags::Valid, epoch: 0 });
        c.setPixelFormat(NV12);
        c.setShowsCursor(cursor);
        c.setQueueDepth(5);
        c
    }
}

pub struct Capture {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    latest: Arc<Latest>,
    pub display: ffi::CGDirectDisplayID,
    pub width: u32,
    pub height: u32,
    fps: u32,
    cursor: bool,
    /// The last frame taken, to encode again for a keyframe on a still screen.
    pub last: Option<Frame>,
}

// The stream is used from the thread that owns the Capture; ScreenCaptureKit objects are thread safe.
unsafe impl Send for Capture {}

impl Capture {
    pub fn new(display: ffi::CGDirectDisplayID, fps: u32, cursor: bool) -> Result<Capture, String> {
        let (width, height, _) = ffi::display_mode(display).ok_or_else(|| format!("display {display} has no mode"))?;
        let sc = sc_display(display)?;
        let latest = Arc::new(Latest::default());
        let l = latest.clone();
        let output = Output::new(Box::new(move |sample, kind| {
            if kind != SCStreamOutputType::Screen || !unsafe { complete(sample) } {
                return;
            }
            let Some(buffer) = Cf::retained(unsafe { ffi::CMSampleBufferGetImageBuffer(sample) }) else { return };
            *l.frame.lock().unwrap() = Some(Frame { buffer, at_ns: timing::now_ns() });
            l.ready.notify_one();
        }));
        let queue = DispatchQueue::new("displaywire.capture", None);
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &sc, &NSArray::new());
            let config = configuration(width, height, fps, cursor);
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue))
                .map_err(|e| format!("add stream output: {}", e.localizedDescription()))?;
            stream
        };
        wait_for("start capture", |b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) })?;
        Ok(Capture { stream, _output: output, _queue: queue, latest, display, width, height, fps, cursor, last: None })
    }

    /// Waits up to `timeout_ms` for a new image.
    pub fn next(&mut self, timeout_ms: u32) -> Option<Frame> {
        let mut slot = self.latest.frame.lock().unwrap();
        if slot.is_none() && timeout_ms > 0 {
            slot = self.latest.ready.wait_timeout(slot, Duration::from_millis(timeout_ms as u64)).unwrap().0;
        }
        let f = slot.take()?;
        self.last = Some(f.clone());
        Some(f)
    }

    /// The last image again, as if captured now (nothing on screen changed since).
    pub fn again(&self) -> Option<Frame> {
        self.last.as_ref().map(|f| Frame { buffer: f.buffer.clone(), at_ns: timing::now_ns() })
    }

    /// Follows a change of the display's resolution; true if the size changed.
    pub fn follow_mode(&mut self) -> Result<bool, String> {
        let Some((w, h, _)) = ffi::display_mode(self.display) else { return Ok(false) };
        if (w, h) == (self.width, self.height) {
            return Ok(false);
        }
        (self.width, self.height) = (w, h);
        self.reconfigure()?;
        self.last = None;
        Ok(true)
    }

    /// Draws the pointer into the video or leaves it out.
    pub fn set_cursor(&mut self, on: bool) -> Result<(), String> {
        if on == self.cursor {
            return Ok(());
        }
        self.cursor = on;
        self.reconfigure()
    }

    fn reconfigure(&self) -> Result<(), String> {
        let config = configuration(self.width, self.height, self.fps, self.cursor);
        wait_for("update capture", |b| unsafe { self.stream.updateConfiguration_completionHandler(&config, Some(b)) })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = wait_for("stop capture", |b| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(b)) });
    }
}

/// The system's audio, as 48 kHz stereo, from a ScreenCaptureKit stream that captures no video
/// worth speaking of. DisplayWire's own sounds are left out.
pub struct AudioCapture {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
}

unsafe impl Send for AudioCapture {}

impl AudioCapture {
    /// `on_audio` gets interleaved stereo f32 samples as they arrive.
    pub fn start(on_audio: impl Fn(&[f32]) + Send + Sync + 'static) -> Result<AudioCapture, String> {
        let sc = sc_display(unsafe { ffi::CGMainDisplayID() })?;
        let scratch = Mutex::new(Vec::<f32>::new());
        let output = Output::new(Box::new(move |sample, kind| {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            let mut v = scratch.lock().unwrap();
            v.clear();
            if unsafe { read_audio(sample, &mut v) } {
                on_audio(&v);
            }
        }));
        let queue = DispatchQueue::new("displaywire.audio", None);
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &sc, &NSArray::new());
            let config = configuration(2, 2, 1, false);
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(proto::stream::AUDIO_RATE as isize);
            config.setChannelCount(2);
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Audio, Some(&queue))
                .map_err(|e| format!("add audio output: {}", e.localizedDescription()))?;
            stream
        };
        wait_for("start audio capture", |b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) })?;
        Ok(AudioCapture { stream, _output: output, _queue: queue })
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        let _ = wait_for("stop audio capture", |b| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(b)) });
    }
}

/// An audio sample buffer → interleaved stereo f32 (mono is doubled, extra channels dropped).
unsafe fn read_audio(sample: CFTypeRef, out: &mut Vec<f32>) -> bool {
    unsafe {
        let desc = ffi::CMSampleBufferGetFormatDescription(sample);
        let asbd = ffi::CMAudioFormatDescriptionGetStreamBasicDescription(desc);
        let Some(asbd) = asbd.as_ref() else { return false };
        // Ask how big the list is (it depends on the channel count), then fill it.
        let mut needed = 0usize;
        ffi::CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample,
            &mut needed,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        );
        if needed == 0 || needed > 4096 {
            return false;
        }
        let mut storage = vec![0u64; needed.div_ceil(8)];
        let mut block = std::ptr::null();
        let status = ffi::CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample,
            std::ptr::null_mut(),
            storage.as_mut_ptr().cast(),
            needed,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut block,
        );
        if status != 0 {
            return false;
        }
        // Only `needed` bytes are there: read through raw pointers, never the whole struct.
        let list = storage.as_ptr() as *const ffi::AudioBufferList8;
        let _block = Cf::owned(block);
        let float = asbd.mFormatFlags & ffi::kAudioFormatFlagIsFloat != 0;
        let bits = asbd.mBitsPerChannel;
        if !(float && bits == 32 || !float && bits == 16) {
            return false;
        }
        let read = |buf: &ffi::AudioBuffer, i: usize| -> f32 {
            if float { *(buf.mData as *const f32).add(i) } else { *(buf.mData as *const i16).add(i) as f32 / 32768.0 }
        };
        let count = ((*list).mNumberBuffers as usize).min(needed.saturating_sub(8) / size_of::<ffi::AudioBuffer>());
        let bufs = std::slice::from_raw_parts((&raw const (*list).mBuffers).cast::<ffi::AudioBuffer>(), count);
        let bytes = (bits / 8) as usize;
        if asbd.mFormatFlags & ffi::kAudioFormatFlagIsNonInterleaved != 0 {
            let Some(left) = bufs.first() else { return false };
            let right = bufs.get(1).unwrap_or(left);
            let frames = left.mDataByteSize as usize / bytes;
            for i in 0..frames {
                out.push(read(left, i));
                out.push(read(right, i));
            }
        } else {
            let Some(b) = bufs.first() else { return false };
            let ch = (b.mNumberChannels as usize).max(1);
            let frames = b.mDataByteSize as usize / bytes / ch;
            for i in 0..frames {
                out.push(read(b, i * ch));
                out.push(read(b, i * ch + (ch > 1) as usize));
            }
        }
        true
    }
}
