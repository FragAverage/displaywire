//! The pointer for a client that draws it (`CAP_CURSOR`): where it is, from a CoreGraphics event,
//! and what it looks like, from `NSCursor.currentSystemCursor` drawn into RGBA at the display's
//! pixel scale (the scale of the video).

use std::cell::RefCell;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};

use objc2::encode::{Encoding, RefEncode};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use timing::now_ns;

use super::cg::{self, CGPoint, CGRect, CGSize};
use crate::{PointerImage, PointerState};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> *const c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CGColorSpaceCreateDeviceRGB() -> *const c_void;
    fn CGColorSpaceRelease(space: *const c_void);
    fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bits: usize, row: usize, space: *const c_void, info: u32) -> *const c_void;
    fn CGContextDrawImage(ctx: *const c_void, rect: CGRect, image: *const c_void);
    fn CGContextRelease(ctx: *const c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// `CGImageRef`, as Objective-C describes it.
#[repr(C)]
struct CGImage {
    _opaque: [u8; 0],
}

unsafe impl RefEncode for CGImage {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGImage", &[]));
}

const PREMULTIPLIED_LAST: u32 = 1;
/// How often the shape is looked at again.
const SHAPE_EVERY_NS: u64 = 50_000_000;

#[derive(Default)]
pub struct Pointer {
    image: RefCell<Option<(PointerImage, u64)>>,
}

/// Where the pointer is on the desktop, in points.
fn location() -> CGPoint {
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return CGPoint::default();
        }
        let at = CGEventGetLocation(event);
        CFRelease(event);
        at
    }
}

impl Pointer {
    /// The pointer as `display` shows it, in a picture `video_width` pixels wide: 0..=65535 across,
    /// and hidden when it's on another display.
    pub fn state(&self, display: u32, video_width: u32) -> PointerState {
        let b = cg::bounds(display);
        let at = location();
        let (fx, fy) = ((at.x - b.origin.x) / b.size.width, (at.y - b.origin.y) / b.size.height);
        let inside = (0.0..1.0).contains(&fx) && (0.0..1.0).contains(&fy);
        let shape = self.refresh(video_width as f64 / b.size.width);
        PointerState { visible: inside && shape.is_some(), x: (fx.clamp(0.0, 1.0) * 65535.0) as u16, y: (fy.clamp(0.0, 1.0) * 65535.0) as u16, shape: shape.unwrap_or(0) }
    }

    pub fn image(&self) -> Option<PointerImage> {
        self.image.borrow().as_ref().map(|(i, _)| PointerImage { id: i.id, width: i.width, height: i.height, hot: i.hot, rgba: i.rgba.clone() })
    }

    /// The current shape's id, drawing it again now and then at `scale` video pixels per point.
    fn refresh(&self, scale: f64) -> Option<u32> {
        let now = now_ns();
        if let Some((i, at)) = &*self.image.borrow()
            && now - at < SHAPE_EVERY_NS
        {
            return Some(i.id);
        }
        let image = current(scale.max(0.5));
        let id = image.as_ref().map(|i| i.id);
        *self.image.borrow_mut() = image.map(|i| (i, now));
        id
    }
}

/// The system's current pointer drawn at `scale` pixels per point, its id a hash of the pixels.
fn current(scale: f64) -> Option<PointerImage> {
    autoreleasepool(|_| unsafe {
        let cursor: *mut AnyObject = msg_send![class!(NSCursor), currentSystemCursor];
        let cursor = cursor.as_ref()?;
        let image: *mut AnyObject = msg_send![cursor, image];
        let image = image.as_ref()?;
        let size: NSSize = msg_send![image, size];
        let hot: NSPoint = msg_send![cursor, hotSpot];
        if size.width <= 0.0 || size.height <= 0.0 {
            return None;
        }
        // No bigger than the protocol carries.
        let max = proto::stream::MAX_CURSOR_SIZE as f64;
        let scale = scale.min(max / size.width).min(max / size.height);
        let (w, h) = ((size.width * scale).round().max(1.0) as usize, (size.height * scale).round().max(1.0) as usize);
        let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w as f64, h as f64));
        let null: *const AnyObject = std::ptr::null();
        let cg_image: *const CGImage = msg_send![image, CGImageForProposedRect: &mut rect, context: null, hints: null];
        if cg_image.is_null() {
            return None;
        }
        let mut rgba = vec![0u8; w * h * 4];
        let space = CGColorSpaceCreateDeviceRGB();
        let ctx = CGBitmapContextCreate(rgba.as_mut_ptr().cast(), w, h, 8, w * 4, space, PREMULTIPLIED_LAST);
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            return None;
        }
        CGContextDrawImage(ctx, CGRect { origin: CGPoint::default(), size: CGSize { width: w as f64, height: h as f64 } }, cg_image.cast());
        CGContextRelease(ctx);
        // Straight alpha, as the protocol wants.
        for px in rgba.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            if a != 0 && a != 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (w, h, &rgba).hash(&mut hasher);
        let id = (hasher.finish() as u32).max(1);
        let hot = (((hot.x * scale) as u32).min(w as u32 - 1), ((hot.y * scale) as u32).min(h as u32 - 1));
        Some(PointerImage { id, width: w as u32, height: h as u32, hot, rgba })
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_pointer_draws_at_retina_scale() {
        // No pointer without a window server (a CI runner may have none).
        let Some(i) = super::current(2.0) else { return };
        assert!(i.width >= 8 && i.width <= 128 && i.height >= 8 && i.height <= 128, "{}x{}", i.width, i.height);
        assert_eq!(i.rgba.len(), (i.width * i.height * 4) as usize);
        assert!(i.rgba.chunks(4).any(|p| p[3] == 255), "nothing opaque");
        assert!(i.hot.0 < i.width && i.hot.1 < i.height);
    }
}
