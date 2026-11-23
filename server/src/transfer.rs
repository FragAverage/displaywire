//! Clipboard sharing and file upload: the server's side of `xfer`.
//!
//! Text copied on either machine appears on the other. Files sent from the
//! client are saved in the logged-in user's `Downloads\DisplayWire` folder (`~/Downloads/DisplayWire`
//! on a Mac); files the server's user sends (the Mac's menu bar) go to the client's.
//! Everything arrives through the authenticated session; `--no-files` and
//! `--no-clipboard` turn the two features off.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::stream::Msg;
use timing::now_ns;
use xfer::clipfiles::Inbox;
use xfer::{Bytes, Endpoint, Event, Kind, Meta, Outgoing, Sink, safe_file_name, unique_path};

use crate::Shared;

/// Clipboard text beyond this is neither sent nor accepted.
const MAX_CLIPBOARD: u64 = 1 << 20;
const CLIPBOARD_POLL_NS: u64 = 250_000_000;
/// Files to the client and clipboard text go at no more than this (bytes a second), so they
/// can't starve the video.
const SEND_RATE: u64 = 8_000_000;

/// The one set of transfers, for sending a file from elsewhere (the menu bar).
static TRANSFERS: std::sync::OnceLock<Arc<Transfers>> = std::sync::OnceLock::new();

pub struct Options {
    pub clipboard: bool,
    pub files: bool,
    pub max_file_bytes: u64,
}

pub struct Transfers {
    endpoint: Mutex<Endpoint>,
    options: Arc<Options>,
    /// Clipboard text arriving from the client.
    slot: Arc<Mutex<Vec<u8>>>,
    /// Where the last upload was saved.
    last_saved: Arc<Mutex<Option<PathBuf>>>,
    /// Files copied on the client, waiting to be pasted here.
    inbox: Option<Arc<Mutex<Inbox>>>,
}

fn make_endpoint(options: &Arc<Options>, slot: &Arc<Mutex<Vec<u8>>>, last_saved: &Arc<Mutex<Option<PathBuf>>>, inbox: &Option<Arc<Mutex<Inbox>>>) -> Endpoint {
    let (opts, slot, last_saved, inbox) = (options.clone(), slot.clone(), last_saved.clone(), inbox.clone());
    Endpoint::new(options.max_file_bytes.max(MAX_CLIPBOARD), SEND_RATE, move |meta| match meta.kind {
        Kind::Clipboard => {
            if !opts.clipboard || meta.size > MAX_CLIPBOARD {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "clipboard sharing is off or the text is too large"));
            }
            slot.lock().unwrap().clear();
            Ok(Box::new(ClipboardSink(slot.clone())) as Box<dyn Sink>)
        }
        Kind::ClipboardFile => match &inbox {
            Some(inbox) if opts.clipboard && opts.files => inbox.lock().unwrap().sink(meta),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "copying files across is off")),
        },
        Kind::File => {
            if !opts.files || meta.size > opts.max_file_bytes {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "file upload is off or the file is too large"));
            }
            let sink = FileSink::create(&meta.name)?;
            *last_saved.lock().unwrap() = Some(sink.dest.clone());
            Ok(Box::new(sink))
        }
    })
}

impl Transfers {
    pub fn new(options: Options) -> Arc<Transfers> {
        let options = Arc::new(options);
        let (slot, last_saved) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
        let inbox = platform::clipboard_files_dir().ok().map(Inbox::new);
        let endpoint = Mutex::new(make_endpoint(&options, &slot, &last_saved, &inbox));
        let t = Arc::new(Transfers { endpoint, options, slot, last_saved, inbox });
        let _ = TRANSFERS.set(t.clone());
        t
    }

    /// Starts sending a file to the connected client, which saves it in its Downloads folder.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn send_file(path: &Path) -> Result<(), String> {
        let t = TRANSFERS.get().ok_or("transfers aren't running")?;
        if !t.options.files {
            return Err("file transfer is off (--no-files)".into());
        }
        let file = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
        let md = file.metadata().map_err(|e| e.to_string())?;
        if !md.is_file() {
            return Err(format!("{} is a folder: zip it first", path.display()));
        }
        let name = path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        println!("sending {name} ({} bytes) to the client", md.len());
        t.endpoint.lock().unwrap().send(Meta { kind: Kind::File, size: md.len(), name }, Box::new(file));
        Ok(())
    }

    /// A transfer message from the client; returns what to send back.
    pub fn handle(&self, now: u64, msg: &Msg) -> Vec<Outgoing> {
        match *msg {
            Msg::Xfer { id, index: 0, .. } => println!("transfer {id}: starting"),
            Msg::XferCancel { id, reason } => println!("transfer {id}: the client stopped it (reason {reason})"),
            _ => {}
        }
        let mut e = self.endpoint.lock().unwrap();
        transport::transfer::handle(&mut e, now, msg).unwrap_or_default()
    }

    /// Runs the transfers: sends what's due, applies clipboard text that arrived,
    /// and watches the local clipboard.
    pub fn spawn(self: &Arc<Self>, sock: UdpSocket, shared: Arc<Shared>) {
        let this = self.clone();
        let _ = std::thread::Builder::new().name("transfer".into()).spawn(move || this.run(&sock, &shared));
    }

    fn run(&self, sock: &UdpSocket, shared: &Shared) {
        let mut out_buf = [0u8; proto::MAX_DATAGRAM];
        let mut plain = [0u8; proto::MAX_DATAGRAM];
        let mut session = 0u32;
        let (mut last_seq, mut own_seq, mut next_poll) = (platform::clipboard_sequence(), 0u32, 0u64);
        let mut last_sent_text = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(2));
            let now = now_ns();
            let link = shared.link.lock().unwrap().clone();
            let Some(link) = link else {
                session = 0;
                continue;
            };
            if link.session.id() != session {
                // A new session: whatever was in flight belonged to the last one.
                if session != 0 {
                    *self.endpoint.lock().unwrap() = make_endpoint(&self.options, &self.slot, &self.last_saved, &self.inbox);
                }
                session = link.session.id();
                last_seq = platform::clipboard_sequence();
                last_sent_text.clear();
            }
            let mut out = Vec::new();
            let events = self.endpoint.lock().unwrap().poll(now, &mut out);
            for o in &out {
                let n = transport::transfer::encode(o, &mut plain);
                link.send(sock, &plain[..n], &mut out_buf);
            }
            for e in events {
                match e {
                    Event::Received { meta, .. } if meta.kind == Kind::Clipboard => {
                        let text = String::from_utf8_lossy(&std::mem::take(&mut *self.slot.lock().unwrap())).into_owned();
                        if self.options.clipboard && platform::set_clipboard_text(&text) {
                            own_seq = platform::clipboard_sequence();
                            last_seq = own_seq;
                            last_sent_text = text;
                        }
                    }
                    Event::Received { meta, .. } if meta.kind == Kind::ClipboardFile => {
                        let paths = self.inbox.as_ref().and_then(|i| i.lock().unwrap().received(&meta));
                        if let Some(paths) = paths
                            && platform::set_clipboard_files(&paths)
                        {
                            own_seq = platform::clipboard_sequence();
                            last_seq = own_seq;
                            println!("{} copied file(s) from the client, ready to paste", paths.len());
                        }
                    }
                    Event::Received { meta, .. } => {
                        let saved = self.last_saved.lock().unwrap().clone();
                        println!("received {} ({} bytes) as {}", meta.name, meta.size, saved.as_ref().map(|p| p.display().to_string()).unwrap_or_default());
                        platform::notify(&format!("Received {}", meta.name));
                        if let Some(p) = saved {
                            platform::reveal(&p);
                        }
                    }
                    Event::ReceiveFailed { why, .. } => println!("transfer from the client failed: {why}"),
                    Event::Sent { meta, .. } if meta.kind == Kind::File => {
                        println!("sent {}", meta.name);
                        platform::notify(&format!("Sent {}", meta.name));
                    }
                    Event::SendFailed { meta, why, .. } if meta.kind == Kind::File => {
                        println!("sending {} failed: {why}", meta.name);
                        platform::notify(&format!("Couldn't send {}: {why}", meta.name));
                    }
                    Event::Sent { .. } | Event::SendFailed { .. } => {}
                }
            }
            if self.options.clipboard && now >= next_poll {
                next_poll = now + CLIPBOARD_POLL_NS;
                let seq = platform::clipboard_sequence();
                if seq != last_seq {
                    last_seq = seq;
                    let files = if seq != own_seq && self.options.files { platform::clipboard_files() } else { None };
                    if let Some(files) = files {
                        // Files copied here: they go across now, ready to paste there.
                        match xfer::clipfiles::send(&mut self.endpoint.lock().unwrap(), &files) {
                            Ok(n) => println!("copying {n} file(s) to the client's clipboard"),
                            Err(e) => platform::notify(&format!("Not copied to the other computer: {e}")),
                        }
                    } else if seq != own_seq
                        && let Some(text) = platform::clipboard_text()
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
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut v = self.0.lock().unwrap();
        let end = offset as usize + data.len();
        if v.len() < end {
            v.resize(end, 0);
        }
        v[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}

/// A file being received: written to `<name>.part`, renamed when complete.
struct FileSink {
    file: File,
    part: PathBuf,
    dest: PathBuf,
}

impl FileSink {
    fn create(name: &str) -> io::Result<FileSink> {
        let name = safe_file_name(name);
        let dir = platform::uploads_dir()?;
        let dest = unique_path(&dir, &name);
        let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap().to_string_lossy()));
        let file = platform::as_user(|| {
            std::fs::create_dir_all(&dir)?;
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&part)
        })?;
        Ok(FileSink { file, part, dest })
    }
}

impl Sink for FileSink {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    // Renaming and deleting happen as the user too: as SYSTEM, a junction in
    // their folder could turn these into moving or deleting any file.
    fn finish(self: Box<Self>) -> io::Result<()> {
        let FileSink { mut file, part, dest } = *self;
        file.flush()?;
        drop(file);
        platform::as_user(|| std::fs::rename(&part, &dest))
    }

    fn abort(self: Box<Self>) {
        let FileSink { file, part, .. } = *self;
        drop(file);
        let _ = platform::as_user(|| std::fs::remove_file(&part));
    }
}

#[cfg(windows)]
mod platform {
    pub use crate::windows::shell::*;
    pub fn notify(text: &str) {
        crate::windows::tray::notify(text);
    }
    /// The tray notification says where it is.
    pub fn reveal(_: &std::path::Path) {}
    // Files on the clipboard aren't shared by the Windows server yet.
    pub fn clipboard_files() -> Option<Vec<std::path::PathBuf>> {
        None
    }
    pub fn set_clipboard_files(_: &[std::path::PathBuf]) -> bool {
        false
    }
    pub fn clipboard_files_dir() -> std::io::Result<std::path::PathBuf> {
        Err(std::io::Error::other("not on Windows"))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub use crate::macos::shell::{
        as_user, clipboard_files, clipboard_files_dir, clipboard_sequence, clipboard_text, notify, reveal, set_clipboard_files,
        set_clipboard_text, uploads_dir,
    };
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    //! Elsewhere (the `--file` test mode): files go to `~/Downloads/DisplayWire`, no clipboard.
    use std::io;
    use std::path::PathBuf;

    pub fn clipboard_sequence() -> u32 {
        0
    }
    pub fn clipboard_text() -> Option<String> {
        None
    }
    pub fn set_clipboard_text(_: &str) -> bool {
        false
    }
    pub fn clipboard_files() -> Option<Vec<PathBuf>> {
        None
    }
    pub fn set_clipboard_files(_: &[PathBuf]) -> bool {
        false
    }
    pub fn clipboard_files_dir() -> io::Result<PathBuf> {
        Err(io::Error::other("no clipboard here"))
    }
    pub fn reveal(_: &std::path::Path) {}
    pub fn uploads_dir() -> io::Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("no home directory"))?;
        Ok(PathBuf::from(home).join("Downloads/DisplayWire"))
    }
    pub fn as_user<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        f()
    }
    pub fn notify(_: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_or_collide() {
        assert_eq!(safe_file_name("report.pdf"), "report.pdf");
        assert_eq!(safe_file_name("../../Windows/System32/evil.dll"), "evil.dll");
        assert_eq!(safe_file_name("C:\\Users\\x\\a.txt"), "a.txt");
        assert_eq!(safe_file_name("a<b>c:d\"e|f?g*h.txt"), "abcdefgh.txt");
        assert_eq!(safe_file_name("con.txt"), "_con.txt");
        assert_eq!(safe_file_name("NUL"), "_NUL");
        assert_eq!(safe_file_name("lpt3.log"), "_lpt3.log");
        assert_eq!(safe_file_name("com10.txt"), "com10.txt");
        assert_eq!(safe_file_name("   ...  "), "file");
        assert_eq!(safe_file_name(""), "file");
        assert_eq!(safe_file_name("trailing. "), "trailing");
        assert_eq!(safe_file_name("nul\0byte.txt"), "nulbyte.txt");
        let long = safe_file_name(&format!("{}.mp4", "x".repeat(400)));
        assert!(long.chars().count() <= 150 && long.ends_with(".mp4"));
    }

    #[test]
    fn unique_names_count_up() {
        let dir = std::env::temp_dir().join(format!("displaywire-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (1).txt"));
        std::fs::write(dir.join("a (1).txt.part"), "x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        std::fs::write(dir.join("noext"), "x").unwrap();
        assert_eq!(unique_path(&dir, "noext"), dir.join("noext (1)"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
