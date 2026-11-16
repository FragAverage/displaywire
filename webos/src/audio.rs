#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
//! Sound out of the TV's SDL2 build: the server's 48 kHz stereo 16-bit PCM, queued to an audio
//! device. The queue is kept short so the sound stays with the picture.

use std::ffi::{CStr, c_char, c_int, c_void};

use proto::stream::{AUDIO_CHANNELS, AUDIO_RATE};

use crate::dl::Lib;
use crate::screen::SDL_LIBRARY;

const SDL_INIT_AUDIO: u32 = 0x10;
/// `AUDIO_S16LSB`.
const FORMAT_S16: u16 = 0x8010;
/// Longest the queue may get before old sound is thrown away.
const MAX_QUEUED_MS: u32 = 120;
/// Sound is held back until this much is queued, so the first chunks don't run dry at once.
const START_QUEUED_MS: u32 = 30;
const BYTES_PER_MS: u32 = AUDIO_RATE * 2 * AUDIO_CHANNELS as u32 / 1000;

/// `SDL_AudioSpec`.
#[repr(C)]
struct Spec {
    freq: c_int,
    format: u16,
    channels: u8,
    silence: u8,
    samples: u16,
    padding: u16,
    size: u32,
    callback: *const c_void,
    userdata: *mut c_void,
}

pub struct Audio {
    queue: extern "C" fn(u32, *const c_void, u32) -> c_int,
    queued: extern "C" fn(u32) -> u32,
    clear: extern "C" fn(u32),
    pause: extern "C" fn(u32, c_int),
    close: extern "C" fn(u32),
    device: u32,
    playing: bool,
    _lib: Lib,
}

// Only function pointers and a device number, used from one thread at a time.
unsafe impl Send for Audio {}

impl Audio {
    pub fn open() -> Result<Audio, String> {
        let lib = Lib::open(SDL_LIBRARY)?;
        // SAFETY: types as in SDL 2.0's headers.
        unsafe {
            let get_error: extern "C" fn() -> *const c_char = lib.get(c"SDL_GetError")?;
            let error = || CStr::from_ptr(get_error()).to_string_lossy().into_owned();
            let init: extern "C" fn(u32) -> c_int = lib.get(c"SDL_InitSubSystem")?;
            let open: extern "C" fn(*const c_char, c_int, *const Spec, *mut Spec, c_int) -> u32 = lib.get(c"SDL_OpenAudioDevice")?;
            let driver: extern "C" fn() -> *const c_char = lib.get(c"SDL_GetCurrentAudioDriver")?;
            if init(SDL_INIT_AUDIO) != 0 {
                return Err(format!("SDL audio: {}", error()));
            }
            let want = Spec {
                freq: AUDIO_RATE as c_int,
                format: FORMAT_S16,
                channels: AUDIO_CHANNELS as u8,
                silence: 0,
                samples: 1024,
                padding: 0,
                size: 0,
                callback: std::ptr::null(),
                userdata: std::ptr::null_mut(),
            };
            let mut have = Spec { callback: std::ptr::null(), userdata: std::ptr::null_mut(), ..want };
            // No callback: sound is queued to the device. Any change of format is for SDL to convert (0).
            let device = open(std::ptr::null(), 0, &want, &mut have, 0);
            if device == 0 {
                return Err(format!("SDL_OpenAudioDevice: {}", error()));
            }
            let name = driver();
            println!(
                "audio: {} Hz, {} channels, device {device}, driver {}",
                have.freq,
                have.channels,
                if name.is_null() { "?".into() } else { CStr::from_ptr(name).to_string_lossy().into_owned() }
            );
            Ok(Audio {
                queue: lib.get(c"SDL_QueueAudio")?,
                queued: lib.get(c"SDL_GetQueuedAudioSize")?,
                clear: lib.get(c"SDL_ClearQueuedAudio")?,
                pause: lib.get(c"SDL_PauseAudioDevice")?,
                close: lib.get(c"SDL_CloseAudioDevice")?,
                device,
                playing: false,
                _lib: lib,
            })
        }
    }

    /// One chunk of interleaved 16-bit stereo.
    pub fn play(&mut self, pcm: &[u8]) {
        let queued = (self.queued)(self.device);
        if queued > MAX_QUEUED_MS * BYTES_PER_MS {
            // Behind: skip what is waiting, rather than play it late.
            (self.clear)(self.device);
        }
        if (self.queue)(self.device, pcm.as_ptr().cast(), pcm.len() as u32) != 0 {
            return;
        }
        if !self.playing && (self.queued)(self.device) >= START_QUEUED_MS * BYTES_PER_MS {
            (self.pause)(self.device, 0);
            self.playing = true;
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        (self.close)(self.device);
    }
}
