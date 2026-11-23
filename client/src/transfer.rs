//! Clipboard sharing and file upload: the client's side of `xfer`.
//!
//! Text copied on the Mac appears on the PC and the other way round (skipping
//! anything a password manager marks as concealed). Files dropped on the window
//! or chosen from the menu are sent to the other computer's Downloads folder, and
//! files it sends (a Mac server's menu bar) land in `Downloads/DisplayWire` here.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(target_os = "macos")]
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
#[cfg(target_os = "macos")]
use objc2_foundation::NSString;
use proto::stream::Msg;
use timing::now_ns;
use winit::event_loop::EventLoopProxy;
use xfer::clipfiles::Inbox;
use xfer::{Bytes, Endpoint, Event, Kind, Meta, Sink};

use crate::app::UserEvent;
use crate::link::Link;

const MS: u64 = 1_000_000;
/// Clipboard text beyond this is neither sent nor accepted.
const MAX_CLIPBOARD: u64 = 1 << 20;
/// Files go out at no more than this, so they can't starve the video.
const SEND_RATE: u64 = 8_000_000;
const CLIPBOARD_POLL_NS: u64 = 250 * MS;
/// Largest file accepted from the other computer.
const MAX_FILE: u64 = 16 << 30;

pub struct Transfers {
    endpoint: Mutex<Endpoint>,
    link: Arc<Link>,
    proxy: EventLoopProxy<UserEvent>,
    /// Clipboard text arriving from the PC.
    slot: Arc<Mutex<Vec<u8>>>,
    clipboard_on: AtomicBool,
    stopped: AtomicBool,
    /// Files copied on the other computer, waiting to be pasted here.
    inbox: Option<Arc<Mutex<Inbox>>>,
}

fn make_endpoint(slot: &Arc<Mutex<Vec<u8>>>, inbox: &Option<Arc<Mutex<Inbox>>>) -> Endpoint {
    let (slot, inbox) = (slot.clone(), inbox.clone());
    Endpoint::new(MAX_FILE, SEND_RATE, move |meta| match meta.kind {
        Kind::Clipboard if meta.size <= MAX_CLIPBOARD => {
            slot.lock().unwrap().clear();
            Ok(Box::new(ClipboardSink(slot.clone())) as Box<dyn Sink>)
        }
        Kind::File if meta.size <= MAX_FILE => Ok(Box::new(FileSink::create(&meta.name)?) as Box<dyn Sink>),
        Kind::ClipboardFile if meta.size <= MAX_FILE && let Some(inbox) = &inbox => inbox.lock().unwrap().sink(meta),
        _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not accepted")),
    })
}

/// Where received files go: `Downloads/DisplayWire` in the user's folder.
pub fn received_dir() -> std::io::Result<std::path::PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .ok_or_else(|| std::io::Error::other("no home folder"))?;
    Ok(std::path::PathBuf::from(home).join("Downloads").join("DisplayWire"))
}

/// A file being received: written to `<name>.part`, renamed when complete.
struct FileSink {
    file: std::fs::File,
    part: std::path::PathBuf,
    dest: std::path::PathBuf,
}

impl FileSink {
    fn create(name: &str) -> std::io::Result<FileSink> {
        let dir = received_dir()?;
        std::fs::create_dir_all(&dir)?;
        let dest = xfer::unique_path(&dir, &xfer::safe_file_name(name));
        let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap().to_string_lossy()));
        let file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&part)?;
        Ok(FileSink { file, part, dest })
    }
}

impl Sink for FileSink {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    fn finish(self: Box<Self>) -> std::io::Result<()> {
        let FileSink { file, part, dest } = *self;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&part, &dest)
    }

    fn abort(self: Box<Self>) {
        let FileSink { file, part, .. } = *self;
        drop(file);
        let _ = std::fs::remove_file(&part);
    }
}

impl Transfers {
    pub fn new(link: Arc<Link>, proxy: EventLoopProxy<UserEvent>, clipboard_on: bool) -> Arc<Transfers> {
        let slot = Arc::new(Mutex::new(Vec::new()));
        let inbox = clipboard_files_dir().map(Inbox::new);
        Arc::new(Transfers {
            endpoint: Mutex::new(make_endpoint(&slot, &inbox)),
            inbox,
            link,
            proxy,
            slot,
            clipboard_on: AtomicBool::new(clipboard_on),
            stopped: AtomicBool::new(false),
        })
    }

    /// Ends the transfer thread (the connection is gone).
    pub fn shutdown(&self) {
        self.stopped.store(true, Relaxed);
    }

    pub fn set_clipboard(&self, on: bool) {
        self.clipboard_on.store(on, Relaxed);
    }

    /// A transfer message from the PC (called on the receiver thread).
    pub fn handle(&self, msg: &Msg) {
        let replies = {
            let mut e = self.endpoint.lock().unwrap();
            transport::transfer::handle(&mut e, now_ns(), msg).unwrap_or_default()
        };
        let mut plain = [0u8; proto::MAX_PLAIN];
        for r in replies {
            let n = transport::transfer::encode(&r, &mut plain);
            self.link.send(&plain[..n]);
        }
    }

    /// Starts sending a file to the PC.
    pub fn send_file(&self, path: &Path) -> Result<(), String> {
        let file = std::fs::File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
        let md = file.metadata().map_err(|e| e.to_string())?;
        if !md.is_file() {
            return Err(format!("{} is a folder: zip it first", path.display()));
        }
        let name = path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        self.endpoint.lock().unwrap().send(Meta { kind: Kind::File, size: md.len(), name }, Box::new(file));
        Ok(())
    }

    pub fn spawn(self: &Arc<Self>) {
        let this = self.clone();
        let _ = std::thread::Builder::new().name("transfer".into()).spawn(move || this.run());
    }

    fn notice(&self, text: Option<String>) {
        let _ = self.proxy.send_event(UserEvent::Notice(text));
    }

    fn run(&self) {
        let mut plain = [0u8; proto::MAX_PLAIN];
        let mut session = 0u32;
        let (mut last_count, mut own_count, mut next_poll) = (pasteboard_count(), -1isize, 0u64);
        let mut last_sent_text = String::new();
        let mut showing: Option<String> = None;
        let mut clear_at = 0u64;
        // A message from the end of one pass, shown in the next.
        let mut later: Option<(String, u64)> = None;
        while !self.stopped.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(2));
            let now = now_ns();
            let Some(s) = self.link.session() else { continue };
            if s.id() != session {
                // A new session after an earlier one: what was in flight belonged to that
                // one. (The first session keeps what was queued before it existed.)
                if session != 0 {
                    *self.endpoint.lock().unwrap() = make_endpoint(&self.slot, &self.inbox);
                }
                session = s.id();
                last_count = pasteboard_count();
                last_sent_text.clear();
            }
            let mut out = Vec::new();
            let events = self.endpoint.lock().unwrap().poll(now, &mut out);
            for o in &out {
                let n = transport::transfer::encode(o, &mut plain);
                self.link.send(&plain[..n]);
            }
            let mut message: Option<(String, u64)> = later.take();
            for e in events {
                match e {
                    Event::Received { meta, .. } if meta.kind == Kind::Clipboard => {
                        let text = String::from_utf8_lossy(&std::mem::take(&mut *self.slot.lock().unwrap())).into_owned();
                        if self.clipboard_on.load(Relaxed) {
                            own_count = set_pasteboard_text(&text);
                            last_count = own_count;
                            last_sent_text = text;
                        }
                    }
                    Event::Sent { meta, .. } if meta.kind == Kind::File => message = Some((format!("Sent {}", meta.name), 3_000)),
                    Event::Received { meta, .. } if meta.kind == Kind::ClipboardFile => {
                        let paths = self.inbox.as_ref().and_then(|i| i.lock().unwrap().received(&meta));
                        if let Some(paths) = paths
                            && self.clipboard_on.load(Relaxed)
                        {
                            own_count = set_pasteboard_files(&paths);
                            last_count = own_count;
                            let what = if paths.len() == 1 { "1 file".to_string() } else { format!("{} files", paths.len()) };
                            message = Some((format!("Copied {what}: ready to paste"), 4_000));
                        }
                    }
                    Event::Received { meta, .. } if meta.kind == Kind::File => {
                        let place = if cfg!(windows) { "Downloads\\DisplayWire" } else { "Downloads/DisplayWire" };
                        message = Some((format!("Received {} (in {place})", meta.name), 6_000));
                    }
                    Event::ReceiveFailed { why, .. } => message = Some((format!("Couldn't receive a file: {why}"), 7_000)),
                    Event::SendFailed { meta, why, .. } if meta.kind == Kind::File => {
                        message = Some((format!("Couldn't send {}: {why}", meta.name), 7_000));
                    }
                    _ => {}
                }
            }
            // What the banner says: a final message for a few seconds, else progress.
            if let Some((text, ms)) = message {
                clear_at = now + ms * MS;
                if showing.as_ref() != Some(&text) {
                    self.notice(Some(text.clone()));
                    showing = Some(text);
                }
            } else if clear_at > now {
                // A final message is still up.
            } else {
                let active = self.endpoint.lock().unwrap().active();
                let progress = |sending: bool, verb: &str| {
                    let files: Vec<_> = active.iter().filter(|a| a.1 == sending && a.2 == Kind::File).collect();
                    (!files.is_empty()).then(|| {
                        let (done, total) = files.iter().fold((0, 0), |(d, t), a| (d + a.3, t + a.4));
                        let pct = (done * 100).checked_div(total).unwrap_or(100);
                        match files.as_slice() {
                            [one] => format!("{verb} {}… {pct}%", one.5),
                            many => format!("{verb} {} files… {pct}%", many.len()),
                        }
                    })
                };
                let copying = active.iter().filter(|a| a.2 == Kind::ClipboardFile).collect::<Vec<_>>();
                let copy_line = (!copying.is_empty()).then(|| {
                    let (done, total) = copying.iter().fold((0, 0), |(d, t), a| (d + a.3, t + a.4));
                    let pct = (done * 100).checked_div(total).unwrap_or(100);
                    format!("Copying {} file(s) across… {pct}%", copying.len())
                });
                let line = [progress(true, "Sending"), progress(false, "Receiving"), copy_line].into_iter().flatten().reduce(|a, b| format!("{a}  ·  {b}"));
                if line != showing {
                    self.notice(line.clone());
                    showing = line;
                }
            }
            if self.clipboard_on.load(Relaxed) && now >= next_poll {
                next_poll = now + CLIPBOARD_POLL_NS;
                let count = pasteboard_count();
                if count != last_count {
                    last_count = count;
                    let files = if count != own_count { pasteboard_files() } else { None };
                    if let Some(files) = files {
                        // Files copied here: they go across now, ready to paste there.
                        if let Err(e) = xfer::clipfiles::send(&mut self.endpoint.lock().unwrap(), &files) {
                            later = Some((format!("Not copied to the other computer: {e}"), 5_000));
                        }
                    } else if count != own_count
                        && let Some(text) = pasteboard_text()
                        && !text.is_empty()
                        && text != last_sent_text
                        && text.len() as u64 <= MAX_CLIPBOARD
                    {
                        last_sent_text = text.clone();
                        let meta = Meta { kind: Kind::Clipboard, size: text.len() as u64, name: String::new() };
                        self.endpoint.lock().unwrap().send(meta, Box::new(Bytes(text.into_bytes())));
                    }
                }
            }
        }
    }
}

struct ClipboardSink(Arc<Mutex<Vec<u8>>>);

impl Sink for ClipboardSink {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        let mut v = self.0.lock().unwrap();
        let end = offset as usize + data.len();
        if v.len() < end {
            v.resize(end, 0);
        }
        v[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    fn finish(self: Box<Self>) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
use crate::win::clipboard::{clipboard_files_dir, pasteboard_count, pasteboard_files, pasteboard_text, set_pasteboard_files, set_pasteboard_text};

#[cfg(target_os = "macos")]
fn pasteboard_files() -> Option<Vec<std::path::PathBuf>> {
    use objc2_app_kit::NSPasteboardTypeFileURL;
    use objc2_foundation::NSURL;
    let items = NSPasteboard::generalPasteboard().pasteboardItems()?;
    let paths: Vec<std::path::PathBuf> = items
        .iter()
        .filter_map(|item| item.stringForType(unsafe { NSPasteboardTypeFileURL }))
        .filter_map(|s| NSURL::URLWithString(&s))
        .filter_map(|u| u.filePathURL()?.path())
        .map(|p| std::path::PathBuf::from(p.to_string()))
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// Puts files on the pasteboard as Finder's Copy does; returns the change count that results.
#[cfg(target_os = "macos")]
fn set_pasteboard_files(paths: &[std::path::PathBuf]) -> isize {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::NSPasteboardWriting;
    use objc2_foundation::{NSArray, NSURL};
    let urls: Vec<_> = paths
        .iter()
        .map(|p| ProtocolObject::<dyn NSPasteboardWriting>::from_retained(NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()))))
        .collect();
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.writeObjects(&NSArray::from_retained_slice(&urls));
    pb.changeCount()
}

#[cfg(target_os = "macos")]
fn clipboard_files_dir() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches/DisplayWire/Clipboard"))
}

#[cfg(target_os = "macos")]
fn pasteboard_count() -> isize {
    NSPasteboard::generalPasteboard().changeCount()
}

/// Password managers mark what they copy so clipboard tools leave it alone.
#[cfg(target_os = "macos")]
const CONCEALED: [&str; 2] = ["org.nspasteboard.ConcealedType", "com.agilebits.onepassword"];

#[cfg(target_os = "macos")]
fn pasteboard_text() -> Option<String> {
    let pb = NSPasteboard::generalPasteboard();
    if pb.types()?.iter().any(|t| CONCEALED.contains(&t.to_string().as_str())) {
        return None;
    }
    pb.stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
}

/// Puts `text` on the pasteboard; returns the change count that results.
#[cfg(target_os = "macos")]
fn set_pasteboard_text(text: &str) -> isize {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
    pb.changeCount()
}
