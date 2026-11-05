//! LG's NDL DirectMedia, API version 1 (webOS before 5): hands compressed video to the TV's
//! hardware decoder, which draws it on the video plane under our (transparent) window.
//! Signatures from webosbrew/tv-native-apis (`NDL_directmedia_v1.h`).

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};

use crate::dl::Lib;

#[repr(C)]
struct VideoInfo {
    width: c_int,
    height: c_int,
    /// `NDL_DIRECTVIDEO_SRC_TYPE`: the header only knows H.264 (1).
    source: c_int,
}

type ResourceReleased = extern "C" fn(*const c_char);

pub struct Ndl {
    get_error: extern "C" fn() -> *const c_char,
    quit: extern "C" fn() -> c_int,
    video_open: extern "C" fn(*const VideoInfo) -> c_int,
    video_close: extern "C" fn() -> c_int,
    video_play: extern "C" fn(*const c_void, c_uint) -> c_int,
    set_app_state: extern "C" fn(c_int) -> c_int,
    /// Size and source of the open video, if any.
    open: Option<(i32, i32, i32)>,
    _lib: Lib,
}

extern "C" fn released(kind: *const c_char) {
    let kind = if kind.is_null() { "?".into() } else { unsafe { CStr::from_ptr(kind) }.to_string_lossy() };
    eprintln!("ndl: resource released ({kind})");
}

impl Ndl {
    pub fn init(app_id: &str) -> Result<Ndl, String> {
        let lib = Lib::open(c"libNDL_directmedia.so.1")?;
        // SAFETY: types as in NDL_directmedia_v1.h.
        let ndl = unsafe {
            let init: extern "C" fn(*const c_char, ResourceReleased) -> c_int = lib.get(c"NDL_DirectMediaInit")?;
            let ndl = Ndl {
                get_error: lib.get(c"NDL_DirectMediaGetError")?,
                quit: lib.get(c"NDL_DirectMediaQuit")?,
                video_open: lib.get(c"NDL_DirectVideoOpen")?,
                video_close: lib.get(c"NDL_DirectVideoClose")?,
                video_play: lib.get(c"NDL_DirectVideoPlay")?,
                set_app_state: lib.get(c"NDL_DirectMediaSetAppState")?,
                open: None,
                _lib: lib,
            };
            let id = CString::new(app_id).map_err(|e| e.to_string())?;
            if init(id.as_ptr(), released) != 0 {
                return Err(format!("NDL_DirectMediaInit: {}", ndl.error()));
            }
            ndl
        };
        Ok(ndl)
    }

    fn error(&self) -> String {
        let e = (self.get_error)();
        if e.is_null() { "no error text".into() } else { unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned() }
    }

    /// Tells NDL the app is in front (0) or behind (1); the decoder is only granted to the front app.
    pub fn set_foreground(&self, front: bool) -> Result<(), String> {
        if (self.set_app_state)(if front { 0 } else { 1 }) != 0 {
            return Err(format!("NDL_DirectMediaSetAppState: {}", self.error()));
        }
        Ok(())
    }

    /// Opens (or reopens, if the size or source changed) the video decoder.
    pub fn open_video(&mut self, width: i32, height: i32, source: i32) -> Result<(), String> {
        if self.open == Some((width, height, source)) {
            return Ok(());
        }
        if self.open.take().is_some() {
            (self.video_close)();
        }
        let info = VideoInfo { width, height, source };
        if (self.video_open)(&info) != 0 {
            return Err(format!("NDL_DirectVideoOpen {width}x{height} source {source}: {}", self.error()));
        }
        println!("ndl: video open {width}x{height} source {source}");
        self.open = Some((width, height, source));
        Ok(())
    }

    /// One access unit, Annex B.
    pub fn play(&self, au: &[u8]) -> Result<(), String> {
        if (self.video_play)(au.as_ptr().cast(), au.len() as c_uint) != 0 {
            return Err(format!("NDL_DirectVideoPlay: {}", self.error()));
        }
        Ok(())
    }
}

impl Drop for Ndl {
    fn drop(&mut self) {
        if self.open.take().is_some() {
            (self.video_close)();
        }
        (self.quit)();
    }
}
