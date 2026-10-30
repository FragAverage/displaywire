//! WASAPI loopback: captures what the default output device is playing, as
//! 48 kHz stereo PCM16 (the audio engine converts from the device's format).

use std::collections::VecDeque;

use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, IAudioClient3, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eConsole,
    eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, IAgileObject, IAgileObject_Impl};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::core::{IUnknown, Interface, Ref, implement, w};

use super::{Context, Res};
use crate::audio::{AudioSource, CHUNK_FRAMES, CHUNK_SAMPLES};

/// 100 ns units: the engine's buffer. Loopback delivers in engine periods
/// (about 10 ms) regardless.
const BUFFER_100NS: i64 = 200_000;
const ONE_FRAME_NS: u64 = 1_000_000_000 / AUDIO_RATE as u64;

/// Tells the opener that process-loopback activation has finished.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivateDone(std::sync::mpsc::Sender<()>);

impl IAgileObject_Impl for ActivateDone_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateDone_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

pub struct Loopback {
    client: IAudioClient,
    /// An idle render stream at the engine's shortest period: the engine runs at
    /// the shortest period any client asks for, and loopback is delivered at the
    /// engine's period. Kept open while capturing.
    _pin: Option<IAudioClient>,
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
    /// Captures every app's audio (but not our own, which includes the client's microphone being
    /// played out), whatever output device each one uses. Falls back to the default output's
    /// loopback where Windows doesn't offer that (before Windows 10 2004), or with
    /// `DISPLAYWIRE_DEVICE_LOOPBACK=1`.
    pub fn open() -> Res<Loopback> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ctx("MMDeviceEnumerator")?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ctx("no default audio output")?;
            let pin = Self::pin_engine_period(&device).map_err(|e| println!("audio: can't shorten the engine period ({e}); capture will be ~10 ms behind")).ok();
            if std::env::var_os("DISPLAYWIRE_DEVICE_LOOPBACK").is_none() {
                match Self::process_client() {
                    Ok(client) => return Self::start(client, pin, "audio: capturing every app's output (process loopback)"),
                    Err(e) => println!("audio: process loopback unavailable ({e}); using the default output instead"),
                }
            }
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ctx("activate audio client")?;
            Self::default_period(&client, AUDCLNT_STREAMFLAGS_LOOPBACK, false)?;
            Self::start(client, pin, "audio: capturing the default output (loopback)")
        }
    }

    unsafe fn start(client: IAudioClient, pin: Option<IAudioClient>, what: &str) -> Res<Loopback> {
        unsafe {
            let event = CreateEventW(None, false, false, None).ctx("create event")?;
            client.SetEventHandle(event).ctx("set event handle")?;
            let capture: IAudioCaptureClient = client.GetService().ctx("capture client")?;
            client.Start().ctx("start capture")?;
            println!("{what}");
            Ok(Loopback { client, _pin: pin, capture, event, pending: VecDeque::new(), front_ns: 0 })
        }
    }

    /// An audio client capturing all processes except this one.
    unsafe fn process_client() -> Res<IAudioClient> {
        unsafe {
            let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: std::process::id(),
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            // The blob points at `params` on our stack: PROPVARIANT's drop would try to free it.
            let mut arg = std::mem::ManuallyDrop::new(PROPVARIANT::default());
            {
                let inner = &mut *arg.Anonymous.Anonymous;
                inner.vt = VT_BLOB;
                inner.Anonymous.blob = BLOB { cbSize: size_of_val(&params) as u32, pBlobData: (&raw mut params).cast() };
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let handler: IActivateAudioInterfaceCompletionHandler = ActivateDone(tx).into();
            let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*arg), &handler).ctx("activate process loopback")?;
            rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "activation timed out")?;
            let (mut result, mut client) = (windows::core::HRESULT(0), None::<IUnknown>);
            op.GetActivateResult(&mut result, &mut client).ctx("activation result")?;
            result.ok().ctx("activation failed")?;
            let client: IAudioClient = client.ok_or("no audio client")?.cast().ctx("audio client")?;
            Self::default_period(&client, AUDCLNT_STREAMFLAGS_LOOPBACK, true)?;
            Ok(client)
        }
    }

    /// Starts a silent render stream at the engine's minimum period.
    unsafe fn pin_engine_period(device: &IMMDevice) -> Res<IAudioClient> {
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ctx("activate render client")?;
            let client3: IAudioClient3 = client.cast()?;
            let mix = client.GetMixFormat().ctx("mix format")?;
            let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
            client3.GetSharedModeEnginePeriod(mix, &mut default, &mut fundamental, &mut min, &mut max).ctx("engine period")?;
            client3.InitializeSharedAudioStream(0, min, mix, None).ctx("initialize low-latency render stream")?;
            client.Start().ctx("start render stream")?;
            println!(
                "audio: engine period pinned to {:.1} ms (default {:.1} ms)",
                min as f64 * 1e3 / AUDIO_RATE as f64,
                default as f64 * 1e3 / AUDIO_RATE as f64
            );
            Ok(client)
        }
    }

    unsafe fn default_period(client: &IAudioClient, loopback: u32, process: bool) -> Res<()> {
        unsafe {
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
                    if process {
                        // The process-loopback client takes the format as given.
                        loopback | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    } else {
                        loopback | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                    },
                    if process { 0 } else { BUFFER_100NS },
                    0,
                    &format,
                    None,
                )
                .ctx("initialize loopback")?;
            Ok(())
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
