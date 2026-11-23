//! Where files dropped on the client's window go: the folder under the pointer, as if they
//! had been dragged there on the Mac. That is a Finder window's folder or the Desktop;
//! anywhere else (another app's window) has no folder, and the caller decides.

use std::path::PathBuf;

use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use super::ffi::{self, CFTypeRef};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
}

const ON_SCREEN_ONLY: u32 = 1;

/// The pointer's position, in global points (top-left origin).
fn pointer() -> Option<(f64, f64)> {
    unsafe {
        let e = ffi::CGEventCreate(std::ptr::null_mut());
        if e.is_null() {
            return None;
        }
        let p = ffi::CGEventGetLocation(e);
        ffi::CFRelease(e);
        Some((p.x, p.y))
    }
}

/// The frontmost window under the point: its owner and layer (front to back, as listed).
fn window_at(x: f64, y: f64) -> Option<(String, i64)> {
    unsafe {
        let list = ffi::Cf::owned(CGWindowListCopyWindowInfo(ON_SCREEN_ONLY, 0))?;
        let list = &*(list.0 as *const NSArray<NSDictionary<NSString, objc2::runtime::AnyObject>>);
        let num = |d: &NSDictionary<NSString, objc2::runtime::AnyObject>, k: &str| -> Option<f64> {
            let v = d.objectForKey(&NSString::from_str(k))?;
            Some((*(&*v as *const objc2::runtime::AnyObject as *const NSNumber)).doubleValue())
        };
        for w in list.iter() {
            let Some(b) = w.objectForKey(&NSString::from_str("kCGWindowBounds")) else { continue };
            let b = &*(&*b as *const objc2::runtime::AnyObject as *const NSDictionary<NSString, objc2::runtime::AnyObject>);
            let (Some(bx), Some(by), Some(bw), Some(bh)) = (num(b, "X"), num(b, "Y"), num(b, "Width"), num(b, "Height")) else { continue };
            if !(x >= bx && y >= by && x < bx + bw && y < by + bh) {
                continue;
            }
            let layer = num(&w, "kCGWindowLayer").unwrap_or(0.0) as i64;
            // Menus, the Dock, overlays: not somewhere to drop.
            if layer > 0 {
                continue;
            }
            let owner = w
                .objectForKey(&NSString::from_str("kCGWindowOwnerName"))
                .map(|o| (*(&*o as *const objc2::runtime::AnyObject as *const NSString)).to_string())
                .unwrap_or_default();
            if (owner.is_empty() || owner == "Window Server") && layer == 0 {
                continue;
            }
            return Some((owner, layer));
        }
        None
    }
}

/// The folder of the Finder window at the point (they're asked front to back).
fn finder_window_folder(x: f64, y: f64) -> Option<PathBuf> {
    let script = format!(
        r#"tell application "Finder"
    set ws to every Finder window
    repeat with i from 1 to count of ws
        set w to item i of ws
        set b to bounds of w
        if {x} ≥ (item 1 of b) and {x} < (item 3 of b) and {y} ≥ (item 2 of b) and {y} < (item 4 of b) then
            return POSIX path of ((target of w) as alias)
        end if
    end repeat
end tell
return """#,
        x = x.round() as i64,
        y = y.round() as i64
    );
    let out = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).output().ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        println!("drop: asking Finder failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    (!path.is_empty()).then(|| PathBuf::from(path)).filter(|p| p.is_dir())
}

/// The folder under the pointer, if it's somewhere files can be dropped.
pub fn folder_under_pointer() -> Option<PathBuf> {
    let (x, y) = pointer()?;
    let (owner, layer) = window_at(x, y)?;
    if owner != "Finder" {
        println!("drop: under the pointer is {owner}, which isn't a folder");
        return None;
    }
    if layer < 0 {
        // Finder's desktop window.
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Desktop"));
    }
    finder_window_folder(x, y)
}

#[cfg(test)]
mod tests {
    /// Prints what's under the pointer now: `cargo test -p displaywire-server -- --ignored --nocapture drop_target`.
    #[test]
    #[ignore]
    fn drop_target() {
        println!("pointer {:?}, window {:?}", super::pointer(), super::pointer().and_then(|(x, y)| super::window_at(x, y)));
        println!("folder: {:?}", super::folder_under_pointer());
    }
}
