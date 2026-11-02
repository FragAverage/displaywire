//! Captures the Mac's microphone: CoreAudio → ring buffer → the receiver thread,
//! which sends it to the server in 5 ms chunks. Only open while the mic is on, so
//! macOS shows its recording indicator exactly then.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SupportedBufferSize};
use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};

use crate::resample::Resampler;

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

/// Opens the input stream at the given rate; the ring always gets 48 kHz stereo.
fn open(device: &cpal::Device, rate: u32, channels: usize, buffer: BufferSize) -> Result<(rtrb::Consumer<i16>, cpal::Stream), String> {
    let config = cpal::StreamConfig { channels: channels as u16, sample_rate: rate, buffer_size: buffer };
    let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(RING_FRAMES * AUDIO_CHANNELS);
    let mut resampler = (rate != AUDIO_RATE).then(|| Resampler::new(rate, AUDIO_RATE));
    let callback = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        let sample = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        // Mono goes to both sides; more than two channels: the first two.
        let frames = data.chunks_exact(channels).map(|f| [f[0], f[channels.min(2) - 1]]);
        match &mut resampler {
            None => {
                let Ok(chunk) = producer.write_chunk_uninit(data.len() / channels * AUDIO_CHANNELS) else { return };
                chunk.fill_from_iter(frames.flat_map(|[l, r]| [sample(l), sample(r)]));
            }
            Some(resampler) => {
                for f in frames {
                    resampler.push(f, |[l, r]| {
                        if producer.slots() >= AUDIO_CHANNELS {
                            let _ = producer.push(sample(l));
                            let _ = producer.push(sample(r));
                        }
                    });
                }
            }
        }
    };
    let stream = device
        .build_input_stream(config, callback, |e| eprintln!("mic input: {e}"), None)
        .map_err(|e| format!("can't open the microphone at {} kHz: {e}", rate as f32 / 1000.0))?;
    Ok((consumer, stream))
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
        // 48 kHz when the device can; otherwise its own rate, converted (a Bluetooth headset in hands-free mode is
        // 16 kHz only). Some devices also refuse a small fixed buffer: let them choose then.
        let mut rates = vec![AUDIO_RATE];
        if supported.sample_rate() != AUDIO_RATE {
            rates.push(supported.sample_rate());
        }
        let buffers = if buffer == BufferSize::Default { vec![buffer] } else { vec![buffer, BufferSize::Default] };
        let mut error = String::new();
        for &rate in &rates {
            for buffer in &buffers {
                match open(&device, rate, channels, *buffer) {
                    Ok((consumer, stream)) => {
                        if rate != AUDIO_RATE {
                            println!("mic: the device runs at {rate} Hz; converting");
                        }
                        stream.play().map_err(|e| e.to_string())?;
                        return Ok(Capture { consumer, _stream: stream });
                    }
                    Err(e) => {
                        println!("mic: {e} (buffer {buffer:?})");
                        error = e;
                    }
                }
            }
        }
        Err(error)
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
