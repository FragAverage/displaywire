//! One display through ScreenCaptureKit: NV12 frames at the display's native pixel size, straight
//! into the encoder. ScreenCaptureKit sends a frame only when something changed; only the newest
//! one is kept, so a slow consumer never builds a queue.
//!
//! Needs the Screen Recording permission (System Settings › Privacy & Security), asked for the
//! first time; until it's granted, opening fails and says so.

use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};
use timing::now_ns;

use super::cg;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetImageBuffer(sample: *const c_void) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// A retained `CVPixelBuffer`.
pub struct Picture(*const c_void);

unsafe impl Send for Picture {}

impl Picture {
    pub fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

impl Clone for Picture {
    fn clone(&self) -> Picture {
        Picture(unsafe { CFRetain(self.0) })
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

/// The newest frame and when it arrived; `new` until taken.
#[derive(Default)]
struct Latest {
    frame: Option<(Picture, u64)>,
    new: bool,
}

type Shared = Arc<(Mutex<Latest>, Condvar)>;

pub struct OutputIvars {
    latest: Shared,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the ivars are Send + Sync.
    #[unsafe(super(NSObject))]
    #[name = "DisplayWireScreenOutput"]
    #[ivars = OutputIvars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle frames (nothing changed) carry no picture.
            let image = unsafe { CMSampleBufferGetImageBuffer(sample as *const CMSampleBuffer as *const c_void) };
            if image.is_null() {
                return;
            }
            let picture = Picture(unsafe { CFRetain(image) });
            let (lock, cv) = &*self.ivars().latest;
            *lock.lock().unwrap() = Latest { frame: Some((picture, now_ns())), new: true };
            cv.notify_one();
        }
    }
);

impl Output {
    fn new(latest: Shared) -> Retained<Output> {
        let this = Output::alloc().set_ivars(OutputIvars { latest });
        unsafe { msg_send![super(this), init] }
    }
}

/// The displays ScreenCaptureKit offers, in its order. Fails without the Screen Recording permission.
pub fn displays() -> Result<Retained<NSArray<SCDisplay>>, String> {
    if !cg::screen_capture_allowed() {
        cg::request_screen_capture();
        return Err("needs the Screen Recording permission: allow displaywire-server in System Settings › Privacy & \
                    Security › Screen Recording, then start it again"
            .into());
    }
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(unsafe { c.displays() }),
            None => Err(unsafe { Retained::retain(error) }.map_or("no shareable content".into(), |e| e.localizedDescription().to_string())),
        };
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(result);
        }
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "ScreenCaptureKit didn't answer".to_string())?
}

pub struct Capture {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    latest: Shared,
    pub display: u32,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Capture {}

impl Capture {
    /// Captures `display` (a `CGDirectDisplayID` from `displays`) at its native pixel size and up to
    /// `fps`, with or without the pointer drawn in.
    pub fn new(display: &SCDisplay, fps: u32, with_pointer: bool) -> Result<Capture, String> {
        let id = unsafe { display.displayID() };
        let (width, height, _) = cg::pixel_size(id);
        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(u32::from_be_bytes(*b"420v"));
            config.setMinimumFrameInterval(CMTime { value: 1, timescale: fps.max(1) as i32, flags: CMTimeFlags::Valid, epoch: 0 });
            config.setShowsCursor(with_pointer);
            // Few buffers: the newest frame is what matters.
            config.setQueueDepth(3);
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            let latest: Shared = Arc::default();
            let output = Output::new(latest.clone());
            let queue = DispatchQueue::new("displaywire.capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue))
                .map_err(|e| format!("add stream output: {}", e.localizedDescription()))?;
            let (tx, rx) = mpsc::channel();
            let tx = Mutex::new(Some(tx));
            let started = RcBlock::new(move |error: *mut NSError| {
                let r = Retained::retain(error).map_or(Ok(()), |e| Err(e.localizedDescription().to_string()));
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(r);
                }
            });
            stream.startCaptureWithCompletionHandler(Some(&started));
            rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "capture didn't start".to_string())??;
            println!("capturing display {id} ({width}x{height})");
            Ok(Capture { stream, _output: output, _queue: queue, latest, display: id, width, height })
        }
    }

    /// The next new frame, waiting up to `timeout_ms`.
    pub fn next(&self, timeout_ms: u32) -> Option<(Picture, u64)> {
        let (lock, cv) = &*self.latest;
        let guard = lock.lock().unwrap();
        let (mut latest, _) = cv.wait_timeout_while(guard, Duration::from_millis(timeout_ms as u64), |l| !l.new).unwrap();
        if !latest.new {
            return None;
        }
        latest.new = false;
        latest.frame.clone()
    }

    /// The last frame, new or not (to encode again for a keyframe on a still screen).
    pub fn last(&self) -> Option<(Picture, u64)> {
        self.latest.0.lock().unwrap().frame.clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}
