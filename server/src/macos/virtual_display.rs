//! A virtual display, like Parsec's: an extra screen macOS extends the desktop onto, which the
//! client opens in its own window (View › Monitor › Open in New Window) on one of its monitors.
//! It exists while a client is connected, when switched on in the menu bar.
//!
//! macOS has no public API for this; `CGVirtualDisplay` (CoreGraphics, private, used by
//! Parsec, BetterDisplay and DeskPad) is reached through the Objective-C runtime. If it's
//! missing, the menu item says so and nothing else changes.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSString};

/// Modes offered in System Settings › Displays; the first is used until another is picked.
/// 1:1 pixels (no HiDPI): what's drawn is what's streamed.
const MODES: [(u32, u32); 8] =
    [(1920, 1080), (2560, 1440), (3840, 2160), (1920, 1200), (2560, 1600), (1680, 1050), (1600, 900), (1280, 720)];
const REFRESH: [f64; 1] = [60.0];

/// How long it stays after the client leaves, so a quick reconnect finds it where it was.
pub const LINGER_NS: u64 = 10_000_000_000;

static ON: AtomicBool = AtomicBool::new(false);

struct Live {
    _display: Retained<AnyObject>,
    _queue: DispatchRetained<DispatchQueue>,
    id: u32,
}

// SAFETY: the display object is only kept alive (and released) here; CoreGraphics runs it on its queue.
unsafe impl Send for Live {}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

fn setting_path() -> PathBuf {
    crate::auth::default_dir().join("virtual-display")
}

/// Whether the private API is there.
pub fn available() -> bool {
    ["CGVirtualDisplay", "CGVirtualDisplayDescriptor", "CGVirtualDisplaySettings", "CGVirtualDisplayMode"]
        .iter()
        .all(|n| AnyClass::get(&std::ffi::CString::new(*n).unwrap()).is_some())
}

/// The menu bar setting, remembered across restarts.
pub fn enabled() -> bool {
    ON.load(Relaxed)
}

pub fn load_setting() {
    ON.store(setting_path().exists(), Relaxed);
}

pub fn set_enabled(on: bool) {
    ON.store(on, Relaxed);
    let path = setting_path();
    let _ = if on {
        std::fs::create_dir_all(path.parent().unwrap()).and_then(|()| std::fs::write(&path, b"on\n"))
    } else {
        std::fs::remove_file(&path)
    };
}

/// The virtual display's id, while it exists.
pub fn current() -> Option<u32> {
    LIVE.lock().unwrap().as_ref().map(|l| l.id)
}

/// Adds the display if it isn't there.
pub fn create() -> Result<u32, String> {
    let mut live = LIVE.lock().unwrap();
    if let Some(l) = live.as_ref() {
        return Ok(l.id);
    }
    if !available() {
        return Err("this version of macOS doesn't offer virtual displays".into());
    }
    let class = |n: &std::ffi::CStr| AnyClass::get(n).ok_or_else(|| format!("{n:?} is missing"));
    let (max_w, max_h) = MODES.iter().fold((0, 0), |(w, h), &(mw, mh)| (w.max(mw), h.max(mh)));
    let queue = DispatchQueue::new("displaywire.virtual-display", None);
    unsafe {
        let desc: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplayDescriptor")?, new];
        let queue_obj = &*queue as *const DispatchQueue as *mut AnyObject;
        let () = msg_send![&*desc, setQueue: queue_obj];
        let () = msg_send![&*desc, setName: &*NSString::from_str("DisplayWire Virtual Display")];
        let () = msg_send![&*desc, setMaxPixelsWide: max_w];
        let () = msg_send![&*desc, setMaxPixelsHigh: max_h];
        // About a 24" 16:9 panel, so macOS picks a sensible default scale.
        let () = msg_send![&*desc, setSizeInMillimeters: CGSize { width: 530.0, height: 300.0 }];
        // Fixed ids, so macOS remembers where it was arranged.
        let () = msg_send![&*desc, setVendorID: 0x4457u32];
        let () = msg_send![&*desc, setProductID: 0x0001u32];
        let () = msg_send![&*desc, setSerialNum: 0x0001u32];

        let display: Option<Retained<AnyObject>> =
            msg_send![class(c"CGVirtualDisplay")?.alloc_any(), initWithDescriptor: &*desc];
        let display = display.ok_or("macOS refused to create the virtual display")?;

        let mode_class = class(c"CGVirtualDisplayMode")?;
        let mut modes = Vec::new();
        for &(w, h) in &MODES {
            for &hz in &REFRESH {
                let m: Option<Retained<AnyObject>> =
                    msg_send![mode_class.alloc_any(), initWithWidth: w, height: h, refreshRate: hz];
                modes.extend(m);
            }
        }
        let settings: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplaySettings")?, new];
        let () = msg_send![&*settings, setHiDPI: 0u32];
        let () = msg_send![&*settings, setModes: &*NSArray::from_retained_slice(&modes)];
        let ok: Bool = msg_send![&*display, applySettings: &*settings];
        if !ok.as_bool() {
            return Err("macOS refused the virtual display's modes".into());
        }
        let id: u32 = msg_send![&*display, displayID];
        println!("virtual display: added (display {id})");
        *live = Some(Live { _display: display, _queue: queue, id });
        Ok(id)
    }
}

/// A client connected: adds it now if it's switched on (the background thread would, a moment
/// later, after the client had already asked for its window).
pub fn on_connect() {
    if enabled()
        && let Err(e) = create()
    {
        println!("virtual display: {e}");
    }
}

/// Removes it; the windows on it move to the other screens.
pub fn remove() {
    if let Some(l) = LIVE.lock().unwrap().take() {
        println!("virtual display: removed (display {})", l.id);
    }
}

trait AllocAny {
    /// `[Class alloc]` for a class only known at run time.
    fn alloc_any(&self) -> objc2::rc::Allocated<AnyObject>;
}

impl AllocAny for AnyClass {
    fn alloc_any(&self) -> objc2::rc::Allocated<AnyObject> {
        unsafe { msg_send![self, alloc] }
    }
}

#[cfg(test)]
mod tests {
    /// Adds a real display for a few seconds: `cargo test -p displaywire-server -- --ignored virtual_display`.
    #[test]
    #[ignore]
    fn virtual_display_appears() {
        let id = super::create().expect("create");
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(super::super::ffi::displays().contains(&id), "display {id} isn't active");
        assert!(super::super::ffi::display_mode(id).is_some());
        println!("mode: {:?}", super::super::ffi::display_mode(id));
        super::remove();
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(!super::super::ffi::displays().contains(&id));
    }
}
