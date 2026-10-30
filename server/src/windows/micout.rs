//! Plays the client's microphone into a render device, normally a virtual
//! cable such as VB-Cable: apps then record from the cable's other end.

use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::core::w;

use super::{Context, Res};

const CH: usize = AUDIO_CHANNELS;
const MS: usize = AUDIO_RATE as usize / 1000;
/// Queued audio needed before playback starts (and again after a gap).
const PREFILL: usize = 10 * MS;
/// More than this queued is a backlog: skip ahead to the cushion.
const BACKLOG: usize = 80 * MS;
/// 100 ns units: the engine's buffer.
const BUFFER_100NS: i64 = 200_000;

fn name_of(device: &IMMDevice) -> Res<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ctx("property store")?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ctx("device name")?;
        let name = value.to_string();
        Ok(name)
    }
}

/// The active render device named by the first of `wanted` (alternatives split by `|`, matched
/// as part of the name in any case) that exists, and every name seen.
fn find(wanted: &str) -> Res<(Option<IMMDevice>, Vec<String>)> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ctx("MMDeviceEnumerator")?;
        let all = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).ctx("list render devices")?;
        let mut devices = Vec::new();
        for i in 0..all.GetCount().ctx("device count")? {
            let device = all.Item(i).ctx("device")?;
            devices.push((name_of(&device).unwrap_or_default().to_lowercase(), device));
        }
        let seen = devices.iter().map(|(n, _)| n.clone()).collect();
        let found = wanted
            .split('|')
            .find_map(|w| devices.iter().find(|(n, _)| n.contains(&w.trim().to_lowercase())))
            .map(|(_, d)| d.clone());
        Ok((found, seen))
    }
}

/// Starts the render thread. Fails (and says why) if no device matches `wanted`.
pub fn start(wanted: &str, consumer: rtrb::Consumer<i16>) -> Result<(), String> {
    let wanted = wanted.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("mic-out".into())
        .spawn(move || {
            let opened = unsafe { open(&wanted) };
            match opened {
                Ok((client, render, event, frames, name)) => {
                    println!("mic: playing the client's microphone into {name:?}");
                    let _ = tx.send(Ok(()));
                    unsafe { run(&client, &render, event, frames, consumer) };
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

type Opened = (IAudioClient, IAudioRenderClient, windows::Win32::Foundation::HANDLE, u32, String);

unsafe fn open(wanted: &str) -> Res<Opened> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let (device, seen) = find(wanted)?;
        let Some(device) = device else {
            return Err(format!(
                "no audio output matching {wanted:?} (install a virtual cable such as VB-Cable, or pass --mic-device); outputs: {}",
                seen.join(", ")
            )
            .into());
        };
        let name = name_of(&device).unwrap_or_default();
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ctx("activate audio client")?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CH as u16,
            nSamplesPerSec: AUDIO_RATE,
            nAvgBytesPerSec: AUDIO_RATE * CH as u32 * 2,
            nBlockAlign: CH as u16 * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_100NS,
                0,
                &format,
                None,
            )
            .ctx("initialize mic output")?;
        let event = CreateEventW(None, false, false, None).ctx("create event")?;
        client.SetEventHandle(event).ctx("set event handle")?;
        let render: IAudioRenderClient = client.GetService().ctx("render client")?;
        let frames = client.GetBufferSize().ctx("buffer size")?;
        // Start with a buffer of silence so the engine has something to read.
        render.GetBuffer(frames).ctx("first buffer")?;
        render.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).ctx("release first buffer")?;
        client.Start().ctx("start mic output")?;
        Ok((client, render, event, frames, name))
    }
}

unsafe fn run(client: &IAudioClient, render: &IAudioRenderClient, event: windows::Win32::Foundation::HANDLE, frames: u32, mut ring: rtrb::Consumer<i16>) {
    unsafe {
        let mut task = 0;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
        let mut playing = false;
        loop {
            // Wakes at least every 100 ms even if the device stops asking.
            if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                continue;
            }
            let Ok(padding) = client.GetCurrentPadding() else { continue };
            let free = frames - padding;
            if free == 0 {
                continue;
            }
            let Ok(data) = render.GetBuffer(free) else { continue };
            let out = std::slice::from_raw_parts_mut(data.cast::<i16>(), free as usize * CH);
            let mut queued = ring.slots() / CH;
            if queued > BACKLOG
                && let Ok(chunk) = ring.read_chunk((queued - PREFILL) * CH)
            {
                chunk.commit_all();
                queued = PREFILL;
            }
            if !playing && queued >= PREFILL {
                playing = true;
            }
            let take = if playing { (queued * CH).min(out.len()) } else { 0 };
            if take > 0
                && let Ok(chunk) = ring.read_chunk(take)
            {
                let (a, b) = chunk.as_slices();
                for (o, s) in out.iter_mut().zip(a.iter().chain(b)) {
                    *o = *s;
                }
                chunk.commit_all();
            }
            out[take..].fill(0);
            if playing && take < out.len() {
                playing = false;
            }
            let _ = render.ReleaseBuffer(free, 0);
        }
    }
}
