//! The menu-bar item, the Mac's counterpart of the Windows tray icon: the connection status,
//! the addresses to connect to, received files, pairing and the device window, starting at
//! login, and shortcuts to the two permissions the server needs. It owns the main thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSModalResponseOK, NSOpenPanel, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, ns_string};

use crate::Shared;
use crate::pairing::View;

static UI_LAUNCHED_NS: AtomicU64 = AtomicU64::new(0);

pub struct State {
    shared: Arc<Shared>,
    exe: PathBuf,
    addresses: Vec<String>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the ivars are plain Rust data.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DisplayWireServerMenu"]
    #[ivars = State]
    struct Menu;

    unsafe impl NSObjectProtocol for Menu {}

    unsafe impl NSApplicationDelegate for Menu {
        /// The app opened again (from Finder or Launchpad) while it's running: show the
        /// permissions window, the one thing there is to see.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            super::setup::open();
            false
        }
    }

    unsafe impl NSMenuDelegate for Menu {
        /// Rebuilt each time it opens, so the status is current.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.fill(menu);
        }
    }

    impl Menu {
        #[unsafe(method(copyAddress:))]
        fn copy_address(&self, sender: Option<&NSMenuItem>) {
            if let Some(a) = sender.and_then(|s| self.ivars().addresses.get(s.tag() as usize)) {
                super::shell::set_clipboard_text(a);
            }
        }

        #[unsafe(method(openFiles:))]
        fn open_files(&self, _sender: Option<&AnyObject>) {
            if let Ok(dir) = super::shell::uploads_dir() {
                let _ = std::fs::create_dir_all(&dir);
                super::shell::open(&dir);
            }
        }

        #[unsafe(method(sendFile:))]
        fn send_file(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(true);
            panel.setCanChooseDirectories(false);
            panel.setAllowsMultipleSelection(true);
            panel.setPrompt(Some(ns_string!("Send")));
            panel.setMessage(Some(&NSString::from_str(&format!("Choose files to send to {}", device_name(&self.ivars().shared)))));
            // A menu-bar app isn't frontmost: bring the panel up in front of everything.
            #[allow(deprecated)]
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
            if panel.runModal() != NSModalResponseOK {
                return;
            }
            for url in panel.URLs().iter() {
                if let Some(path) = url.path() {
                    let path = PathBuf::from(path.to_string());
                    if let Err(e) = crate::transfer::Transfers::send_file(&path) {
                        super::shell::notify(&e);
                    }
                }
            }
        }

        #[unsafe(method(pair:))]
        fn pair(&self, _sender: Option<&AnyObject>) {
            self.ivars().shared.pairing.start(timing::now_ns());
            open_ui(&self.ivars().shared, &self.ivars().exe);
        }

        #[unsafe(method(manage:))]
        fn manage(&self, _sender: Option<&AnyObject>) {
            open_ui(&self.ivars().shared, &self.ivars().exe);
        }

        #[unsafe(method(toggleLogin:))]
        fn toggle_login(&self, _sender: Option<&AnyObject>) {
            let r = if super::agent::installed() {
                super::agent::uninstall(true)
            } else {
                // This one keeps running; launchd starts the next one at login.
                let args: Vec<_> = std::env::args_os().skip(1).filter(|a| a != "--run").collect();
                super::agent::install(&args, false).map(|()| {
                    if !super::agent::running_as_agent() {
                        super::shell::notify("DisplayWire Server will start when you log in.");
                    }
                })
            };
            if let Err(e) = r {
                super::shell::notify(&format!("Start at login: {e}"));
            }
        }

        #[unsafe(method(toggleVirtual:))]
        fn toggle_virtual(&self, _sender: Option<&AnyObject>) {
            super::virtual_display::set_enabled(!super::virtual_display::enabled());
        }

        #[unsafe(method(togglePcShortcuts:))]
        fn toggle_pc_shortcuts(&self, _sender: Option<&AnyObject>) {
            super::input::set_pc_shortcuts(!super::input::pc_shortcuts());
        }

        #[unsafe(method(openSetup:))]
        fn open_setup(&self, _sender: Option<&AnyObject>) {
            super::setup::open();
        }

        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: Option<&AnyObject>) {
            if let Ok(log) = super::agent::log_path() {
                let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(log).spawn();
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            // A successful exit: the LaunchAgent leaves it stopped until the next login.
            std::process::exit(0);
        }
    }
);

impl Menu {
    fn new(mtm: MainThreadMarker, state: State) -> Retained<Menu> {
        let this = mtm.alloc::<Menu>().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }

    fn item(&self, menu: &NSMenu, title: &str, action: Option<Sel>, tag: isize) -> Retained<NSMenuItem> {
        let mtm = self.mtm();
        let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), action, ns_string!("")) };
        if action.is_some() {
            unsafe { item.setTarget(Some(self)) };
        } else {
            item.setEnabled(false);
        }
        item.setTag(tag);
        menu.addItem(&item);
        item
    }

    fn fill(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let st = self.ivars();
        for line in status(&st.shared).lines() {
            self.item(menu, line, None, 0);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for (i, a) in st.addresses.iter().enumerate() {
            self.item(menu, &format!("Copy Address {a}"), Some(sel!(copyAddress:)), i as isize);
        }
        if st.shared.link.lock().unwrap().is_some() {
            self.item(menu, &format!("Send Files to {}…", device_name(&st.shared)), Some(sel!(sendFile:)), 0);
        }
        self.item(menu, "Open Received Files", Some(sel!(openFiles:)), 0);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.item(menu, "Pair New Device…", Some(sel!(pair:)), 0);
        self.item(menu, "Manage Devices…", Some(sel!(manage:)), 0);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let screen = super::screen_recording_allowed_quiet();
        let input = super::accessibility_allowed(false);
        if !screen || !input {
            self.item(menu, "⚠︎ Permissions Needed: Set Up…", Some(sel!(openSetup:)), 0);
        } else {
            self.item(menu, "Permissions…", Some(sel!(openSetup:)), 0);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let pc = self.item(menu, "PC Shortcuts (Ctrl Works as ⌘)", Some(sel!(togglePcShortcuts:)), 0);
        pc.setState(if super::input::pc_shortcuts() { NSControlStateValueOn } else { NSControlStateValueOff });
        if super::virtual_display::available() {
            let v = self.item(menu, "Virtual Display While Connected", Some(sel!(toggleVirtual:)), 0);
            v.setState(if super::virtual_display::enabled() { NSControlStateValueOn } else { NSControlStateValueOff });
        }
        let login = self.item(menu, "Start at Login", Some(sel!(toggleLogin:)), 0);
        login.setState(if super::agent::installed() { NSControlStateValueOn } else { NSControlStateValueOff });
        self.item(menu, "Show Log", Some(sel!(openLog:)), 0);
        self.item(menu, "Quit DisplayWire Server", Some(sel!(quit:)), 0);
    }
}

fn status(shared: &Shared) -> String {
    match shared.link.lock().unwrap().as_ref() {
        Some(link) => {
            let fps = shared.fps.load(Relaxed);
            let mbps = shared.mbps_x10.load(Relaxed) as f32 / 10.0;
            format!("Connected: {}\n{fps} fps, {mbps:.0} Mbps", link.addr.ip())
        }
        None => "DisplayWire Server: waiting for a client".to_string(),
    }
}

/// The connected device's name, or "the PC".
fn device_name(shared: &Shared) -> String {
    let client = shared.link.lock().unwrap().as_ref().map(|l| l.client.clone());
    client
        .and_then(|c| shared.clients.lock().unwrap().get(&c).map(|c| c.name.clone()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "the PC".into())
}

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

/// Opens the device window (another process), or brings the open one to the front.
fn open_ui(shared: &Shared, exe: &std::path::Path) {
    if shared.ui_open.load(Relaxed) > 0 {
        shared.ui_front.store(true, Relaxed);
        return;
    }
    let now = timing::now_ns();
    if now.saturating_sub(UI_LAUNCHED_NS.load(Relaxed)) < 5_000_000_000 {
        return;
    }
    UI_LAUNCHED_NS.store(now, Relaxed);
    let Some(handle) = shared.ui_handle.get() else { return };
    println!("ui: starting the device window");
    if let Err(e) = std::process::Command::new(exe).args(["--ui", &handle.port.to_string(), &handle.token]).spawn() {
        println!("ui: {e}");
    }
}

/// Runs the menu-bar item on the main thread; never returns.
pub fn run(shared: Arc<Shared>) -> ! {
    let mtm = MainThreadMarker::new().expect("the menu bar runs on the main thread");
    let exe = std::env::current_exe().unwrap_or_default();
    // A device asking to pair opens the window, as on Windows.
    {
        let (shared, exe) = (shared.clone(), exe.clone());
        let _ = std::thread::Builder::new().name("pair-watch".into()).spawn(move || {
            loop {
                if matches!(shared.pairing.snapshot(timing::now_ns()).view, View::Asking { .. } | View::Pin { .. }) {
                    open_ui(&shared, &exe);
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        });
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let target = Menu::new(mtm, State { shared, exe, addresses: local_addresses() });
    let item: Retained<NSStatusItem> = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        match NSImage::imageWithSystemSymbolName_accessibilityDescription(ns_string!("display"), Some(ns_string!("DisplayWire Server"))) {
            Some(img) => {
                img.setTemplate(true);
                button.setImage(Some(&img));
            }
            None => button.setTitle(ns_string!("DW")),
        }
    }
    app.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    let menu = NSMenu::new(mtm);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    target.fill(&menu);
    item.setMenu(Some(&menu));
    println!("menu bar: ready");
    if super::setup::missing() {
        super::setup::open();
    }
    app.run();
    std::process::exit(0)
}
