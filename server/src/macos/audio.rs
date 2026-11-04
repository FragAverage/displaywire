//! The Mac's sound, every app's (not ours), through ScreenCaptureKit: an audio-only capture of
//! the main display, converted to the 48 kHz stereo 16-bit chunks the stream sends.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{SCContentFilter, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType};
use proto::stream::AUDIO_RATE;
use timing::now_ns;

use super::{capture, cg};
use crate::audio::{AudioSource, CHUNK_SAMPLES};

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    bytes: u32,
    data: *mut c_void,
}

/// `AudioBufferList` with room for two buffers (planar stereo).
#[repr(C)]
struct AudioBufferList2 {
    count: u32,
    buffers: [AudioBuffer; 2],
}

#[repr(C)]
struct StreamDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits_per_channel: u32,
    reserved: u32,
}

const FLAG_FLOAT: u32 = 1;
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetFormatDescription(sample: *const c_void) -> *const c_void;
    fn CMAudioFormatDescriptionGetStreamBasicDescription(format: *const c_void) -> *const StreamDescription;
    fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sample: *const c_void,
        size_needed: *mut usize,
        list: *mut AudioBufferList2,
        list_size: usize,
        structure_alloc: *const c_void,
        block_alloc: *const c_void,
        flags: u32,
        block_out: *mut *const c_void,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// Interleaved 16-bit samples captured so far, and when the first of them was.
#[derive(Default)]
struct Pending {
    samples: VecDeque<i16>,
    front_ns: u64,
}

type Shared = Arc<(Mutex<Pending>, Condvar)>;

pub struct OutputIvars {
    pending: Shared,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the ivars are Send + Sync.
    #[unsafe(super(NSObject))]
    #[name = "DisplayWireAudioOutput"]
    #[ivars = OutputIvars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            let samples = unsafe { to_i16_stereo(sample as *const CMSampleBuffer as *const c_void) };
            if samples.is_empty() {
                return;
            }
            let (lock, cv) = &*self.ivars().pending;
            let mut p = lock.lock().unwrap();
            if p.samples.is_empty() {
                p.front_ns = now_ns();
            }
            p.samples.extend(samples);
            // Don't let it pile up if nothing is taking it: keep about half a second.
            let excess = p.samples.len().saturating_sub(AUDIO_RATE as usize);
            p.samples.drain(..excess);
            cv.notify_one();
        }
    }
);

/// The sample buffer's audio as interleaved 16-bit stereo (float or integer, planar or not).
unsafe fn to_i16_stereo(sample: *const c_void) -> Vec<i16> {
    unsafe {
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(CMSampleBufferGetFormatDescription(sample));
        if asbd.is_null() {
            return Vec::new();
        }
        let (float, planar, channels) = ((*asbd).format_flags & FLAG_FLOAT != 0, (*asbd).format_flags & FLAG_NON_INTERLEAVED != 0, (*asbd).channels.max(1) as usize);
        let mut list = AudioBufferList2 { count: 0, buffers: [AudioBuffer { channels: 0, bytes: 0, data: std::ptr::null_mut() }, AudioBuffer { channels: 0, bytes: 0, data: std::ptr::null_mut() }] };
        let mut block = std::ptr::null();
        let r = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sample, std::ptr::null_mut(), &mut list, size_of::<AudioBufferList2>(), std::ptr::null(), std::ptr::null(), 0, &mut block);
        if r != 0 {
            return Vec::new();
        }
        let value = |buf: &AudioBuffer, i: usize| -> f32 {
            if float {
                *(buf.data as *const f32).add(i)
            } else {
                *(buf.data as *const i16).add(i) as f32 / 32768.0
            }
        };
        let width = if float { 4 } else { 2 };
        let mut out = Vec::new();
        if planar && list.count >= 1 {
            let frames = list.buffers[0].bytes as usize / width;
            let right = if list.count >= 2 { 1 } else { 0 };
            out.reserve(frames * 2);
            for i in 0..frames {
                out.push(to_i16(value(&list.buffers[0], i)));
                out.push(to_i16(value(&list.buffers[right], i)));
            }
        } else if list.count >= 1 {
            let frames = list.buffers[0].bytes as usize / width / channels;
            for i in 0..frames {
                let left = value(&list.buffers[0], i * channels);
                let right = if channels > 1 { value(&list.buffers[0], i * channels + 1) } else { left };
                out.push(to_i16(left));
                out.push(to_i16(right));
            }
        }
        if !block.is_null() {
            CFRelease(block);
        }
        out
    }
}

fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

pub struct Audio {
    _stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    pending: Shared,
}

unsafe impl Send for Audio {}

impl Audio {
    pub fn open() -> Result<Audio, String> {
        let displays = capture::displays()?;
        let main = cg::main_display();
        let display = displays.iter().find(|d| unsafe { d.displayID() } == main).or_else(|| displays.iter().next()).ok_or("no display")?;
        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setSampleRate(AUDIO_RATE as isize);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
            // Video it has to have; as little as possible.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime { value: 1, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 });
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            let pending: Shared = Arc::default();
            let output = Output::alloc().set_ivars(OutputIvars { pending: pending.clone() });
            let output: Retained<Output> = msg_send![super(output), init];
            let queue = DispatchQueue::new("displaywire.audio", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Audio, Some(&queue))
                .map_err(|e| format!("add audio output: {}", e.localizedDescription()))?;
            capture::start(&stream, "audio capture")?;
            println!("audio: capturing every app's sound (ScreenCaptureKit)");
            Ok(Audio { _stream: stream, _output: output, _queue: queue, pending })
        }
    }
}

impl AudioSource for Audio {
    fn next_chunk(&mut self, out: &mut [i16; CHUNK_SAMPLES]) -> Option<u64> {
        let (lock, cv) = &*self.pending;
        let guard = lock.lock().unwrap();
        // Nothing comes while the Mac is silent: say so after a short wait.
        let (mut p, _) = cv.wait_timeout_while(guard, Duration::from_millis(20), |p| p.samples.len() < CHUNK_SAMPLES).unwrap();
        if p.samples.len() < CHUNK_SAMPLES {
            return None;
        }
        for (o, s) in out.iter_mut().zip(p.samples.drain(..CHUNK_SAMPLES)) {
            *o = s;
        }
        let at = p.front_ns;
        p.front_ns += (CHUNK_SAMPLES / 2) as u64 * 1_000_000_000 / AUDIO_RATE as u64;
        Some(at)
    }
}
