//! The app's window, through the TV's SDL2 build: full screen, and either fully transparent (so
//! the video plane the decoder draws on shows through) or drawn on with rectangles for the
//! launcher's screens. It also gives us the remote's key presses.
//!
//! `DW_WINDOWED=1` opens an ordinary window instead, to try the screens on a desktop.

use std::ffi::{CStr, c_char, c_int, c_void};

use crate::dl::Lib;

const SDL_INIT_VIDEO: u32 = 0x20;
const SDL_WINDOW_FULLSCREEN: u32 = 0x1;
const SDL_WINDOW_SHOWN: u32 = 0x4;
const SDL_WINDOWPOS_UNDEFINED: c_int = 0x1FFF_0000;
const SDL_RENDERER_ACCELERATED: u32 = 0x2;
const SDL_QUIT: u32 = 0x100;
const SDL_KEYDOWN: u32 = 0x300;

/// The size of the picture the screens are laid out in, whatever the panel.
pub const WIDTH: i32 = 1920;
pub const HEIGHT: i32 = 1080;

#[cfg(target_os = "macos")]
const SDL_LIBRARY: &CStr = c"/opt/homebrew/lib/libSDL2.dylib";
#[cfg(not(target_os = "macos"))]
const SDL_LIBRARY: &CStr = c"libSDL2-2.0.so.0";

/// `SDL_Event` is a 56-byte union; we read its type and, for key events, the scancode.
#[repr(C, align(8))]
struct Event([u8; 56]);

/// `SDL_Rect`.
#[repr(C)]
struct Rect {
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
}

/// Red, green, blue, alpha.
pub type Color = [u8; 4];

/// Where the screens draw: rectangles on a `WIDTH`×`HEIGHT` picture.
pub trait Canvas {
    fn clear(&mut self, color: Color);
    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color);

    /// `text` in the bitmap font, `scale` screen pixels per font pixel, top left at `(x, y)`.
    fn text(&mut self, text: &str, x: i32, y: i32, scale: i32, color: Color) {
        crate::font::draw(text, x, y, scale, |x, y, w, h| self.rect(x, y, w, h, color));
    }
}

pub struct Screen {
    poll_event: extern "C" fn(*mut Event) -> c_int,
    destroy_window: extern "C" fn(*mut c_void),
    quit: extern "C" fn(),
    set_color: extern "C" fn(*mut c_void, u8, u8, u8, u8) -> c_int,
    clear: extern "C" fn(*mut c_void) -> c_int,
    fill_rect: extern "C" fn(*mut c_void, *const Rect) -> c_int,
    present: extern "C" fn(*mut c_void),
    renderer: *mut c_void,
    window: *mut c_void,
    _lib: Lib,
}

// The window is only used from the thread that made it; this lets the app hold it beside the
// stream's thread.
unsafe impl Send for Screen {}

pub enum Input {
    Quit,
    Key(i32),
}

impl Screen {
    pub fn open() -> Result<Screen, String> {
        let lib = Lib::open(SDL_LIBRARY)?;
        let windowed = std::env::var_os("DW_WINDOWED").is_some();
        // SAFETY: types as in SDL 2.0's headers.
        unsafe {
            let get_error: extern "C" fn() -> *const c_char = lib.get(c"SDL_GetError")?;
            let error = || CStr::from_ptr(get_error()).to_string_lossy().into_owned();
            let init: extern "C" fn(u32) -> c_int = lib.get(c"SDL_Init")?;
            let create_window: extern "C" fn(*const c_char, c_int, c_int, c_int, c_int, u32) -> *mut c_void = lib.get(c"SDL_CreateWindow")?;
            let create_renderer: extern "C" fn(*mut c_void, c_int, u32) -> *mut c_void = lib.get(c"SDL_CreateRenderer")?;
            if init(SDL_INIT_VIDEO) != 0 {
                return Err(format!("SDL_Init: {}", error()));
            }
            let (flags, w, h) = if windowed { (SDL_WINDOW_SHOWN, 1280, 720) } else { (SDL_WINDOW_SHOWN | SDL_WINDOW_FULLSCREEN, WIDTH, HEIGHT) };
            let window = create_window(c"DisplayWire".as_ptr(), SDL_WINDOWPOS_UNDEFINED, SDL_WINDOWPOS_UNDEFINED, w, h, flags);
            if window.is_null() {
                return Err(format!("SDL_CreateWindow: {}", error()));
            }
            let renderer = create_renderer(window, -1, SDL_RENDERER_ACCELERATED);
            if renderer.is_null() {
                return Err(format!("SDL_CreateRenderer: {}", error()));
            }
            if windowed {
                // The screens are laid out for 1920×1080 whatever the window.
                let logical: extern "C" fn(*mut c_void, c_int, c_int) -> c_int = lib.get(c"SDL_RenderSetLogicalSize")?;
                logical(renderer, WIDTH, HEIGHT);
            }
            let mut screen = Screen {
                poll_event: lib.get(c"SDL_PollEvent")?,
                destroy_window: lib.get(c"SDL_DestroyWindow")?,
                quit: lib.get(c"SDL_Quit")?,
                set_color: lib.get(c"SDL_SetRenderDrawColor")?,
                clear: lib.get(c"SDL_RenderClear")?,
                fill_rect: lib.get(c"SDL_RenderFillRect")?,
                present: lib.get(c"SDL_RenderPresent")?,
                renderer,
                window,
                _lib: lib,
            };
            screen.clear([0, 0, 0, 0]);
            screen.present();
            Ok(screen)
        }
    }

    /// Shows what has been drawn since the last time.
    pub fn present(&mut self) {
        (self.present)(self.renderer);
    }

    /// The next pending input, if any.
    pub fn poll(&self) -> Option<Input> {
        let mut e = Event([0; 56]);
        while (self.poll_event)(&mut e) != 0 {
            let kind = u32::from_ne_bytes(e.0[..4].try_into().unwrap());
            match kind {
                SDL_QUIT => return Some(Input::Quit),
                // SDL_KeyboardEvent as the TV's SDL lays it out (observed): type, timestamp, windowID,
                // one more 32-bit field, state/repeat/padding, then the keysym, starting with its
                // scancode. Standard SDL 2 has no extra field and the scancode at 16.
                SDL_KEYDOWN => {
                    if std::env::var_os("DW_KEYS_DEBUG").is_some() {
                        let hex: Vec<String> = e.0[..32].iter().map(|b| format!("{b:02x}")).collect();
                        println!("key event bytes: {}", hex.join(" "));
                    }
                    return Some(Input::Key(scancode(&e.0)));
                }
                other if std::env::var_os("DW_KEYS_DEBUG").is_some() && other != 0x400 => {
                    let hex: Vec<String> = e.0[..24].iter().map(|b| format!("{b:02x}")).collect();
                    println!("event {other:#x}: {}", hex.join(" "));
                }
                _ => {}
            }
        }
        None
    }
}

impl Canvas for Screen {
    fn clear(&mut self, [r, g, b, a]: Color) {
        (self.set_color)(self.renderer, r, g, b, a);
        (self.clear)(self.renderer);
    }

    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, [r, g, b, a]: Color) {
        (self.set_color)(self.renderer, r, g, b, a);
        (self.fill_rect)(self.renderer, &Rect { x, y, w, h });
    }
}

/// The scancode of a key event. The keysym's `sym` follows it (`scancode | 0x4000_0000` for keys
/// without a character), which tells the TV's layout from the standard one.
fn scancode(event: &[u8; 56]) -> i32 {
    let at = |o: usize| i32::from_ne_bytes(event[o..o + 4].try_into().unwrap());
    let consistent = |scan: i32, sym: i32| scan > 0 && scan < 512 && (sym == scan | 0x4000_0000 || (1..127).contains(&sym));
    if consistent(at(20), at(24)) || !consistent(at(16), at(20)) { at(20) } else { at(16) }
}

impl Drop for Screen {
    fn drop(&mut self) {
        (self.destroy_window)(self.window);
        (self.quit)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(bytes: &[u8]) -> [u8; 56] {
        let mut e = [0u8; 56];
        e[..bytes.len()].copy_from_slice(bytes);
        e
    }

    #[test]
    fn reads_the_tvs_key_events() {
        // Down, and "1", as dumped on the 50UM7450PLA.
        let down = event(&[0, 3, 0, 0, 0x44, 0xf0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0x84, 0x9e, 0x51, 0, 0, 0, 0x51, 0, 0, 0x40]);
        let one = event(&[0, 3, 0, 0, 0x5c, 8, 1, 0, 1, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0x84, 0x9e, 0x1e, 0, 0, 0, 0x31, 0, 0, 0]);
        assert_eq!(scancode(&down), 0x51);
        assert_eq!(scancode(&one), 0x1e);
    }

    #[test]
    fn reads_standard_sdl_key_events_too() {
        // Standard layout: Return pressed, scancode at 16.
        let enter = event(&[0, 3, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0x28, 0, 0, 0, 0x0d, 0, 0, 0]);
        assert_eq!(scancode(&enter), 0x28);
    }
}
