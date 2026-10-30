//! Captures the Mac's microphone: CoreAudio → ring buffer → the receiver thread,
//! which sends it to the server in 5 ms chunks. Only open while the mic is on, so
//! macOS shows its recording indicator exactly then.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SupportedBufferSize};
use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};

/// Frames per datagram: 5 ms, as the server sends desktop audio.
pub const CHUNK_FRAMES: usize = 240;
pub const CHUNK_SAMPLES: usize = CHUNK_FRAMES * AUDIO_CHANNELS;
/// Device buffer requested: ~5 ms.
const DEVICE_FRAMES: u32 = 240;
/// Ring capacity: a quarter second.
const RING_FRAMES: usize = 12_000;

pub struct Capture {
    consumer: rtrb::Consumer<i16>,
    _stream: cpal::Stream,
}

impl Capture {
    pub fn new() -> Result<Capture, String> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("no microphone found")?;
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        let channels = supported.channels() as usize;
        let buffer = match supported.buffer_size() {
            SupportedBufferSize::Range { min, max } => BufferSize::Fixed(DEVICE_FRAMES.clamp(*min, *max)),
            _ => BufferSize::Default,
        };
        let name = device.description().map(|d| d.to_string()).unwrap_or_default();
        println!("mic: device {name:?}, {channels} channel(s) at {} Hz", supported.sample_rate());
        let config = cpal::StreamConfig { channels: channels as u16, sample_rate: AUDIO_RATE, buffer_size: buffer };
        let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(RING_FRAMES * AUDIO_CHANNELS);
        let callback = move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // Mono goes to both sides; more than two channels: the first two.
            let frames = data.len() / channels;
            let Ok(chunk) = producer.write_chunk_uninit(frames * AUDIO_CHANNELS) else { return };
            let sample = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            chunk.fill_from_iter(data.chunks_exact(channels).flat_map(|f| {
                let (l, r) = (sample(f[0]), sample(f[channels.min(2) - 1]));
                [l, r]
            }));
        };
        let stream = device
            .build_input_stream(config, callback, |e| eprintln!("mic input: {e}"), None)
            .map_err(|e| format!("can't open the microphone at 48 kHz: {e}"))?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Capture { consumer, _stream: stream })
    }

    /// The next whole 5 ms chunk, as little-endian bytes, if one has been captured.
    pub fn next_chunk(&mut self, out: &mut [u8; CHUNK_SAMPLES * 2]) -> bool {
        let Ok(chunk) = self.consumer.read_chunk(CHUNK_SAMPLES) else { return false };
        let (a, b) = chunk.as_slices();
        for (o, s) in out.as_chunks_mut::<2>().0.iter_mut().zip(a.iter().chain(b)) {
            o.copy_from_slice(&s.to_le_bytes());
        }
        chunk.commit_all();
        true
    }
}
