//! The macOS menu bar. Standard items use the responder chain (quit, hide,
//! full screen, minimise); ours go through a small Objective-C target that
//! forwards them to the app's event loop.

use std::cell::RefCell;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSObject, NSString};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Connect,
    Disconnect,
    SendFile,
    ToggleStats,
    Settings,
    Mute,
    Microphone,
    WifiSetup,
    Shortcuts,
    CheckUpdates,
    /// Stream the PC's monitor at this place in its list, in the main window or a new one.
    Monitor(u8),
    OpenMonitor(u8),
}

static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

fn send(action: Action) {
    if let Some(p) = PROXY.get() {
        let _ = p.send_event(UserEvent::Menu(action));
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class has no ivars or Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DisplayWireMenuTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(connect:))]
        fn connect(&self, _sender: Option<&AnyObject>) {
            send(Action::Connect);
        }

        #[unsafe(method(disconnect:))]
        fn disconnect(&self, _sender: Option<&AnyObject>) {
            send(Action::Disconnect);
        }

        #[unsafe(method(sendFile:))]
        fn send_file(&self, _sender: Option<&AnyObject>) {
            send(Action::SendFile);
        }

        #[unsafe(method(toggleStats:))]
        fn toggle_stats(&self, _sender: Option<&AnyObject>) {
            send(Action::ToggleStats);
        }

        #[unsafe(method(settings:))]
        fn settings(&self, _sender: Option<&AnyObject>) {
            send(Action::Settings);
        }

        #[unsafe(method(mute:))]
        fn mute(&self, _sender: Option<&AnyObject>) {
            send(Action::Mute);
        }

        /// Called by AppKit as a menu opens: the mute item says what choosing it will do.
        /// Every change to the setting is saved at once, so the saved one is current.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            if item.action() == Some(sel!(mute:)) {
                let title = if crate::settings::Prefs::load().muted { "Unmute Audio" } else { "Mute Audio" };
                item.setTitle(&NSString::from_str(title));
            }
            true
        }

        #[unsafe(method(microphone:))]
        fn microphone(&self, _sender: Option<&AnyObject>) {
            send(Action::Microphone);
        }

        #[unsafe(method(wifiSetup:))]
        fn wifi_setup(&self, _sender: Option<&AnyObject>) {
            send(Action::WifiSetup);
        }

        #[unsafe(method(checkUpdates:))]
        fn check_updates(&self, _sender: Option<&AnyObject>) {
            send(Action::CheckUpdates);
        }

        #[unsafe(method(shortcuts:))]
        fn shortcuts(&self, _sender: Option<&AnyObject>) {
            send(Action::Shortcuts);
        }

        #[unsafe(method(openMonitor:))]
        fn open_monitor(&self, sender: Option<&AnyObject>) {
            if let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) {
                send(Action::OpenMonitor(item.tag() as u8));
            }
        }

        #[unsafe(method(chooseMonitor:))]
        fn choose_monitor(&self, sender: Option<&AnyObject>) {
            if let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) {
                send(Action::Monitor(item.tag() as u8));
            }
        }
    }
);

fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<Sel>,
    key: &str,
    mods: NSEventModifierFlags,
    target: Option<&AnyObject>,
) -> Retained<NSMenuItem> {
    let it = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), action, &NSString::from_str(key))
    };
    it.setKeyEquivalentModifierMask(mods);
    if let Some(t) = target {
        unsafe { it.setTarget(Some(t)) };
    }
    it
}

thread_local! {
    /// View › Monitor, refilled from each `Msg::Monitors`, and the target its items call.
    static MONITOR_MENU: RefCell<Option<(Retained<NSMenu>, Retained<Target>)>> = const { RefCell::new(None) };
}

/// Lists the PC's monitors under View › Monitor, the streamed one ticked; None while not connected.
pub fn set_monitors(mtm: MainThreadMarker, monitors: Option<&proto::stream::Monitors>) {
    MONITOR_MENU.with_borrow(|m| {
        let Some((menu, target)) = m else { return };
        menu.removeAllItems();
        let Some(list) = monitors.filter(|l| l.count > 0) else {
            let it = item(mtm, "Connect to choose", None, "", NSEventModifierFlags::empty(), None);
            it.setEnabled(false);
            menu.addItem(&it);
            return;
        };
        for (i, mon) in list.iter().enumerate() {
            let mut title = format!("{} · {}×{}", mon.name(), mon.width, mon.height);
            if mon.refresh > 0 {
                title += &format!(" · {} Hz", mon.refresh);
            }
            if mon.primary {
                title += " · main";
            }
            let it = item(mtm, &title, Some(sel!(chooseMonitor:)), "", NSEventModifierFlags::empty(), Some(target));
            it.setTag(i as isize);
            it.setState(if i == list.streaming as usize { NSControlStateValueOn } else { NSControlStateValueOff });
            menu.addItem(&it);
        }
        if list.count > 1 {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let header = item(mtm, "Open in New Window", None, "", NSEventModifierFlags::empty(), None);
            header.setEnabled(false);
            menu.addItem(&header);
            for (i, mon) in list.iter().enumerate().filter(|&(i, _)| i != list.streaming as usize) {
                let it = item(mtm, mon.name(), Some(sel!(openMonitor:)), "", NSEventModifierFlags::empty(), Some(target));
                it.setTag(i as isize);
                it.setIndentationLevel(1);
                menu.addItem(&it);
            }
        }
    });
}

fn submenu(mtm: MainThreadMarker, title: &str, items: Vec<Retained<NSMenuItem>>) -> Retained<NSMenuItem> {
    let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title));
    for i in &items {
        menu.addItem(i);
    }
    let holder = NSMenuItem::new(mtm);
    // A nested submenu shows its item's title (the menu bar shows the menu's own).
    holder.setTitle(&NSString::from_str(title));
    holder.setSubmenu(Some(&menu));
    holder
}

/// Replaces the menu bar. Main thread only, once the app has finished launching.
pub fn install(mtm: MainThreadMarker, proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(proxy);
    let target: Retained<Target> = unsafe { msg_send![super(mtm.alloc::<Target>().set_ivars(())), init] };
    let t: &AnyObject = &target;
    let none = NSEventModifierFlags::empty();
    let cmd = NSEventModifierFlags::Command;
    let sep = || NSMenuItem::separatorItem(mtm);

    let app_menu = submenu(
        mtm,
        "DisplayWire",
        vec![
            item(mtm, "About DisplayWire", Some(sel!(orderFrontStandardAboutPanel:)), "", none, None),
            item(mtm, "Check for Updates…", Some(sel!(checkUpdates:)), "", none, Some(t)),
            item(mtm, "Settings…", Some(sel!(settings:)), ",", cmd, Some(t)),
            sep(),
            item(mtm, "Hide DisplayWire", Some(sel!(hide:)), "h", cmd, None),
            item(mtm, "Hide Others", Some(sel!(hideOtherApplications:)), "h", cmd | NSEventModifierFlags::Option, None),
            item(mtm, "Show All", Some(sel!(unhideAllApplications:)), "", none, None),
            sep(),
            item(mtm, "Quit DisplayWire", Some(sel!(terminate:)), "q", cmd, None),
        ],
    );
    let connection = submenu(
        mtm,
        "Connection",
        vec![
            item(mtm, "Connect…", Some(sel!(connect:)), "", none, Some(t)),
            item(mtm, "Disconnect", Some(sel!(disconnect:)), "", none, Some(t)),
            sep(),
            item(mtm, "Send File to PC…", Some(sel!(sendFile:)), "", none, Some(t)),
            sep(),
            item(mtm, "Set Up Wi-Fi Tuning…", Some(sel!(wifiSetup:)), "", none, Some(t)),
        ],
    );
    let monitor_menu = submenu(mtm, "Monitor", Vec::new());
    if let Some(menu) = monitor_menu.submenu() {
        MONITOR_MENU.with_borrow_mut(|m| *m = Some((menu, target.clone())));
    }
    let view = submenu(
        mtm,
        "View",
        vec![
            item(mtm, "Enter Full Screen", Some(sel!(toggleFullScreen:)), "f", cmd | NSEventModifierFlags::Control, None),
            item(mtm, "Show Stats", Some(sel!(toggleStats:)), "s", NSEventModifierFlags::Control | NSEventModifierFlags::Option, Some(t)),
            item(mtm, "Mute Audio", Some(sel!(mute:)), "", none, Some(t)),
            monitor_menu.clone(),
            item(mtm, "Microphone On/Off", Some(sel!(microphone:)), "v", NSEventModifierFlags::Control | NSEventModifierFlags::Option, Some(t)),
        ],
    );
    let window = submenu(
        mtm,
        "Window",
        vec![
            item(mtm, "Minimize", Some(sel!(performMiniaturize:)), "m", cmd, None),
            item(mtm, "Zoom", Some(sel!(performZoom:)), "", none, None),
        ],
    );
    let help = submenu(mtm, "Help", vec![item(mtm, "Keyboard Shortcuts", Some(sel!(shortcuts:)), "", none, Some(t))]);

    let bar = NSMenu::new(mtm);
    for m in [&app_menu, &connection, &view, &window, &help] {
        bar.addItem(m);
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&bar));
    set_monitors(mtm, None);
    // AppKit doesn't retain a menu item's target: keep ours for the life of the app.
    std::mem::forget(target);
}
