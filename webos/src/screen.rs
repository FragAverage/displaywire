//! The app's window, through the TV's SDL2 build: full screen and fully transparent, so the
//! video plane NDL draws on shows through. It also gives us the remote's key presses.

use std::ffi::{CStr, c_char, c_int, c_void};

use crate::dl::Lib;

const SDL_INIT_VIDEO: u32 = 0x20;
const SDL_WINDOW_FULLSCREEN: u32 = 0x1;
const SDL_WINDOW_SHOWN: u32 = 0x4;
const SDL_WINDOWPOS_UNDEFINED: c_int = 0x1FFF_0000;
const SDL_RENDERER_ACCELERATED: u32 = 0x2;
const SDL_QUIT: u32 = 0x100;
const SDL_KEYDOWN: u32 = 0x300;

/// `SDL_Event` is a 56-byte union; we read its type and, for key events, the scancode.
#[repr(C, align(8))]
struct Event([u8; 56]);

pub struct Screen {
    poll_event: extern "C" fn(*mut Event) -> c_int,
    destroy_window: extern "C" fn(*mut c_void),
    quit: extern "C" fn(),
    window: *mut c_void,
    _lib: Lib,
}

pub enum Input {
    Quit,
    Key(i32),
}

impl Screen {
    pub fn open() -> Result<Screen, String> {
        let lib = Lib::open(c"libSDL2-2.0.so.0")?;
        // SAFETY: types as in SDL 2.0's headers.
        unsafe {
            let get_error: extern "C" fn() -> *const c_char = lib.get(c"SDL_GetError")?;
            let error = || CStr::from_ptr(get_error()).to_string_lossy().into_owned();
            let init: extern "C" fn(u32) -> c_int = lib.get(c"SDL_Init")?;
            let create_window: extern "C" fn(*const c_char, c_int, c_int, c_int, c_int, u32) -> *mut c_void = lib.get(c"SDL_CreateWindow")?;
            let create_renderer: extern "C" fn(*mut c_void, c_int, u32) -> *mut c_void = lib.get(c"SDL_CreateRenderer")?;
            let set_color: extern "C" fn(*mut c_void, u8, u8, u8, u8) -> c_int = lib.get(c"SDL_SetRenderDrawColor")?;
            let clear: extern "C" fn(*mut c_void) -> c_int = lib.get(c"SDL_RenderClear")?;
            let present: extern "C" fn(*mut c_void) = lib.get(c"SDL_RenderPresent")?;
            if init(SDL_INIT_VIDEO) != 0 {
                return Err(format!("SDL_Init: {}", error()));
            }
            let flags = SDL_WINDOW_SHOWN | SDL_WINDOW_FULLSCREEN;
            let window = create_window(c"DisplayWire".as_ptr(), SDL_WINDOWPOS_UNDEFINED, SDL_WINDOWPOS_UNDEFINED, 1920, 1080, flags);
            if window.is_null() {
                return Err(format!("SDL_CreateWindow: {}", error()));
            }
            let renderer = create_renderer(window, -1, SDL_RENDERER_ACCELERATED);
            if renderer.is_null() {
                return Err(format!("SDL_CreateRenderer: {}", error()));
            }
            set_color(renderer, 0, 0, 0, 0);
            clear(renderer);
            present(renderer);
            Ok(Screen {
                poll_event: lib.get(c"SDL_PollEvent")?,
                destroy_window: lib.get(c"SDL_DestroyWindow")?,
                quit: lib.get(c"SDL_Quit")?,
                window,
                _lib: lib,
            })
        }
    }

    /// The next pending input, if any.
    pub fn poll(&self) -> Option<Input> {
        let mut e = Event([0; 56]);
        while (self.poll_event)(&mut e) != 0 {
            let kind = u32::from_ne_bytes(e.0[..4].try_into().unwrap());
            match kind {
                SDL_QUIT => return Some(Input::Quit),
                // SDL_KeyboardEvent: type, timestamp, windowID, state/repeat/padding, then keysym.scancode.
                SDL_KEYDOWN => return Some(Input::Key(i32::from_ne_bytes(e.0[16..20].try_into().unwrap()))),
                _ => {}
            }
        }
        None
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        (self.destroy_window)(self.window);
        (self.quit)();
    }
}
