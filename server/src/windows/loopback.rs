//! WASAPI loopback: captures what the default output device is playing, as
//! 48 kHz stereo PCM16 (the audio engine converts from the device's format).

use std::collections::VecDeque;

use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eConsole,
    eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::core::w;

use super::{Context, Res};
use crate::audio::{AudioSource, CHUNK_FRAMES, CHUNK_SAMPLES};

/// 100 ns units: the engine's buffer. Loopback delivers in engine periods
/// (about 10 ms) regardless.
const BUFFER_100NS: i64 = 200_000;
const ONE_FRAME_NS: u64 = 1_000_000_000 / AUDIO_RATE as u64;

pub struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    /// Captured samples not yet sent: whole chunks are taken from the front.
    pending: VecDeque<i16>,
    /// Server time of the front of `pending`.
    front_ns: u64,
}

// COM interfaces here are agile and used from the audio thread only.
unsafe impl Send for Loopback {}

impl Loopback {
    pub fn open() -> Res<Loopback> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ctx("MMDeviceEnumerator")?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ctx("no default audio output")?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ctx("activate audio client")?;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: AUDIO_CHANNELS as u16,
                nSamplesPerSec: AUDIO_RATE,
                nAvgBytesPerSec: AUDIO_RATE * AUDIO_CHANNELS as u32 * 2,
                nBlockAlign: AUDIO_CHANNELS as u16 * 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER_100NS,
                    0,
                    &format,
                    None,
                )
                .ctx("initialize loopback")?;
            let event = CreateEventW(None, false, false, None).ctx("create event")?;
            client.SetEventHandle(event).ctx("set event handle")?;
            let capture: IAudioCaptureClient = client.GetService().ctx("capture client")?;
            client.Start().ctx("start capture")?;
            println!("audio: capturing the default output (loopback)");
            Ok(Loopback { client, capture, event, pending: VecDeque::new(), front_ns: 0 })
        }
    }

    /// Moves everything the engine has captured into `pending`.
    fn drain(&mut self) -> Res<()> {
        unsafe {
            loop {
                let frames = self.capture.GetNextPacketSize().ctx("packet size")?;
                if frames == 0 {
                    return Ok(());
                }
                let (mut data, mut got, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                self.capture.GetBuffer(&mut data, &mut got, &mut flags, None, None).ctx("get buffer")?;
                let now = timing::now_ns();
                let samples = got as usize * AUDIO_CHANNELS;
                if self.pending.is_empty() {
                    // The packet ends about now.
                    self.front_ns = now.saturating_sub(got as u64 * ONE_FRAME_NS);
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    self.pending.extend(std::iter::repeat_n(0, samples));
                } else {
                    self.pending.extend(std::slice::from_raw_parts(data.cast::<i16>(), samples).iter().copied());
                }
                self.capture.ReleaseBuffer(got).ctx("release buffer")?;
            }
        }
    }
}

impl AudioSource for Loopback {
    fn on_thread_start(&mut self) {
        let mut task = 0;
        let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) };
    }

    fn next_chunk(&mut self, out: &mut [i16; CHUNK_SAMPLES]) -> Option<u64> {
        loop {
            if self.pending.len() >= CHUNK_SAMPLES {
                for (o, s) in out.iter_mut().zip(self.pending.drain(..CHUNK_SAMPLES)) {
                    *o = s;
                }
                let at = self.front_ns;
                self.front_ns += CHUNK_FRAMES as u64 * ONE_FRAME_NS;
                return Some(at);
            }
            // Nothing is captured while the system is silent: report that after a short wait.
            if unsafe { WaitForSingleObject(self.event, 20) } != WAIT_OBJECT_0 {
                self.pending.clear();
                return None;
            }
            if let Err(e) = self.drain() {
                eprintln!("audio capture: {e}");
                std::thread::sleep(std::time::Duration::from_millis(500));
                return None;
            }
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}
