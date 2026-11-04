//! The few CoreGraphics calls the Mac server needs, over raw FFI: screen-capture permission,
//! display geometry and modes.

use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> *const c_void;
    fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
    fn CGDisplayModeGetRefreshRate(mode: *const c_void) -> f64;
    fn CGDisplayModeRelease(mode: *const c_void);
}

pub fn screen_capture_allowed() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Shows the system's request (once; after that only System Settings changes it).
pub fn request_screen_capture() {
    unsafe { CGRequestScreenCaptureAccess() };
}

pub fn main_display() -> u32 {
    unsafe { CGMainDisplayID() }
}

pub fn is_builtin(display: u32) -> bool {
    unsafe { CGDisplayIsBuiltin(display) != 0 }
}

/// Where `display` is on the desktop, in points (what mouse events use).
pub fn bounds(display: u32) -> CGRect {
    unsafe { CGDisplayBounds(display) }
}

/// The display's current mode in pixels (Retina: twice its points) and its refresh rate in Hz.
pub fn pixel_size(display: u32) -> (u32, u32, u32) {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(display);
        if mode.is_null() {
            let b = CGDisplayBounds(display);
            return (b.size.width as u32 & !1, b.size.height as u32 & !1, 60);
        }
        let size = (CGDisplayModeGetPixelWidth(mode) as u32 & !1, CGDisplayModeGetPixelHeight(mode) as u32 & !1);
        let hz = CGDisplayModeGetRefreshRate(mode).round() as u32;
        CGDisplayModeRelease(mode);
        // Built-in panels report 0 (ProMotion adapts); treat them as 120 Hz.
        (size.0, size.1, if hz == 0 { 120 } else { hz })
    }
}
