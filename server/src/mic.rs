//! The client's microphone: datagrams arrive on the control thread and are
//! queued for the platform's renderer, which plays them into a virtual audio
//! cable (see `windows::micout`).

use proto::stream::AUDIO_CHANNELS;

/// About 200 ms of audio: enough to ride out a stalled renderer, short enough not to matter.
#[cfg_attr(not(windows), allow(dead_code))]
const RING_FRAMES: usize = 9600;

pub struct Sink {
    producer: rtrb::Producer<i16>,
    last_seq: Option<u32>,
    /// Chunks and peak level since the last report, and when that was.
    chunks: u32,
    peak: i16,
    reported_ns: u64,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn channel() -> (Sink, rtrb::Consumer<i16>) {
    let (producer, consumer) = rtrb::RingBuffer::new(RING_FRAMES * AUDIO_CHANNELS);
    (Sink { producer, last_seq: None, chunks: 0, peak: 0, reported_ns: 0 }, consumer)
}

impl Sink {
    /// Queues one chunk of PCM16 (little endian, interleaved stereo). Repeated chunks, and ones
    /// that arrive a little late, are dropped; a long way back is a new connection starting over.
    pub fn push(&mut self, seq: u32, payload: &[u8]) {
        if let Some(prev) = self.last_seq
            && (-200..=0).contains(&(seq.wrapping_sub(prev) as i32))
        {
            return;
        }
        self.last_seq = Some(seq);
        self.chunks += 1;
        self.peak = payload.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b).saturating_abs()).fold(self.peak, i16::max);
        let now = timing::now_ns();
        if now - self.reported_ns >= 1_000_000_000 {
            if self.reported_ns != 0 {
                println!("mic: {} chunks in the last second, peak level {}%", self.chunks, self.peak as u32 * 100 / 32767);
            }
            (self.chunks, self.peak, self.reported_ns) = (0, 0, now);
        }
        if let Ok(chunk) = self.producer.write_chunk_uninit(payload.len() / 2) {
            chunk.fill_from_iter(payload.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)));
        }
    }
}
