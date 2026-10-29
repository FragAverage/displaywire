//! Desktop audio: a capture thread that sends 5 ms chunks of 48 kHz stereo
//! PCM16 to the connected client, if it asked for audio.

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE, Audio, AudioFormat, CAP_AUDIO, Msg};
use timing::{now_ns, sleep_until};

use crate::Shared;

/// Frames per datagram: 5 ms.
pub const CHUNK_FRAMES: usize = 240;
pub const CHUNK_SAMPLES: usize = CHUNK_FRAMES * AUDIO_CHANNELS;
const CHUNK_NS: u64 = CHUNK_FRAMES as u64 * 1_000_000_000 / AUDIO_RATE as u64;

pub trait AudioSource: Send {
    /// Called once on the capture thread before the first chunk.
    fn on_thread_start(&mut self) {}

    /// Blocks for the next chunk and returns the server time its first sample
    /// was captured. `None` means nothing to send right now (e.g. silence on
    /// a loopback device).
    fn next_chunk(&mut self, out: &mut [i16; CHUNK_SAMPLES]) -> Option<u64>;
}

/// Left 440 Hz, right 660 Hz: checks both channels and their order.
pub struct Tone {
    phase: u64,
    next_ns: u64,
}

impl Tone {
    pub fn new() -> Tone {
        Tone { phase: 0, next_ns: now_ns() }
    }
}

impl AudioSource for Tone {
    fn next_chunk(&mut self, out: &mut [i16; CHUNK_SAMPLES]) -> Option<u64> {
        sleep_until(self.next_ns);
        let at = self.next_ns;
        self.next_ns += CHUNK_NS;
        for frame in out.chunks_mut(AUDIO_CHANNELS) {
            let t = self.phase as f64 / AUDIO_RATE as f64;
            frame[0] = ((t * 440.0 * std::f64::consts::TAU).sin() * 6000.0) as i16;
            frame[1] = ((t * 660.0 * std::f64::consts::TAU).sin() * 6000.0) as i16;
            self.phase += 1;
        }
        Some(at)
    }
}

/// Starts the capture thread.
pub fn spawn(sock: UdpSocket, shared: Arc<Shared>, mut source: Box<dyn AudioSource>) {
    let _ = std::thread::Builder::new().name("audio".into()).spawn(move || {
        let mut samples = [0i16; CHUNK_SAMPLES];
        let mut bytes = [0u8; CHUNK_SAMPLES * 2];
        let mut out = [0u8; proto::MAX_DATAGRAM];
        let mut seq = 0u32;
        source.on_thread_start();
        loop {
            let Some(capture_ns) = source.next_chunk(&mut samples) else { continue };
            let peer = *shared.peer.lock().unwrap();
            let Some(peer) = peer.filter(|_| shared.caps.load(Relaxed) & CAP_AUDIO != 0) else { continue };
            for (b, s) in bytes.chunks_mut(2).zip(samples) {
                b.copy_from_slice(&s.to_le_bytes());
            }
            let n = Msg::Audio(Audio { seq, format: AudioFormat::Pcm16, capture_ns }, &bytes).encode(&mut out);
            let _ = sock.send_to(&out[..n], peer);
            seq = seq.wrapping_add(1);
        }
    });
}
