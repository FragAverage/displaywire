//! The stream window's menu bar. Choices arrive on the event loop as `UserEvent::Menu`.

use std::sync::Mutex;

use proto::stream::Monitors;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreateMenu, CreatePopupMenu, DeleteMenu, DrawMenuBar, GetMenuItemCount, HMENU, MF_BYCOMMAND, MF_BYPOSITION,
    MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, SC_KEYMENU, SetMenu, WM_COMMAND, WM_SYSCOMMAND,
};
use windows::core::HSTRING;
use winit::event_loop::EventLoopProxy;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::app::UserEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Disconnect,
    SendFile,
    FullScreen,
    /// Every stream window in or out of full screen.
    FullScreenAll,
    Capture,
    ToggleStats,
    Mute,
    Microphone,
    Settings,
    Shortcuts,
    /// Show this monitor in the main window.
    Monitor(u8),
    /// Show this monitor in a window of its own.
    OpenMonitor(u8),
    /// Every monitor that isn't showing yet, each in a window of its own.
    OpenAllMonitors,
}

const FIXED: [(u32, Action); 11] = [
    (1, Action::Disconnect),
    (2, Action::SendFile),
    (3, Action::FullScreen),
    (4, Action::Capture),
    (5, Action::ToggleStats),
    (6, Action::Mute),
    (7, Action::Microphone),
    (8, Action::Settings),
    (9, Action::Shortcuts),
    (10, Action::OpenAllMonitors),
    (11, Action::FullScreenAll),
];
const MONITOR_BASE: u32 = 100;
const OPEN_BASE: u32 = 200;

fn id_of(action: Action) -> u32 {
    match action {
        Action::Monitor(i) => MONITOR_BASE + i as u32,
        Action::OpenMonitor(i) => OPEN_BASE + i as u32,
        a => FIXED.iter().find(|f| f.1 == a).map_or(0, |f| f.0),
    }
}

fn action_of(id: u32) -> Option<Action> {
    match id {
        MONITOR_BASE..OPEN_BASE => Some(Action::Monitor((id - MONITOR_BASE) as u8)),
        OPEN_BASE..300 => Some(Action::OpenMonitor((id - OPEN_BASE) as u8)),
        _ => FIXED.iter().find(|f| f.0 == id).map(|f| f.1),
    }
}

static PROXY: Mutex<Option<EventLoopProxy<UserEvent>>> = Mutex::new(None);

pub struct Menu {
    hwnd: HWND,
    bar: HMENU,
    view: HMENU,
    monitors: HMENU,
}

fn add(menu: HMENU, action: Action, title: &str) {
    let _ = unsafe { AppendMenuW(menu, MF_STRING, id_of(action) as usize, &HSTRING::from(title)) };
}

fn separator(menu: HMENU) {
    let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) };
}

fn hwnd_of(window: &Window) -> HWND {
    match window.window_handle().expect("window handle").as_raw() {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
        _ => panic!("not a Win32 window"),
    }
}

impl Menu {
    /// Gives `window` the menu bar. UI thread.
    pub fn install(window: &Window, proxy: EventLoopProxy<UserEvent>) -> Menu {
        *PROXY.lock().unwrap() = Some(proxy);
        let hwnd = hwnd_of(window);
        unsafe {
            let bar = CreateMenu().unwrap_or_default();
            let connection = CreatePopupMenu().unwrap_or_default();
            add(connection, Action::SendFile, "Send File to PC…");
            separator(connection);
            add(connection, Action::Disconnect, "Disconnect\tCtrl+Alt+D");
            let view = CreatePopupMenu().unwrap_or_default();
            add(view, Action::FullScreen, "Full Screen\tCtrl+Shift+W");
            add(view, Action::FullScreenAll, "Full Screen (All Windows)");
            add(view, Action::Capture, "Capture Pointer (for games)\tCtrl+Alt+M");
            add(view, Action::ToggleStats, "Show Stats\tCtrl+Alt+S");
            separator(view);
            add(view, Action::Mute, "Mute Audio");
            add(view, Action::Microphone, "Microphone On\tCtrl+Alt+V");
            let monitors = CreatePopupMenu().unwrap_or_default();
            let help = CreatePopupMenu().unwrap_or_default();
            add(help, Action::Shortcuts, "Keyboard Shortcuts");
            let _ = AppendMenuW(bar, MF_POPUP, connection.0 as usize, &HSTRING::from("Connection"));
            let _ = AppendMenuW(bar, MF_POPUP, view.0 as usize, &HSTRING::from("View"));
            let _ = AppendMenuW(bar, MF_POPUP, monitors.0 as usize, &HSTRING::from("Monitors"));
            let _ = AppendMenuW(bar, MF_STRING, id_of(Action::Settings) as usize, &HSTRING::from("Settings"));
            let _ = AppendMenuW(bar, MF_POPUP, help.0 as usize, &HSTRING::from("Help"));
            let _ = SetMenu(hwnd, Some(bar));
            let _ = SetWindowSubclass(hwnd, Some(on_message), 1, 0);
            let menu = Menu { hwnd, bar, view, monitors };
            menu.set_monitors(None, &[]);
            menu
        }
    }

    /// The Monitors menu: which one the main window shows, and each to open in a window of its own.
    pub fn set_monitors(&self, monitors: Option<&Monitors>, open: &[u8]) {
        unsafe {
            while GetMenuItemCount(Some(self.monitors)) > 0 {
                let _ = DeleteMenu(self.monitors, 0, MF_BYPOSITION);
            }
            let Some(m) = monitors else {
                let _ = AppendMenuW(self.monitors, MF_STRING | MF_GRAYED, 0, &HSTRING::from("Connect to choose"));
                return;
            };
            for (i, mon) in m.iter().enumerate() {
                let mut title = format!("{} · {}×{}", mon.name(), mon.width, mon.height);
                if mon.refresh > 0 {
                    title += &format!(" · {} Hz", mon.refresh);
                }
                if mon.primary {
                    title += " · main";
                }
                let checked = if i == m.streaming as usize { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(self.monitors, MF_STRING | checked, id_of(Action::Monitor(i as u8)) as usize, &HSTRING::from(title));
            }
            if m.count > 1 {
                separator(self.monitors);
                let _ = AppendMenuW(self.monitors, MF_STRING | MF_GRAYED, 0, &HSTRING::from("Open in New Window"));
                for (i, mon) in m.iter().enumerate() {
                    let state = if open.contains(&(i as u8)) { MF_CHECKED | MF_GRAYED } else { MF_UNCHECKED };
                    let _ = AppendMenuW(self.monitors, MF_STRING | state, id_of(Action::OpenMonitor(i as u8)) as usize, &HSTRING::from(format!("    {}", mon.name())));
                }
                separator(self.monitors);
                add(self.monitors, Action::OpenAllMonitors, "Open All Monitors\tCtrl+Alt+A");
            }
        }
    }

    /// Ticks beside the things that are on.
    pub fn set_checks(&self, full_screen: bool, captured: bool, stats: bool, muted: bool, mic: bool) {
        for (action, on) in
            [(Action::FullScreen, full_screen), (Action::Capture, captured), (Action::ToggleStats, stats), (Action::Mute, muted), (Action::Microphone, mic)]
        {
            let flag = if on { MF_CHECKED } else { MF_UNCHECKED };
            unsafe { CheckMenuItem(self.view, id_of(action), (MF_BYCOMMAND | flag).0) };
        }
    }

    /// The bar is put away in full screen, where the picture has the whole display.
    pub fn set_visible(&self, visible: bool) {
        unsafe {
            let _ = SetMenu(self.hwnd, if visible { Some(self.bar) } else { None });
            let _ = DrawMenuBar(self.hwnd);
        }
    }
}

unsafe extern "system" fn on_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        // A menu item (not an accelerator or a control).
        WM_COMMAND if (wparam.0 >> 16) & 0xFFFF == 0 && lparam.0 == 0 => {
            if let Some(action) = action_of((wparam.0 & 0xFFFF) as u32)
                && let Some(proxy) = PROXY.lock().unwrap().as_ref()
            {
                let _ = proxy.send_event(UserEvent::Menu(action));
            }
            return LRESULT(0);
        }
        // Alt and F10 belong to the remote PC: they don't open this menu.
        WM_SYSCOMMAND if (wparam.0 & 0xFFF0) as u32 == SC_KEYMENU => return LRESULT(0),
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}
