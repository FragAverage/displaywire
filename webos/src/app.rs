//! The app you launch from the TV's home screen: a list of computers (the ones paired, and any
//! the network answers for), pairing with a new one with the remote's number keys, and the stream
//! itself. Back leaves a stream, then the app.
//!
//! The screens only draw rectangles (`Canvas`), so they can be tried on a desktop with
//! `DW_WINDOWED=1 displaywire-tv app`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pairing::{Pairing, Progress};
use secure::pair::PIN_DIGITS;
use transport::video::Frame;

use crate::discovery::{self, Found};
use crate::screen::{Canvas, Color, HEIGHT, Input, Screen, WIDTH};
use crate::session::{self, VideoSink};
use crate::store::Store;

const DEFAULT_PORT: u16 = 9000;
/// What the PC's list of devices calls this TV.
const DEVICE_NAME: &str = "LG TV";

const BACKGROUND: Color = [16, 20, 26, 255];
const TEXT: Color = [236, 240, 244, 255];
const MUTED: Color = [122, 135, 148, 255];
const ACCENT: Color = [74, 163, 255, 255];
const GOOD: Color = [92, 214, 140, 255];
const BAD: Color = [255, 107, 107, 255];
const SELECTED: Color = [31, 58, 90, 255];
const BOX: Color = [30, 38, 48, 255];

/// What the app needs from the TV's video decoder.
pub trait Video: Send + Sync {
    /// Where one session's frames go.
    fn sink(&self) -> Box<dyn VideoSink + Send>;
    /// A session ended: take the picture down.
    fn close(&self);
    /// What to ask the server for (`proto::stream::CAP_*`).
    fn caps(&self) -> u8;
    /// Why there is no video, if there can't be.
    fn unavailable(&self) -> Option<String> {
        None
    }
}

/// No decoder: the screens and the connection, with frames thrown away (trying the app on a desktop).
pub struct NoVideo;

impl Video for NoVideo {
    fn sink(&self) -> Box<dyn VideoSink + Send> {
        Box::new(session::Discard)
    }
    fn close(&self) {}
    fn caps(&self) -> u8 {
        proto::stream::CAP_H264 | proto::stream::CAP_MAX_1080P
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Ok,
    Back,
    Digit(u8),
    Other,
}

/// The remote (or a keyboard) as SDL scancodes report it.
pub fn key(scancode: i32) -> Key {
    match scancode {
        82 => Key::Up,
        81 => Key::Down,
        80 => Key::Left,
        79 => Key::Right,
        40 | 88 => Key::Ok,
        // Escape, Backspace, AC Back, and webOS's own Back code.
        41 | 42 | 270 | 461 => Key::Back,
        30..=38 => Key::Digit((scancode - 29) as u8),
        39 => Key::Digit(0),
        89..=97 => Key::Digit((scancode - 88) as u8),
        98 => Key::Digit(0),
        _ => Key::Other,
    }
}

/// A computer in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    name: String,
    address: String,
    paired: bool,
    /// Answered the last search.
    online: bool,
}

/// The paired computers, then the others the network answered for.
fn entries(paired: &[(String, String)], found: &[Found]) -> Vec<Entry> {
    let mut list: Vec<Entry> = paired
        .iter()
        .map(|(address, name)| Entry {
            name: if name.is_empty() { address.clone() } else { name.clone() },
            address: address.clone(),
            paired: true,
            online: found.iter().any(|f| f.address.eq_ignore_ascii_case(address)),
        })
        .collect();
    for f in found {
        if !list.iter().any(|e| e.address.eq_ignore_ascii_case(&f.address)) {
            list.push(Entry { name: f.name.clone(), address: f.address.clone(), paired: false, online: true });
        }
    }
    list
}

enum State {
    List,
    /// Typing a computer's address.
    Address(String),
    Pairing { entry: Entry, pairing: Pairing, pin: String },
    Streaming(Stream),
}

struct Stream {
    entry: Entry,
    stop: Arc<AtomicBool>,
    /// Set by the sink at the first picture.
    shown: Arc<AtomicBool>,
    thread: JoinHandle<Result<(), String>>,
    /// Whether the connecting message has been taken down.
    cleared: bool,
}

/// Passes frames on, noting that the first has come.
struct Shown(Box<dyn VideoSink + Send>, Arc<AtomicBool>);

impl VideoSink for Shown {
    fn frame(&mut self, frame: &Frame) -> bool {
        let ok = self.0.frame(frame);
        if ok {
            self.1.store(true, Relaxed);
        }
        ok
    }
}

pub struct App {
    store: Store,
    video: Arc<dyn Video>,
    state: State,
    found: Arc<Mutex<Vec<Found>>>,
    /// Stops the search thread.
    quit: Arc<AtomicBool>,
    /// The search is paused while streaming.
    searching: Arc<AtomicBool>,
    list: Vec<Entry>,
    selected: usize,
    /// Something to tell the user on the list, and whether it is bad news.
    message: Option<(String, bool)>,
    dirty: bool,
    /// What the pairing screen last showed.
    last_progress: Option<Progress>,
}

impl App {
    pub fn new(store: Store, video: Arc<dyn Video>) -> App {
        let found = Arc::new(Mutex::new(Vec::new()));
        let (quit, searching) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(true)));
        {
            let (found, quit, searching) = (found.clone(), quit.clone(), searching.clone());
            let _ = std::thread::Builder::new().name("search".into()).spawn(move || {
                while !quit.load(Relaxed) {
                    if searching.load(Relaxed) {
                        let now = discovery::browse(Duration::from_millis(1500));
                        *found.lock().unwrap() = now;
                    }
                    for _ in 0..20 {
                        if quit.load(Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            });
        }
        let mut app = App { store, video, state: State::List, found, quit, searching, list: Vec::new(), selected: 0, message: None, dirty: true, last_progress: None };
        app.refresh();
        app
    }

    /// The list again from what is paired and what answered.
    fn refresh(&mut self) {
        let paired: Vec<(String, String)> = self.store.load().unwrap_or_default().into_iter().map(|s| (s.address, s.name)).collect();
        let list = entries(&paired, &self.found.lock().unwrap());
        if list != self.list {
            self.list = list;
            self.dirty = true;
        }
        // One more row than there are computers: "type an address".
        self.selected = self.selected.min(self.list.len());
    }

    /// Something to show above the list, until the next thing happens.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn say(&mut self, message: String, bad: bool) {
        self.message = Some((message, bad));
        self.dirty = true;
    }

    /// Runs until the user leaves.
    pub fn run(mut self, mut screen: Screen) -> Result<(), String> {
        let mut last_refresh = Instant::now();
        loop {
            let mut input = false;
            while let Some(event) = screen.poll() {
                match event {
                    Input::Quit => return Ok(()),
                    Input::Key(code) => {
                        println!("key {code}");
                        input = true;
                        if self.key(key(code)) {
                            return Ok(());
                        }
                    }
                }
            }
            self.tick();
            if last_refresh.elapsed() > Duration::from_millis(500) {
                last_refresh = Instant::now();
                self.refresh();
            }
            if self.dirty || input {
                self.dirty = false;
                self.draw(&mut screen);
                screen.present();
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// A key press. True to leave the app.
    fn key(&mut self, k: Key) -> bool {
        self.dirty = true;
        match &mut self.state {
            State::List => match k {
                Key::Up => self.selected = self.selected.saturating_sub(1),
                Key::Down => self.selected = (self.selected + 1).min(self.list.len()),
                Key::Ok => match self.list.get(self.selected).cloned() {
                    Some(entry) if entry.paired => self.connect(entry),
                    Some(entry) => self.pair(entry),
                    None => self.state = State::Address(String::new()),
                },
                // Pair again, if the PC has forgotten this TV.
                Key::Right => {
                    if let Some(entry) = self.list.get(self.selected).cloned() {
                        self.pair(entry);
                    }
                }
                Key::Back => return true,
                _ => {}
            },
            State::Address(text) => match k {
                Key::Digit(d) => text.push((b'0' + d) as char),
                Key::Right => text.push('.'),
                Key::Up => text.push(':'),
                Key::Left | Key::Down => {
                    text.pop();
                }
                Key::Back => self.state = State::List,
                Key::Ok => {
                    let address = text.trim().to_string();
                    if !address.is_empty() {
                        let known = self.store.find(&address).ok().flatten();
                        let entry = Entry { name: known.as_ref().map_or(address.clone(), |s| s.name.clone()), address, paired: known.is_some(), online: false };
                        if entry.paired { self.connect(entry) } else { self.pair(entry) }
                    }
                }
                Key::Other => {}
            },
            State::Pairing { pairing, pin, entry } => match (k, pairing.progress()) {
                (Key::Back, _) => {
                    pairing.cancel();
                    self.state = State::List;
                }
                (_, p) if p.finished() => {
                    let done = matches!(p, Progress::Done { .. });
                    let entry = entry.clone();
                    if done {
                        self.connect(Entry { paired: true, ..entry });
                    } else {
                        self.state = State::List;
                    }
                }
                (Key::Digit(d), Progress::NeedPin) if pin.len() < PIN_DIGITS => {
                    pin.push((b'0' + d) as char);
                    if pin.len() == PIN_DIGITS {
                        pairing.submit_pin(pin);
                    }
                }
                (Key::Left | Key::Down, Progress::NeedPin) => {
                    pin.pop();
                }
                _ => {}
            },
            State::Streaming(s) => {
                if k == Key::Back {
                    s.stop.store(true, Relaxed);
                    self.finish_stream();
                }
            }
        }
        false
    }

    fn pair(&mut self, entry: Entry) {
        self.last_progress = None;
        let started = transport::resolve(&entry.address, DEFAULT_PORT)
            .map_err(|e| e.to_string())
            .and_then(|addr| Pairing::start(addr, DEVICE_NAME).map_err(|e| e.to_string()));
        match started {
            Ok(pairing) => self.state = State::Pairing { entry, pairing, pin: String::new() },
            Err(e) => self.fail(format!("can't reach {}: {e}", entry.address)),
        }
    }

    fn connect(&mut self, entry: Entry) {
        if let Some(why) = self.video.unavailable() {
            return self.fail(why);
        }
        let key = match self.store.find(&entry.address) {
            Ok(Some(server)) => server.key,
            Ok(None) => return self.pair(entry),
            Err(e) => return self.fail(e),
        };
        let addr: SocketAddr = match transport::resolve(&entry.address, DEFAULT_PORT) {
            Ok(a) => a,
            Err(e) => return self.fail(format!("can't reach {}: {e}", entry.address)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let shown = Arc::new(AtomicBool::new(false));
        let sink: Box<dyn VideoSink + Send> = Box::new(Shown(self.video.sink(), shown.clone()));
        let caps = self.video.caps();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || session::run(addr, key, caps, sink, &stop))
        };
        self.searching.store(false, Relaxed);
        self.message = None;
        self.state = State::Streaming(Stream { entry, stop, shown, thread, cleared: false });
    }

    /// The stream's thread has been told to stop, or has stopped: join it and go back to the list.
    fn finish_stream(&mut self) {
        let State::Streaming(s) = std::mem::replace(&mut self.state, State::List) else { return };
        let result = s.thread.join().unwrap_or_else(|_| Err("the stream stopped unexpectedly".into()));
        self.video.close();
        self.searching.store(true, Relaxed);
        match result {
            Err(e) => self.fail(e),
            Ok(()) => self.message = None,
        }
        self.dirty = true;
    }

    fn fail(&mut self, message: String) {
        println!("error: {message}");
        self.message = Some((message, true));
        self.state = State::List;
        self.dirty = true;
    }

    /// Things that happen without a key: pairing moving on, a stream ending or starting.
    fn tick(&mut self) {
        match &mut self.state {
            State::Pairing { entry, pairing, pin } => {
                let progress = pairing.progress();
                match progress {
                    Progress::Done { key, server_name } => {
                        let entry = Entry { name: server_name.clone(), paired: true, ..entry.clone() };
                        if let Err(e) = self.store.upsert(&entry.address, &server_name, &key) {
                            return self.fail(e);
                        }
                        self.refresh();
                        self.connect(entry);
                    }
                    Progress::WrongPin { .. } => {
                        pin.clear();
                        pairing.retry();
                        self.dirty = true;
                    }
                    _ => {
                        if self.last_progress.as_ref() != Some(&progress) {
                            self.last_progress = Some(progress);
                            self.dirty = true;
                        }
                    }
                }
            }
            State::Streaming(s) => {
                if s.thread.is_finished() {
                    self.finish_stream();
                } else if s.shown.load(Relaxed) && !s.cleared {
                    // The first picture: stop covering it.
                    s.cleared = true;
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn draw(&self, c: &mut dyn Canvas) {
        match &self.state {
            // The picture is on a plane behind the window: leave the window clear.
            State::Streaming(s) if s.cleared => c.clear([0, 0, 0, 0]),
            State::Streaming(s) => {
                self.page(c, "CONNECTING", &format!("TO {}", s.entry.name));
                self.hint(c, "BACK: CANCEL");
            }
            State::List => self.draw_list(c),
            State::Address(text) => {
                self.page(c, "ADD A COMPUTER", "TYPE ITS ADDRESS");
                c.rect(120, 340, WIDTH - 240, 120, BOX);
                c.text(&format!("{text}_"), 160, 372, 7, TEXT);
                c.text("THE ADDRESS ON ITS SCREEN, LIKE 192.168.1.20", 120, 500, 3, MUTED);
                self.hint(c, "0-9: TYPE   >: DOT   ^: COLON   <: DELETE   OK: GO   BACK: CANCEL");
            }
            State::Pairing { entry, pairing, pin } => self.draw_pairing(c, entry, &pairing.progress(), pin),
        }
    }

    fn page(&self, c: &mut dyn Canvas, title: &str, subtitle: &str) {
        c.clear(BACKGROUND);
        c.rect(0, 0, 24, HEIGHT, ACCENT);
        c.text("DISPLAYWIRE", 120, 80, 8, TEXT);
        c.text(&format!("{title}  {subtitle}"), 120, 190, 4, MUTED);
    }

    fn hint(&self, c: &mut dyn Canvas, text: &str) {
        c.text(text, 120, HEIGHT - 100, 3, MUTED);
    }

    fn draw_list(&self, c: &mut dyn Canvas) {
        self.page(c, "CHOOSE A COMPUTER", "");
        let (left, right) = (120, WIDTH - 120);
        let rows = self.list.iter().map(|e| (e.name.as_str(), e.address.as_str(), e.paired, e.online)).chain([("TYPE AN ADDRESS", "", false, false)]);
        for (i, (name, address, paired, online)) in rows.enumerate() {
            let y = 290 + i as i32 * 100;
            if y > HEIGHT - 260 {
                break;
            }
            if i == self.selected {
                c.rect(left, y, right - left, 84, SELECTED);
                c.rect(left, y, 10, 84, ACCENT);
            }
            c.text(name, left + 40, y + 24, 5, TEXT);
            if !address.is_empty() {
                let width = crate::font::width(address, 4);
                c.text(address, right - width - 40, y + 28, 4, MUTED);
                let (tag, color) = match (paired, online) {
                    (true, true) => ("PAIRED, ONLINE", GOOD),
                    (true, false) => ("PAIRED", MUTED),
                    (false, _) => ("NEW: PRESS OK TO PAIR", ACCENT),
                };
                let tag_width = crate::font::width(tag, 3);
                c.text(tag, right - width - tag_width - 100, y + 32, 3, color);
            }
        }
        if self.list.is_empty() {
            c.text("LOOKING FOR COMPUTERS ON THE NETWORK...", left, 290 + 100 * 1 + 120, 3, MUTED);
        }
        if let Some((message, bad)) = &self.message {
            c.text(message, left, HEIGHT - 200, 3, if *bad { BAD } else { TEXT });
        }
        self.hint(c, "OK: CONNECT   >: PAIR AGAIN   BACK: QUIT");
    }

    fn draw_pairing(&self, c: &mut dyn Canvas, entry: &Entry, progress: &Progress, pin: &str) {
        self.page(c, "PAIRING WITH", &entry.name);
        let (line, color) = match progress {
            Progress::Contacting => ("ASKING THE COMPUTER TO PAIR...".to_string(), TEXT),
            Progress::NoReply => ("NO REPLY. CHOOSE PAIR NEW DEVICE IN DISPLAYWIRE ON THE COMPUTER.".into(), BAD),
            Progress::WaitingForAllow => ("CLICK ALLOW ON THE COMPUTER.".into(), TEXT),
            Progress::NeedPin => (format!("TYPE THE {PIN_DIGITS} DIGIT PIN THE COMPUTER SHOWS."), TEXT),
            Progress::Checking => ("CHECKING...".into(), TEXT),
            Progress::WrongPin { tries_left } => (format!("WRONG PIN, {tries_left} TRIES LEFT."), BAD),
            Progress::Denied => ("THE COMPUTER SAID NO. PRESS ANY KEY.".into(), BAD),
            Progress::Busy => ("ANOTHER DEVICE IS PAIRING WITH THE COMPUTER. WAITING...".into(), TEXT),
            Progress::Locked => ("TOO MANY WRONG PINS, OR TOO SLOW. PRESS ANY KEY, THEN START AGAIN ON THE COMPUTER.".into(), BAD),
            Progress::Cancelled => ("CANCELLED. PRESS ANY KEY.".into(), BAD),
            Progress::Failed(e) => (format!("{e}. PRESS ANY KEY."), BAD),
            Progress::Done { .. } => ("PAIRED.".into(), GOOD),
        };
        c.text(&line, 120, 330, 4, color);
        if matches!(progress, Progress::NeedPin | Progress::Checking | Progress::WrongPin { .. }) {
            for i in 0..PIN_DIGITS {
                let x = 120 + i as i32 * 130;
                c.rect(x, 440, 110, 150, if i == pin.len() { SELECTED } else { BOX });
                if let Some(d) = pin.get(i..i + 1) {
                    c.text(d, x + 25, 475, 12, TEXT);
                }
            }
        }
        self.hint(c, "0-9: PIN   <: DELETE   BACK: CANCEL");
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.quit.store(true, Relaxed);
        if let State::Streaming(s) = &self.state {
            s.stop.store(true, Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder(Vec<(i32, i32, i32, i32)>);

    impl Canvas for Recorder {
        fn clear(&mut self, _: Color) {}
        fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, _: Color) {
            self.0.push((x, y, w, h));
        }
    }

    #[test]
    fn the_remote_maps_to_keys() {
        assert_eq!(key(82), Key::Up);
        assert_eq!(key(40), Key::Ok);
        assert_eq!(key(461), Key::Back);
        // Number row and keypad: 1..9 then 0.
        assert_eq!(key(30), Key::Digit(1));
        assert_eq!(key(38), Key::Digit(9));
        assert_eq!(key(39), Key::Digit(0));
        assert_eq!(key(89), Key::Digit(1));
        assert_eq!(key(97), Key::Digit(9));
        assert_eq!(key(98), Key::Digit(0));
        assert_eq!(key(4), Key::Other);
    }

    #[test]
    fn paired_computers_come_first_and_answers_are_not_listed_twice() {
        let paired = vec![("192.168.1.20".to_string(), "Studio Mac".to_string()), ("10.0.0.5".to_string(), String::new())];
        let found = vec![
            Found { name: "Studio Mac".into(), address: "192.168.1.20".into() },
            Found { name: "Gaming PC".into(), address: "192.168.1.30".into() },
        ];
        let list = entries(&paired, &found);
        assert_eq!(list.len(), 3);
        assert_eq!((list[0].paired, list[0].online), (true, true));
        // No name given: the address does.
        assert_eq!((list[1].name.as_str(), list[1].online), ("10.0.0.5", false));
        assert_eq!((list[2].name.as_str(), list[2].paired, list[2].online), ("Gaming PC", false, true));
    }

    #[test]
    fn the_list_fits_on_the_screen() {
        let paired: Vec<_> = (0..20).map(|i| (format!("10.0.0.{i}"), format!("Computer number {i}"))).collect();
        let app = App {
            store: Store::new(Some(std::env::temp_dir().join("dw-tv-test"))),
            video: Arc::new(NoVideo),
            state: State::List,
            found: Arc::default(),
            quit: Arc::default(),
            searching: Arc::default(),
            list: entries(&paired, &[]),
            selected: 3,
            message: Some(("something went wrong".into(), true)),
            dirty: false,
            last_progress: None,
        };
        let mut r = Recorder(Vec::new());
        app.draw(&mut r);
        assert!(!r.0.is_empty());
        assert!(r.0.iter().all(|&(x, y, w, h)| x >= 0 && y >= 0 && x + w <= WIDTH && y + h <= HEIGHT), "something is drawn off the screen");
    }
}
