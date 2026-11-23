//! The client's microphone, played into a virtual audio device such as BlackHole
//! (https://existential.audio/blackhole/): apps on this Mac then choose that device as their
//! microphone. The Mac has no virtual microphone of its own, so without one this is off.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};

/// Starts playing into the first output device whose name contains one of the `|`-separated
/// `names` (case-insensitive), in order. The stream lives for the rest of the process, on a
/// thread of its own: CoreAudio can take a while to open a device, and the server shouldn't wait.
pub fn start(names: &str, consumer: rtrb::Consumer<i16>) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let names = names.to_string();
    std::thread::Builder::new()
        .name("mic-out".into())
        .spawn(move || match open(&names, consumer) {
            Ok(stream) => {
                let _ = tx.send(Ok(()));
                // Keep it playing for good.
                let _stream = stream;
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(r) => r,
        // Still opening: it starts playing when it's ready.
        Err(_) => {
            println!("microphone: the output device is slow to open; carrying on without waiting");
            Ok(())
        }
    }
}

fn open(names: &str, mut consumer: rtrb::Consumer<i16>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let devices: Vec<(String, cpal::Device)> = host
        .output_devices()
        .map_err(|e| e.to_string())?
        .filter_map(|d| Some((d.description().ok()?.to_string(), d)))
        .collect();
    let wanted: Vec<String> = names.split('|').map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()).collect();
    let (name, device) = wanted
        .iter()
        .find_map(|w| devices.iter().find(|(n, _)| n.to_lowercase().contains(w.as_str())))
        .ok_or_else(|| format!("no output device named like {names:?}: install BlackHole 2ch (brew install blackhole-2ch), or pass --mic-device"))?;
    let channels = device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| c.min_sample_rate() <= AUDIO_RATE && c.max_sample_rate() >= AUDIO_RATE && c.channels() >= 1)
        .map(|c| c.channels())
        .min_by_key(|&c| (c < AUDIO_CHANNELS as u16, c))
        .ok_or_else(|| format!("{name} can't play {} kHz audio", AUDIO_RATE / 1000))?;
    let config = cpal::StreamConfig { channels, sample_rate: AUDIO_RATE, buffer_size: cpal::BufferSize::Default };
    let ch = channels as usize;
    let mut frame = [0i16; AUDIO_CHANNELS];
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for out in data.chunks_mut(ch) {
                    if consumer.slots() >= AUDIO_CHANNELS {
                        for s in &mut frame {
                            *s = consumer.pop().unwrap_or(0);
                        }
                    } else {
                        frame = [0; AUDIO_CHANNELS];
                    }
                    for (i, o) in out.iter_mut().enumerate() {
                        *o = frame.get(i).or(if ch == 1 { frame.first() } else { None }).map_or(0.0, |&s| s as f32 / 32768.0);
                    }
                }
            },
            |e| eprintln!("microphone output: {e}"),
            None,
        )
        .map_err(|e| format!("{name}: {e}"))?;
    stream.play().map_err(|e| format!("{name}: {e}"))?;
    println!("microphone: playing the client's into \"{name}\"");
    Ok(stream)
}
