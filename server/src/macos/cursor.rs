//! The pointer for clients that draw it themselves (`CAP_CURSOR`): its position on the streamed
//! display and its image, from `NSCursor.currentSystemCursor`. Where macOS doesn't give the
//! system cursor (it is deprecated, and may one day return nothing), the pointer stays in the
//! video instead.

use std::ffi::c_void;
use std::sync::Mutex;

use objc2::rc::autoreleasepool;
use objc2_app_kit::NSCursor;
use objc2_core_graphics::CGImage;

use super::ffi::{self, CFTypeRef, CGDirectDisplayID, CGPoint, CGRect, CGSize};
use crate::{PointerImage, PointerState};

/// How often the image is looked at again.
const IMAGE_EVERY_NS: u64 = 100_000_000;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> CFTypeRef;
    fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bits: usize, row: usize, space: CFTypeRef, info: u32) -> CFTypeRef;
    fn CGContextDrawImage(ctx: CFTypeRef, rect: CGRect, image: CFTypeRef);
    fn CGContextSetInterpolationQuality(ctx: CFTypeRef, q: i32);
}

/// Premultiplied RGBA, in memory order.
const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);

pub struct Tracker {
    available: bool,
    in_video: bool,
    image: Mutex<Option<(u64, PointerImage)>>,
}

impl Tracker {
    pub fn new() -> Tracker {
        let available = autoreleasepool(|_| {
            #[allow(deprecated)]
            NSCursor::currentSystemCursor().is_some()
        });
        if !available {
            println!("cursor: macOS doesn't give the system cursor; it stays in the video");
        }
        Tracker { available, in_video: true, image: Mutex::new(None) }
    }

    pub fn in_video(&self) -> bool {
        self.in_video
    }

    /// Whether the client asked to draw it: only possible when the image is available.
    pub fn set_in_video(&mut self, on: bool) {
        self.in_video = on || !self.available;
    }

    pub fn state(&self, display: CGDirectDisplayID) -> Option<PointerState> {
        if !self.available {
            return None;
        }
        let at = unsafe {
            let e = ffi::CGEventCreate(std::ptr::null_mut());
            if e.is_null() {
                return None;
            }
            let p = ffi::CGEventGetLocation(e);
            ffi::CFRelease(e);
            p
        };
        let b = unsafe { ffi::CGDisplayBounds(display) };
        let (x, y) = ((at.x - b.origin.x) / b.size.width.max(1.0), (at.y - b.origin.y) / b.size.height.max(1.0));
        let on_display = (0.0..1.0).contains(&x) && (0.0..1.0).contains(&y);
        let shape = self.refresh(display).unwrap_or(0);
        Some(PointerState {
            visible: on_display && shape != 0,
            x: (x.clamp(0.0, 1.0) * 65535.0) as u16,
            y: (y.clamp(0.0, 1.0) * 65535.0) as u16,
            shape,
        })
    }

    pub fn image(&self) -> Option<PointerImage> {
        let g = self.image.lock().unwrap();
        g.as_ref().map(|(_, i)| PointerImage { id: i.id, width: i.width, height: i.height, hot: i.hot, rgba: i.rgba.clone() })
    }

    /// Looks at the cursor's image again if it's due; its id.
    fn refresh(&self, display: CGDirectDisplayID) -> Option<u32> {
        let now = timing::now_ns();
        let mut g = self.image.lock().unwrap();
        if let Some((at, img)) = g.as_ref()
            && now - at < IMAGE_EVERY_NS
        {
            return Some(img.id);
        }
        // Pixels per point on this display: the video is in pixels.
        let scale = ffi::display_mode(display).map_or(1.0, |(w, ..)| w as f64 / unsafe { ffi::CGDisplayBounds(display) }.size.width.max(1.0));
        match current(scale) {
            Some(img) => {
                let id = img.id;
                *g = Some((now, img));
                Some(id)
            }
            None => g.as_ref().map(|(_, i)| i.id),
        }
    }
}

/// The system cursor's image at `scale` pixels per point, straight-alpha RGBA.
fn current(scale: f64) -> Option<PointerImage> {
    autoreleasepool(|_| {
        #[allow(deprecated)]
        let cursor = NSCursor::currentSystemCursor()?;
        let image = cursor.image();
        let hot = cursor.hotSpot();
        let size = image.size();
        let cg: objc2::rc::Retained<CGImage> = unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
        let (w, h) = ((size.width * scale).round() as usize, (size.height * scale).round() as usize);
        let max = proto::stream::MAX_CURSOR_SIZE as usize;
        if w == 0 || h == 0 || w > max || h > max {
            return None;
        }
        let mut rgba = vec![0u8; w * h * 4];
        unsafe {
            let space = ffi::Cf::owned(CGColorSpaceCreateDeviceRGB())?;
            let ctx = ffi::Cf::owned(CGBitmapContextCreate(rgba.as_mut_ptr().cast(), w, h, 8, w * 4, space.0, RGBA_PREMULTIPLIED))?;
            CGContextSetInterpolationQuality(ctx.0, 3);
            let rect = CGRect { origin: CGPoint::default(), size: CGSize { width: w as f64, height: h as f64 } };
            CGContextDrawImage(ctx.0, rect, &*cg as *const CGImage as CFTypeRef);
        }
        // Premultiplied → straight alpha.
        for p in rgba.as_chunks_mut::<4>().0 {
            let a = p[3] as u32;
            if a > 0 && a < 255 {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        // The id follows the image: FNV-1a of the pixels and hotspot (0 is kept for "none").
        let mut id: u32 = 0x811c_9dc5;
        for b in rgba.iter().chain(&[(hot.x * scale) as u8, (hot.y * scale) as u8]) {
            id = (id ^ *b as u32).wrapping_mul(0x0100_0193);
        }
        Some(PointerImage {
            id: id.max(1),
            width: w as u32,
            height: h as u32,
            hot: (((hot.x * scale) as u32).min(w as u32 - 1), ((hot.y * scale) as u32).min(h as u32 - 1)),
            rgba,
        })
    })
}
