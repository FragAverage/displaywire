//! Loading the TV's own libraries at run time. LG's NDL and the webOS SDL build live on the TV
//! only (no headers or stubs in the toolchain), so their functions are looked up by name.

use std::ffi::{CStr, c_char, c_int, c_void};

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

const RTLD_NOW: c_int = 2;
/// LG's libraries look up their own symbols by name, as they would if linked normally.
const RTLD_GLOBAL: c_int = 0x100;

fn last_error() -> String {
    let e = unsafe { dlerror() };
    if e.is_null() { "unknown error".into() } else { unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned() }
}

/// Never closed: the functions are used until the process exits.
pub struct Lib(*mut c_void);

// Only function pointers are read from the handle.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

impl Lib {
    pub fn open(name: &CStr) -> Result<Lib, String> {
        let h = unsafe { dlopen(name.as_ptr(), RTLD_NOW | RTLD_GLOBAL) };
        if h.is_null() { Err(last_error()) } else { Ok(Lib(h)) }
    }

    /// # Safety
    /// `F` must be the `extern "C" fn` type the library defines for `name`.
    pub unsafe fn get<F: Copy>(&self, name: &CStr) -> Result<F, String> {
        assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
        let p = unsafe { dlsym(self.0, name.as_ptr()) };
        if p.is_null() {
            return Err(format!("{}: {}", name.to_string_lossy(), last_error()));
        }
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
    }
}
