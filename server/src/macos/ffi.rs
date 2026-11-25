//! The C parts of CoreFoundation, CoreGraphics, CoreMedia, CoreVideo, IOKit and Accessibility
//! the Mac server uses, declared by hand (the Objective-C parts go through objc2).

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::{c_char, c_void};

pub type CFTypeRef = *const c_void;
pub type OSStatus = i32;
pub type CGDirectDisplayID = u32;
pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

pub const kAudioFormatFlagIsFloat: u32 = 1;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// An `AudioBufferList` with room for up to 8 buffers (one per channel when planar).
#[repr(C)]
pub struct AudioBufferList8 {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 8],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFArrayGetCount(array: CFTypeRef) -> isize;
    pub fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    pub fn CFDictionaryCreateMutable(alloc: CFTypeRef, cap: isize, k: *const c_void, v: *const c_void) -> CFTypeRef;
    pub fn CFDictionarySetValue(dict: CFTypeRef, key: CFTypeRef, value: CFTypeRef);
    pub fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    pub fn CFNumberGetValue(number: CFTypeRef, ty: isize, out: *mut c_void) -> u8;
    pub fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFTypeRef;
}

pub const kCFNumberSInt64Type: isize = 4;
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    pub fn CMSampleBufferGetImageBuffer(sbuf: CFTypeRef) -> CFTypeRef;
    pub fn CMSampleBufferGetSampleAttachmentsArray(sbuf: CFTypeRef, create: u8) -> CFTypeRef;
    pub fn CMSampleBufferGetFormatDescription(sbuf: CFTypeRef) -> CFTypeRef;
    pub fn CMSampleBufferGetNumSamples(sbuf: CFTypeRef) -> isize;
    pub fn CMAudioFormatDescriptionGetStreamBasicDescription(desc: CFTypeRef) -> *const AudioStreamBasicDescription;
    pub fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sbuf: CFTypeRef,
        size_needed: *mut usize,
        list: *mut AudioBufferList8,
        list_size: usize,
        block_alloc: CFTypeRef,
        mem_alloc: CFTypeRef,
        flags: u32,
        block_out: *mut CFTypeRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    pub fn CVPixelBufferGetWidth(pb: CFTypeRef) -> usize;
    pub fn CVPixelBufferGetHeight(pb: CFTypeRef) -> usize;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub fn CGMainDisplayID() -> CGDirectDisplayID;
    pub fn CGGetActiveDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> i32;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    pub fn CGDisplayIsBuiltin(display: CGDirectDisplayID) -> u32;
    pub fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CFTypeRef;
    pub fn CGDisplayModeGetPixelWidth(mode: CFTypeRef) -> usize;
    pub fn CGDisplayModeGetPixelHeight(mode: CFTypeRef) -> usize;
    pub fn CGDisplayModeGetRefreshRate(mode: CFTypeRef) -> f64;
    pub fn CGDisplayModeRelease(mode: CFTypeRef);
    pub fn CGDisplayVendorNumber(display: CGDirectDisplayID) -> u32;
    pub fn CGDisplayModelNumber(display: CGDirectDisplayID) -> u32;
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;

    pub fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    pub fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    pub fn CGEventCreateMouseEvent(source: CGEventSourceRef, ty: u32, at: CGPoint, button: u32) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent2(source: CGEventSourceRef, units: u32, count: u32, w1: i32, w2: i32, w3: i32) -> CGEventRef;
    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventSetType(event: CGEventRef, ty: u32);
    pub fn CGEventSetFlags(event: CGEventRef, flags: u64);
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    pub fn CGEventPost(tap: u32, event: CGEventRef);
    pub fn CGWarpMouseCursorPosition(at: CGPoint) -> i32;
    pub fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
}

pub const kCGEventSourceStateHIDSystemState: i32 = 1;
pub const kCGHIDEventTap: u32 = 0;

pub const kCGEventLeftMouseDown: u32 = 1;
pub const kCGEventLeftMouseUp: u32 = 2;
pub const kCGEventRightMouseDown: u32 = 3;
pub const kCGEventRightMouseUp: u32 = 4;
pub const kCGEventMouseMoved: u32 = 5;
pub const kCGEventLeftMouseDragged: u32 = 6;
pub const kCGEventRightMouseDragged: u32 = 7;
pub const kCGEventFlagsChanged: u32 = 12;
pub const kCGEventOtherMouseDown: u32 = 25;
pub const kCGEventOtherMouseUp: u32 = 26;
pub const kCGEventOtherMouseDragged: u32 = 27;

pub const kCGMouseEventClickState: u32 = 1;
pub const kCGMouseEventDeltaX: u32 = 4;
pub const kCGMouseEventDeltaY: u32 = 5;
pub const kCGMouseEventButtonNumber: u32 = 3;
pub const kCGKeyboardEventAutorepeat: u32 = 8;
pub const kCGScrollEventUnitPixel: u32 = 0;

pub const kCGEventFlagMaskAlphaShift: u64 = 0x0001_0000;
pub const kCGEventFlagMaskShift: u64 = 0x0002_0000;
pub const kCGEventFlagMaskControl: u64 = 0x0004_0000;
pub const kCGEventFlagMaskAlternate: u64 = 0x0008_0000;
pub const kCGEventFlagMaskCommand: u64 = 0x0010_0000;
pub const kCGEventFlagMaskNumericPad: u64 = 0x0020_0000;
pub const kCGEventFlagMaskSecondaryFn: u64 = 0x0080_0000;
/// Device-dependent bits (IOLLEvent.h) saying which side's modifier is down.
pub const NX_DEVICELCTLKEYMASK: u64 = 0x0001;
pub const NX_DEVICELSHIFTKEYMASK: u64 = 0x0002;
pub const NX_DEVICERSHIFTKEYMASK: u64 = 0x0004;
pub const NX_DEVICELCMDKEYMASK: u64 = 0x0008;
pub const NX_DEVICERCMDKEYMASK: u64 = 0x0010;
pub const NX_DEVICELALTKEYMASK: u64 = 0x0020;
pub const NX_DEVICERALTKEYMASK: u64 = 0x0040;
pub const NX_DEVICERCTLKEYMASK: u64 = 0x2000;
/// Always set on real keyboard events.
pub const NX_NONCOALSESCEDMASK: u64 = 0x0100;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub fn IOPMAssertionCreateWithName(ty: CFTypeRef, level: u32, name: CFTypeRef, id: *mut u32) -> i32;
    pub fn IOPMAssertionRelease(id: u32) -> i32;
    pub fn IOPMAssertionDeclareUserActivity(name: CFTypeRef, ty: u32, id: *mut u32) -> i32;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub static kAXTrustedCheckOptionPrompt: CFTypeRef;
    pub fn AXIsProcessTrusted() -> bool;
    pub fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    pub fn AXUIElementCreateSystemWide() -> CFTypeRef;
    pub fn AXUIElementCopyAttributeValue(element: CFTypeRef, attribute: CFTypeRef, value: *mut CFTypeRef) -> i32;
}

/// An owned CoreFoundation object.
pub struct Cf(pub CFTypeRef);

// Only immutable CF objects (strings, pixel buffers handed between threads) are kept in these.
unsafe impl Send for Cf {}
unsafe impl Sync for Cf {}

impl Cf {
    /// Takes a +1 reference (from a Create or Copy function); None if null.
    pub fn owned(p: CFTypeRef) -> Option<Cf> {
        (!p.is_null()).then_some(Cf(p))
    }

    /// Retains a +0 reference.
    pub fn retained(p: CFTypeRef) -> Option<Cf> {
        (!p.is_null()).then(|| Cf(unsafe { CFRetain(p) }))
    }

    pub fn string(s: &str) -> Cf {
        let c = std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
        Cf(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) })
    }
}

impl Clone for Cf {
    fn clone(&self) -> Self {
        Cf(unsafe { CFRetain(self.0) })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// The display's mode: pixel size and refresh rate (Hz, 60 when the display doesn't say).
pub fn display_mode(display: CGDirectDisplayID) -> Option<(u32, u32, u32)> {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(display);
        if mode.is_null() {
            return None;
        }
        let r = (CGDisplayModeGetPixelWidth(mode) as u32, CGDisplayModeGetPixelHeight(mode) as u32, CGDisplayModeGetRefreshRate(mode));
        CGDisplayModeRelease(mode);
        Some((r.0, r.1, if r.2 >= 1.0 { r.2.round() as u32 } else { 60 }))
    }
}

/// Active displays, main first then in the system's order.
pub fn displays() -> Vec<CGDirectDisplayID> {
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) } != 0 {
        return vec![unsafe { CGMainDisplayID() }];
    }
    let mut v = ids[..n as usize].to_vec();
    let main = unsafe { CGMainDisplayID() };
    if let Some(i) = v.iter().position(|&d| d == main) {
        v.remove(i);
        v.insert(0, main);
    }
    v
}
