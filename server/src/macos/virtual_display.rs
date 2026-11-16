//! A display that exists only for a device to show: macOS lays the desktop out over it like any
//! other monitor, and its picture is captured like one. Made with `CGVirtualDisplay`, which Apple
//! doesn't document (the classes are in CoreGraphics; other extend-the-desktop apps use them the
//! same way), so every step is checked and a refusal is an error, not a crash. The display goes
//! when this is dropped, or with the process.

use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, sel};
use objc2_foundation::{NSArray, NSSize, NSString};

use super::cg;

pub struct VirtualDisplay {
    _display: Retained<AnyObject>,
    pub id: u32,
}

// Only kept and dropped; the display itself lives in the window server.
unsafe impl Send for VirtualDisplay {}

fn class(name: &std::ffi::CStr, selectors: &[objc2::runtime::Sel]) -> Result<&'static AnyClass, String> {
    let class = AnyClass::get(name).ok_or("this macOS has no virtual displays")?;
    if selectors.iter().all(|&s| class.instance_method(s).is_some()) { Ok(class) } else { Err("this macOS's virtual displays have changed".into()) }
}

impl VirtualDisplay {
    /// A `width`×`height` display at `hz`, called `name` in System Settings › Displays.
    pub fn new(name: &str, width: u32, height: u32, hz: u32) -> Result<VirtualDisplay, String> {
        let descriptor = class(
            c"CGVirtualDisplayDescriptor",
            &[sel!(setQueue:), sel!(setName:), sel!(setMaxPixelsWide:), sel!(setMaxPixelsHigh:), sel!(setSizeInMillimeters:), sel!(setProductID:), sel!(setVendorID:), sel!(setSerialNum:)],
        )?;
        let mode = class(c"CGVirtualDisplayMode", &[sel!(initWithWidth:height:refreshRate:)])?;
        let settings = class(c"CGVirtualDisplaySettings", &[sel!(setModes:), sel!(setHiDPI:)])?;
        let display = class(c"CGVirtualDisplay", &[sel!(initWithDescriptor:), sel!(applySettings:), sel!(displayID)])?;
        // SAFETY: the selectors exist (checked above), with the types other users of these classes pass.
        unsafe {
            let d: Retained<AnyObject> = msg_send![descriptor, new];
            let _: () = msg_send![&*d, setQueue: DispatchQueue::main()];
            let _: () = msg_send![&*d, setName: &*NSString::from_str(name)];
            let _: () = msg_send![&*d, setMaxPixelsWide: width];
            let _: () = msg_send![&*d, setMaxPixelsHigh: height];
            // About 100 pixels to the inch: text at its usual size, as on a desktop monitor.
            let _: () = msg_send![&*d, setSizeInMillimeters: NSSize::new(width as f64 * 0.254, height as f64 * 0.254)];
            // Any numbers do; the same ones each time let macOS remember where the display was put.
            let _: () = msg_send![&*d, setVendorID: 0x4457u32];
            let _: () = msg_send![&*d, setProductID: 0x5456u32];
            let _: () = msg_send![&*d, setSerialNum: 1u32];
            let allocated: objc2::rc::Allocated<AnyObject> = msg_send![display, alloc];
            let display: Option<Retained<AnyObject>> = msg_send![allocated, initWithDescriptor: &*d];
            let display = display.ok_or("macOS wouldn't make a virtual display")?;

            let allocated: objc2::rc::Allocated<AnyObject> = msg_send![mode, alloc];
            let mode: Option<Retained<AnyObject>> = msg_send![allocated, initWithWidth: width, height: height, refreshRate: hz as f64];
            let mode = mode.ok_or("macOS wouldn't make a virtual display mode")?;
            let s: Retained<AnyObject> = msg_send![settings, new];
            let _: () = msg_send![&*s, setHiDPI: 0u32];
            let _: () = msg_send![&*s, setModes: &*NSArray::from_retained_slice(&[mode])];
            let applied: bool = msg_send![&*display, applySettings: &*s];
            if !applied {
                return Err(format!("macOS wouldn't set the virtual display to {width}x{height}"));
            }
            let id: u32 = msg_send![&*display, displayID];
            // It joins the desktop a moment later.
            let begin = Instant::now();
            while !cg::displays().contains(&id) {
                if begin.elapsed() > Duration::from_secs(3) {
                    return Err("the virtual display didn't join the desktop".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(VirtualDisplay { _display: display, id })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adds a display to this Mac's desktop for a moment: `cargo test -- --ignored virtual`.
    #[test]
    #[ignore]
    fn a_virtual_display_joins_the_desktop_and_leaves_it() {
        let display = VirtualDisplay::new("DisplayWire test", 1920, 1080, 60).expect("made");
        let id = display.id;
        assert!(cg::displays().contains(&id));
        assert_eq!(cg::pixel_size(id), (1920, 1080, 60));
        drop(display);
        let begin = Instant::now();
        while cg::displays().contains(&id) && begin.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!cg::displays().contains(&id), "still there after being dropped");
    }
}
