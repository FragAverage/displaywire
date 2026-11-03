//! LG's older LGNC direct video (`liblgncopenapi`): H.264 only, but unlike NDL it works from the
//! Developer Mode jail on k3lp/k5lp TVs (moonlight-tv falls back to it there).
//! Signatures from webosbrew/tv-native-apis (`lgncapi/include`).

use std::ffi::{CString, c_int, c_uint, c_void};

use crate::dl::Lib;

const LGNC_VDEC_FMT_H264: c_int = 2;
const LGNC_VDEC_3D_TYPE_NONE: c_int = 0;

#[repr(C)]
struct VideoInfo {
    width: c_int,
    height: c_int,
    format: c_int,
    three_d: c_int,
}

/// `LGNC_CALLBACKS_T`: message, key, mouse and joystick handlers; we register none.
#[repr(C)]
struct Callbacks([*const c_void; 4]);

pub struct Lgnc {
    finalize: extern "C" fn() -> c_int,
    video_open: extern "C" fn(*const VideoInfo) -> c_int,
    video_close: extern "C" fn() -> c_int,
    video_play: extern "C" fn(*const c_void, c_uint) -> c_int,
    set_window: extern "C" fn(c_int, c_int, c_int, c_int) -> c_int,
    open: Option<(i32, i32)>,
    _lib: Lib,
}

// Only function pointers.
unsafe impl Send for Lgnc {}

impl Lgnc {
    pub fn init(app_id: &str) -> Result<Lgnc, String> {
        let lib = Lib::open(c"liblgncopenapi.so.2")?;
        // SAFETY: types as in lgnc_plugin.h and lgnc_directvideo.h.
        unsafe {
            let init: extern "C" fn(*mut Callbacks) -> c_int = lib.get(c"LGNC_PLUGIN_Initialize")?;
            let set_app_id: extern "C" fn(*const std::ffi::c_char) -> c_int = lib.get(c"LGNC_PLUGIN_SetAppId")?;
            let lgnc = Lgnc {
                finalize: lib.get(c"LGNC_PLUGIN_Finalize")?,
                video_open: lib.get(c"LGNC_DIRECTVIDEO_Open")?,
                video_close: lib.get(c"LGNC_DIRECTVIDEO_Close")?,
                video_play: lib.get(c"LGNC_DIRECTVIDEO_Play")?,
                set_window: lib.get(c"_LGNC_DIRECTVIDEO_SetDisplayWindow")?,
                open: None,
                _lib: lib,
            };
            let mut callbacks = Callbacks([std::ptr::null(); 4]);
            let rc = init(&mut callbacks);
            if rc != 0 {
                return Err(format!("LGNC_PLUGIN_Initialize: {rc}"));
            }
            let id = CString::new(app_id).map_err(|e| e.to_string())?;
            set_app_id(id.as_ptr());
            Ok(lgnc)
        }
    }

    /// Opens (or reopens, if the size changed) the H.264 decoder, letterboxed to the screen.
    pub fn open_video(&mut self, width: i32, height: i32) -> Result<(), String> {
        if self.open == Some((width, height)) {
            return Ok(());
        }
        if self.open.take().is_some() {
            (self.video_close)();
        }
        let info = VideoInfo { width, height, format: LGNC_VDEC_FMT_H264, three_d: LGNC_VDEC_3D_TYPE_NONE };
        let rc = (self.video_open)(&info);
        if rc != 0 {
            return Err(format!("LGNC_DIRECTVIDEO_Open {width}x{height}: {rc}"));
        }
        // The display window is in 1920×1080 UI coordinates whatever the panel.
        let h = 1920 * height / width;
        if h <= 1080 {
            (self.set_window)(0, (1080 - h) / 2, 1920, h);
        } else {
            let w = 1920 * 1080 / h;
            (self.set_window)((1920 - w) / 2, 0, w, 1080);
        }
        println!("lgnc: video open {width}x{height}");
        self.open = Some((width, height));
        Ok(())
    }

    /// One access unit, Annex B.
    pub fn play(&self, au: &[u8]) -> Result<(), String> {
        let rc = (self.video_play)(au.as_ptr().cast(), au.len() as c_uint);
        if rc != 0 {
            return Err(format!("LGNC_DIRECTVIDEO_Play: {rc}"));
        }
        Ok(())
    }
}

impl Drop for Lgnc {
    fn drop(&mut self) {
        if self.open.take().is_some() {
            (self.video_close)();
        }
        (self.finalize)();
    }
}
