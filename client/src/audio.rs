//! Plays the server's desktop audio: network → ring buffer → CoreAudio.
//!
//! The ring is kept as short as the network allows. Playback starts once a
//! cushion (`prefill`) has arrived, and again after an underrun. Every second
//! the callback looks at the lowest fill it saw: whatever was never needed is
//! dropped (the server's clock can run ahead of the DAC's, and bursts of
//! packets pile up), while an underrun raises the cushion a little.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SupportedBufferSize};
use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};
use timing::{Samples, now_ns};

const CH: usize = AUDIO_CHANNELS;
const MS: usize = AUDIO_RATE as usize / 1000;
/// Cushion before playback starts, to begin with / at least / at most.
const PREFILL_START: usize = 10 * MS;
const PREFILL_MIN: usize = 5 * MS;
const PREFILL_MAX: usize = 160 * MS;
/// How often the unused backlog is shed: 1 s of playback. Shorter windows (250 ms) shed audio the
/// player then turns out to need, and the cushion climbs instead.
const WINDOW: usize = AUDIO_RATE as usize;
/// Kept on top of the cushion before the unused part is dropped.
const SLACK: usize = 5 * MS;
/// A burst beyond this is dropped at once.
const HARD_MAX: usize = 100 * MS;
/// Ring capacity.
const RING_FRAMES: usize = 12_000;
/// No packet for this long: the server is idle (silence), not late.
const IDLE_NS: u64 = 60_000_000;
/// Device buffer requested: ~2.7 ms.
const DEVICE_FRAMES: u32 = 128;

#[derive(Default)]
struct Counters {
    /// Frames queued when the device last asked for audio.
    fill: AtomicU32,
    underruns: AtomicU32,
    /// Output gain (an `f32` as bits): volume and mute.
    gain: AtomicU32,
    /// Current cushion (`prefill`), in frames.
    cushion: AtomicU32,
    /// Frames per callback, as the device really asks for them.
    block: AtomicU32,
    /// `now_ns` of the last packet, to tell lateness from silence.
    last_packet_ns: AtomicU64,
    /// Frames discarded: backlog trimmed, or the ring was full.
    dropped: AtomicU32,
}

pub struct Player {
    producer: rtrb::Producer<i16>,
    _stream: cpal::Stream,
    counters: Arc<Counters>,
    last_seq: Option<u32>,
    lost_chunks: u32,
    last_report: (u32, u32, u32),
    /// Server capture → arrival here, per chunk, in the last second.
    age: Samples,
    /// Smoothed arrival age, in ms, for the graph.
    age_ms: f32,
    /// Loudest sample received since the last stats line.
    peak: i16,
}

impl Player {
    pub fn new() -> Result<Player, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let buffer = match supported.buffer_size() {
            SupportedBufferSize::Range { min, max } => BufferSize::Fixed(DEVICE_FRAMES.clamp(*min, *max)),
            _ => BufferSize::Default,
        };
        println!("audio: device {:?}, buffer sizes {:?}, asking for {:?}", device.description().map(|d| d.to_string()).unwrap_or_default(), supported.buffer_size(), buffer);
        let config = cpal::StreamConfig { channels: CH as u16, sample_rate: AUDIO_RATE, buffer_size: buffer };
        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(RING_FRAMES * CH);
        let counters = Arc::new(Counters::default());
        counters.gain.store(1.0f32.to_bits(), Relaxed);
        let c = counters.clone();
        let mut playing = false;
        let mut prefill = PREFILL_START;
        let (mut win_frames, mut win_min, mut win_ok, mut calm) = (0usize, usize::MAX, true, 0u32);
        let callback = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut avail = consumer.slots() / CH;
            let drop_frames = |consumer: &mut rtrb::Consumer<i16>, frames: usize| {
                if frames > 0 && let Ok(chunk) = consumer.read_chunk(frames * CH) {
                    chunk.commit_all();
                    c.dropped.fetch_add(frames as u32, Relaxed);
                }
            };
            if avail > HARD_MAX {
                drop_frames(&mut consumer, avail - prefill);
                avail = prefill;
            }
            c.fill.store(avail as u32, Relaxed);
            c.cushion.store(prefill as u32, Relaxed);
            c.block.store((data.len() / CH) as u32, Relaxed);
            let idle = now_ns().saturating_sub(c.last_packet_ns.load(Relaxed)) > IDLE_NS;
            if !playing && avail >= prefill {
                playing = true;
                (win_frames, win_min, win_ok) = (0, usize::MAX, true);
            }
            if playing {
                win_min = win_min.min(avail);
                win_frames += data.len() / CH;
            }
            let take = if playing { (avail * CH).min(data.len()) } else { 0 };
            if take > 0 && let Ok(chunk) = consumer.read_chunk(take) {
                let (a, b) = chunk.as_slices();
                let gain = f32::from_bits(c.gain.load(Relaxed)) / 32768.0;
                for (o, s) in data.iter_mut().zip(a.iter().chain(b)) {
                    *o = *s as f32 * gain;
                }
                chunk.commit_all();
            }
            data[take..].fill(0.0);
            if playing && take < data.len() {
                playing = false;
                if !idle {
                    c.underruns.fetch_add(1, Relaxed);
                    win_ok = false;
                    prefill = (prefill + 6 * MS).min(PREFILL_MAX);
                    calm = 0;
                }
            }
            if playing && win_frames >= WINDOW {
                // A window without an underrun: shed what was never used.
                if win_ok {
                    let excess = win_min.saturating_sub(prefill + SLACK);
                    drop_frames(&mut consumer, excess);
                    calm += 1;
                    if calm >= 2 {
                        prefill = prefill.saturating_sub(2 * MS).max(PREFILL_MIN);
                        calm = 0;
                    }
                }
                (win_frames, win_min, win_ok) = (0, usize::MAX, true);
            }
        };
        let stream = device
            .build_output_stream(config, callback, |e| eprintln!("audio output: {e}"), None)
            .map_err(|e| format!("can't open the output device at 48 kHz stereo: {e}"))?;
        if supported.sample_format() != SampleFormat::F32 {
            // cpal converts for us; nothing to do, but worth knowing.
            println!("audio: device format is {}", supported.sample_format());
        }
        stream.play().map_err(|e| e.to_string())?;
        println!("audio: playing on the default output");
        Ok(Player { producer, _stream: stream, counters, last_seq: None, lost_chunks: 0, last_report: (0, 0, 0), age: Samples::default(), age_ms: 0.0, peak: 0 })
    }

    /// Volume and mute: a linear gain, 1.0 for unchanged.
    pub fn set_gain(&self, gain: f32) {
        self.counters.gain.store(gain.to_bits(), Relaxed);
    }

    /// Queues one chunk of PCM16 (little endian, interleaved stereo).
    /// `age_ns` is how old it was on arrival, once the clocks are synced.
    pub fn push(&mut self, seq: u32, payload: &[u8], age_ns: Option<i64>) {
        if let Some(a) = age_ns {
            self.age.push(a);
            self.age_ms += (a as f32 / 1e6 - self.age_ms) * 0.05;
        }
        if let Some(prev) = self.last_seq {
            self.lost_chunks += seq.wrapping_sub(prev).wrapping_sub(1).min(1000);
        }
        self.last_seq = Some(seq);
        self.counters.last_packet_ns.store(now_ns(), Relaxed);
        let samples = payload.len() / 2;
        self.peak = payload.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b).saturating_abs()).fold(self.peak, i16::max);
        match self.producer.write_chunk_uninit(samples) {
            Ok(chunk) => {
                chunk.fill_from_iter(payload.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)));
            }
            Err(_) => {
                self.counters.dropped.fetch_add((samples / CH) as u32, Relaxed);
            }
        }
    }

    /// Total audio delay and the target cushion, in ms, or `None` while nothing is playing.
    pub fn delay(&self) -> Option<(f32, f32)> {
        if now_ns().saturating_sub(self.counters.last_packet_ns.load(Relaxed)) > IDLE_NS {
            return None;
        }
        let ms = |frames: u32| frames as f32 * 1e3 / AUDIO_RATE as f32;
        Some((self.age_ms + ms(self.counters.fill.load(Relaxed)), ms(self.counters.cushion.load(Relaxed))))
    }

    /// For the once-a-second stats line: queued audio and what went wrong since the last call.
    pub fn summary(&mut self) -> String {
        let (under, dropped, lost) = (self.counters.underruns.load(Relaxed), self.counters.dropped.load(Relaxed), self.lost_chunks);
        let (pu, pd, pl) = std::mem::replace(&mut self.last_report, (under, dropped, lost));
        let queued = self.counters.fill.load(Relaxed) as f64 * 1e3 / AUDIO_RATE as f64;
        let age = timing::ms(self.age.summary().p50);
        self.age.clear();
        let cushion = self.counters.cushion.load(Relaxed) as f64 * 1e3 / AUDIO_RATE as f64;
        let mut s = format!("audio {:.0} ms (arrived {age:.0} + queued {queued:.0}, cushion {cushion:.0}, level {}%)", age + queued, self.peak as u32 * 100 / 32767);
        self.peak = 0;
        if under != pu || dropped != pd || lost != pl {
            s += &format!(" (underruns {} trimmed {} lost {})", under - pu, (dropped - pd) / (AUDIO_RATE / 1000), lost - pl);
        }
        s
    }
}
