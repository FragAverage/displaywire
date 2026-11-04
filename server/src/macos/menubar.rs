//! The menu-bar icon, the Mac's counterpart of the Windows tray: who's connected, pairing a new
//! device (with the Allow question and the PIN as dialogs), the paired devices, the permissions
//! the server needs, starting at login, and quitting. It owns the main thread (AppKit's), so the
//! main stream runs on another.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSModalResponseAbort, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn, NSImage,
    NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use timing::now_ns;

use super::{agent, cg, input};
use crate::Shared;
use crate::pairing::{Outcome, View};

/// Which pairing dialog was last shown, so each is shown once.
#[derive(Default, PartialEq)]
enum Shown {
    #[default]
    Nothing,
    Asked(String),
    Pin(String),
}

pub struct Ivars {
    shared: Arc<Shared>,
    item: RefCell<Option<Retained<NSStatusItem>>>,
    shown: RefCell<Shown>,
    last_outcome: RefCell<Option<Outcome>>,
    /// The dialog that's up, if any (the timer keeps firing inside its modal loop).
    modal: RefCell<Option<Shown>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; used on the main thread only.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DisplayWireMenuBar"]
    #[ivars = Ivars]
    struct MenuBar;

    unsafe impl NSObjectProtocol for MenuBar {}

    unsafe impl NSMenuDelegate for MenuBar {
        /// Rebuilt each time it opens, so it's always current.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.fill(menu);
        }
    }

    impl MenuBar {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: Option<&AnyObject>) {
            self.update_icon();
            if self.ivars().modal.borrow().is_none() {
                self.pairing_dialogs();
            } else if self.dialog_is_stale() {
                // Pairing moved on: close the dialog in the way, and the next tick shows what's next.
                NSApplication::sharedApplication(self.mtm()).abortModal();
            }
        }

        #[unsafe(method(pair:))]
        fn pair(&self, _sender: Option<&AnyObject>) {
            self.ivars().shared.pairing.start(now_ns());
            self.alert(
                "Ready to pair",
                "On the other computer, open DisplayWire and choose this Mac (or add it by its address). This Mac \
                 accepts a new device for the next two minutes and asks you before it does.",
                &["OK"],
            );
        }

        #[unsafe(method(removeDevice:))]
        fn remove_device(&self, sender: Option<&AnyObject>) {
            let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) else { return };
            let Some(id) = item.representedObject().and_then(|o| o.downcast::<NSString>().ok()) else { return };
            let id = id.to_string();
            let name = self.ivars().shared.clients.lock().unwrap().get(&id).map(|c| c.name.clone()).unwrap_or_default();
            if self.alert(&format!("Remove “{name}”?"), "Its key stops working. To use it again, pair it again.", &["Remove", "Cancel"]) {
                match self.ivars().shared.clients.lock().unwrap().remove(&id) {
                    Ok(_) => println!("removed \"{name}\"; its key no longer works"),
                    Err(e) => println!("can't remove \"{name}\": {e}"),
                }
            }
        }

        #[unsafe(method(openFiles:))]
        fn open_files(&self, _sender: Option<&AnyObject>) {
            if let Ok(dir) = crate::transfer::platform::uploads_dir() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::process::Command::new("/usr/bin/open").arg(dir).spawn();
            }
        }

        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: Option<&AnyObject>) {
            let log = agent::log_path();
            if log.exists() {
                let _ = std::process::Command::new("/usr/bin/open").args(["-a", "Console"]).arg(log).spawn();
            } else {
                self.alert("No log yet", "The log is kept when the server starts at login. Started from Terminal, it writes there instead.", &["OK"]);
            }
        }

        #[unsafe(method(allowScreen:))]
        fn allow_screen(&self, _sender: Option<&AnyObject>) {
            cg::request_screen_capture();
            open_settings("Privacy_ScreenCapture");
        }

        #[unsafe(method(allowInput:))]
        fn allow_input(&self, _sender: Option<&AnyObject>) {
            input::request_accessibility();
            open_settings("Privacy_Accessibility");
        }

        #[unsafe(method(setResolution:))]
        fn set_resolution(&self, sender: Option<&AnyObject>) {
            let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) else { return };
            let height = item.tag().max(0) as u32;
            super::MAX_HEIGHT.store(height, Relaxed);
            println!("resolution: {}", if height == 0 { "native".to_string() } else { format!("up to {height} pixels high") });
        }

        #[unsafe(method(setUpWifi:))]
        fn set_up_wifi(&self, _sender: Option<&AnyObject>) {
            match transport::awdl::install_rule() {
                Ok(()) => {
                    self.alert(
                        "Wi-Fi tuning is on",
                        "From the next connection, this Mac pauses AirDrop and AirPlay's Wi-Fi link while a device is connected, \
                         and brings it back when it disconnects.",
                        &["OK"],
                    );
                }
                Err(e) if e == "cancelled" => {}
                Err(e) => {
                    self.alert("Couldn't set up Wi-Fi tuning", &e, &["OK"]);
                }
            }
        }

        #[unsafe(method(toggleLogin:))]
        fn toggle_login(&self, _sender: Option<&AnyObject>) {
            let result = if agent::installed() { agent::uninstall() } else { agent::install() };
            if let Err(e) = result {
                self.alert("Start at Login", &e, &["OK"]);
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            super::exit(0);
        }

        #[unsafe(method(restart:))]
        fn restart(&self, _sender: Option<&AnyObject>) {
            super::restart();
        }
    }
);

fn open_settings(pane: &str) {
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
}

/// A menu item: disabled (a line of text) without an action.
fn item(mtm: MainThreadMarker, title: &str, action: Option<Sel>, target: Option<&AnyObject>) -> Retained<NSMenuItem> {
    let it = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), action, &NSString::from_str("")) };
    if let Some(t) = target {
        unsafe { it.setTarget(Some(t)) };
    }
    it.setEnabled(action.is_some());
    it
}

/// The addresses another computer can reach this one at: whichever this Mac would send from.
fn local_addresses() -> Vec<String> {
    let mut v = Vec::new();
    for target in ["192.168.1.1:9", "10.0.0.1:9", "100.100.100.100:9"] {
        if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
            && s.connect(target).is_ok()
            && let Ok(a) = s.local_addr()
        {
            let ip = a.ip().to_string();
            if !ip.starts_with("0.") && !v.contains(&ip) {
                v.push(ip);
            }
        }
    }
    v
}

impl MenuBar {
    fn new(mtm: MainThreadMarker, shared: Arc<Shared>) -> Retained<MenuBar> {
        let this = mtm.alloc::<MenuBar>().set_ivars(Ivars {
            shared,
            item: RefCell::new(None),
            shown: RefCell::default(),
            last_outcome: RefCell::new(None),
            modal: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// A dialog in front of everything; true for the first button.
    fn alert(&self, title: &str, text: &str, buttons: &[&str]) -> bool {
        self.dialog(Shown::Nothing, title, text, buttons) == Some(true)
    }

    /// Whether the dialog that's up no longer fits where pairing is: a device asking to pair
    /// replaces any other dialog, and the Allow and PIN dialogs go once their step is over.
    fn dialog_is_stale(&self) -> bool {
        let view = self.ivars().shared.pairing.snapshot(now_ns()).view;
        match (&*self.ivars().modal.borrow(), &view) {
            (Some(Shown::Nothing), View::Asking { .. }) => true,
            (Some(Shown::Asked(n)), View::Asking { name, .. }) => n != name,
            (Some(Shown::Pin(p)), View::Pin { pin, .. }) => p != pin,
            (Some(Shown::Asked(_) | Shown::Pin(_)), _) => true,
            _ => false,
        }
    }

    /// A dialog about `kind`: Some(true) for the first button, None if it was closed because
    /// pairing moved on (see `dialog_is_stale`).
    fn dialog(&self, kind: Shown, title: &str, text: &str, buttons: &[&str]) -> Option<bool> {
        let mtm = self.mtm();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        for b in buttons {
            alert.addButtonWithTitle(&NSString::from_str(b));
        }
        NSApplication::sharedApplication(mtm).activate();
        let outer = self.ivars().modal.replace(Some(kind));
        let response = alert.runModal();
        *self.ivars().modal.borrow_mut() = outer;
        (response != NSModalResponseAbort).then_some(response == NSAlertFirstButtonReturn)
    }

    /// The icon, different while a device is connected; its tooltip says who.
    fn update_icon(&self) {
        let item = self.ivars().item.borrow();
        let Some(item) = item.as_ref() else { return };
        let connected = self.ivars().shared.link.lock().unwrap().is_some();
        let symbol = if connected { "display.and.arrow.down" } else { "display" };
        if let Some(button) = item.button(self.mtm()) {
            if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), Some(&NSString::from_str("DisplayWire")))
            {
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
            button.setToolTip(Some(&NSString::from_str(&format!("DisplayWire Server\n{}", self.status()))));
        }
    }

    fn status(&self) -> String {
        let shared = &self.ivars().shared;
        let link = shared.link.lock().unwrap();
        match link.as_ref() {
            Some(link) => {
                let name = shared.clients.lock().unwrap().get(&link.client).map_or_else(|| link.addr.ip().to_string(), |c| c.name.clone());
                let (fps, mbps) = (shared.fps.load(Relaxed), shared.mbps_x10.load(Relaxed) as f32 / 10.0);
                format!("Connected: {name} · {fps} fps, {mbps:.0} Mbps")
            }
            None => "Waiting for a device".into(),
        }
    }

    /// The pairing questions as dialogs: allow this device? then the PIN to type on it.
    fn pairing_dialogs(&self) {
        let shared = self.ivars().shared.clone();
        let snap = shared.pairing.snapshot(now_ns());
        match &snap.view {
            View::Asking { name, addr, .. } if *self.ivars().shown.borrow() != Shown::Asked(name.clone()) => {
                *self.ivars().shown.borrow_mut() = Shown::Asked(name.clone());
                let text = format!("{name} ({addr}) wants to pair with this Mac. Allow it only if it's yours.");
                match self.dialog(Shown::Asked(name.clone()), &format!("Allow “{name}” to connect?"), &text, &["Allow", "Don't Allow"]) {
                    Some(true) => shared.pairing.allow(now_ns()),
                    Some(false) => shared.pairing.deny(now_ns()),
                    None => {}
                }
            }
            View::Pin { name, pin, .. } if *self.ivars().shown.borrow() != Shown::Pin(pin.clone()) => {
                *self.ivars().shown.borrow_mut() = Shown::Pin(pin.clone());
                let (a, b) = pin.split_at(pin.len() / 2);
                // It closes by itself once the PIN has been typed (or pairing ends).
                if self.dialog(Shown::Pin(pin.clone()), &format!("Type this PIN on {name}"), &format!("{a} {b}"), &["Cancel"]) == Some(true) {
                    shared.pairing.stop(now_ns());
                }
            }
            View::Off | View::Listening { .. } => *self.ivars().shown.borrow_mut() = Shown::Nothing,
            _ => {}
        }
        let snap = shared.pairing.snapshot(now_ns());
        if snap.last != *self.ivars().last_outcome.borrow() {
            *self.ivars().last_outcome.borrow_mut() = snap.last.clone();
            match &snap.last {
                Some(Outcome::Paired(name)) => {
                    self.alert(&format!("Paired with {name}"), "It can connect to this Mac now.", &["OK"]);
                }
                Some(Outcome::Locked(name)) => {
                    self.alert("Pairing stopped", &format!("Too many wrong PINs from {name}."), &["OK"]);
                }
                _ => {}
            }
        }
    }

    fn fill(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        let t: &AnyObject = self.as_ref();
        menu.removeAllItems();
        menu.addItem(&item(mtm, "DisplayWire Server", None, None));
        menu.addItem(&item(mtm, &self.status(), None, None));
        for a in local_addresses() {
            menu.addItem(&item(mtm, &format!("This Mac: {a}"), None, None));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&item(mtm, "Pair New Device…", Some(sel!(pair:)), Some(t)));
        let devices = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Paired Devices"));
        let clients = self.ivars().shared.clients.lock().unwrap().list().to_vec();
        if clients.is_empty() {
            devices.addItem(&item(mtm, "None yet", None, None));
        }
        for c in clients {
            let it = item(mtm, &format!("Remove “{}”…", c.name), Some(sel!(removeDevice:)), Some(t));
            unsafe { it.setRepresentedObject(Some(&NSString::from_str(&c.id))) };
            devices.addItem(&it);
        }
        let holder = item(mtm, "Paired Devices", None, None);
        holder.setEnabled(true);
        holder.setSubmenu(Some(&devices));
        menu.addItem(&holder);
        let sizes = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Resolution"));
        let current = super::MAX_HEIGHT.load(Relaxed);
        for (title, height) in [("Native (the display's own size)", 0), ("4K (2160p)", 2160), ("1440p", 1440), ("1080p", 1080)] {
            let it = item(mtm, title, Some(sel!(setResolution:)), Some(t));
            it.setTag(height as isize);
            it.setState(if height == current { NSControlStateValueOn } else { NSControlStateValueOff });
            sizes.addItem(&it);
        }
        let holder = item(mtm, "Resolution", None, None);
        holder.setEnabled(true);
        holder.setSubmenu(Some(&sizes));
        menu.addItem(&holder);
        menu.addItem(&item(mtm, "Open Received Files", Some(sel!(openFiles:)), Some(t)));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        if cg::screen_capture_allowed() {
            menu.addItem(&item(mtm, "Screen Recording: allowed", None, None));
        } else {
            menu.addItem(&item(mtm, "Allow Screen Recording…", Some(sel!(allowScreen:)), Some(t)));
            // macOS applies the permission to a new process only.
            menu.addItem(&item(mtm, "Restart After Allowing It", Some(sel!(restart:)), Some(t)));
        }
        if input::accessibility_allowed() {
            menu.addItem(&item(mtm, "Keyboard and mouse control: allowed", None, None));
        } else {
            menu.addItem(&item(mtm, "Allow Keyboard and Mouse Control…", Some(sel!(allowInput:)), Some(t)));
        }
        if transport::awdl::ready() {
            menu.addItem(&item(mtm, "Wi-Fi tuning: on while connected", None, None));
        } else {
            menu.addItem(&item(mtm, "Smoother Wi-Fi…", Some(sel!(setUpWifi:)), Some(t)));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let login = item(mtm, "Start at Login", Some(sel!(toggleLogin:)), Some(t));
        login.setState(if agent::installed() { NSControlStateValueOn } else { NSControlStateValueOff });
        menu.addItem(&login);
        menu.addItem(&item(mtm, "Open Log", Some(sel!(openLog:)), Some(t)));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&item(mtm, "Quit DisplayWire Server", Some(sel!(quit:)), Some(t)));
    }
}

/// Puts the icon in the menu bar and runs AppKit on this (the main) thread, for good.
pub fn run(shared: Arc<Shared>) -> ! {
    let mtm = MainThreadMarker::new().expect("the menu bar runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // A menu-bar app: no Dock icon, no main menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let bar = MenuBar::new(mtm, shared);
    let status_item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::new(mtm);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*bar)));
    status_item.setMenu(Some(&menu));
    *bar.ivars().item.borrow_mut() = Some(status_item);
    bar.update_icon();
    // In the common modes, so it keeps firing while a dialog or the menu is open.
    let timer = unsafe { NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(0.5, &bar, sel!(tick:), None, true) };
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    println!("menu bar: icon ready");
    app.run();
    std::process::exit(0)
}
