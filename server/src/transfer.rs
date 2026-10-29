//! Clipboard sharing and file upload: the server's side of `xfer`.
//!
//! Text copied on either machine appears on the other. Files sent from the
//! client are saved in the logged-in user's `Downloads\Streamy` folder.
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
use xfer::{Bytes, Endpoint, Event, Kind, Meta, Outgoing, Sink};

use crate::Shared;

/// Clipboard text beyond this is neither sent nor accepted.
const MAX_CLIPBOARD: u64 = 1 << 20;
const CLIPBOARD_POLL_NS: u64 = 250_000_000;
/// Uploads to the client (there are none yet) and clipboard text go at this rate.
const SEND_RATE: u64 = 5_000_000;

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
    /// Folder uploads go to, once known.
    last_dir: Arc<Mutex<Option<PathBuf>>>,
}

fn make_endpoint(options: &Arc<Options>, slot: &Arc<Mutex<Vec<u8>>>, last_dir: &Arc<Mutex<Option<PathBuf>>>) -> Endpoint {
    let (opts, slot, last_dir) = (options.clone(), slot.clone(), last_dir.clone());
    Endpoint::new(options.max_file_bytes.max(MAX_CLIPBOARD), SEND_RATE, move |meta| match meta.kind {
        Kind::Clipboard => {
            if !opts.clipboard || meta.size > MAX_CLIPBOARD {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "clipboard sharing is off or the text is too large"));
            }
            slot.lock().unwrap().clear();
            Ok(Box::new(ClipboardSink(slot.clone())) as Box<dyn Sink>)
        }
        Kind::File => {
            if !opts.files || meta.size > opts.max_file_bytes {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "file upload is off or the file is too large"));
            }
            let sink = FileSink::create(&meta.name)?;
            *last_dir.lock().unwrap() = sink.dest.parent().map(Path::to_path_buf);
            Ok(Box::new(sink))
        }
    })
}

impl Transfers {
    pub fn new(options: Options) -> Arc<Transfers> {
        let options = Arc::new(options);
        let (slot, last_dir) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
        Arc::new(Transfers { endpoint: Mutex::new(make_endpoint(&options, &slot, &last_dir)), options, slot, last_dir })
    }

    /// A transfer message from the client; returns what to send back.
    pub fn handle(&self, now: u64, msg: &Msg) -> Vec<Outgoing> {
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
                    *self.endpoint.lock().unwrap() = make_endpoint(&self.options, &self.slot, &self.last_dir);
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
                    Event::Received { meta, .. } => {
                        let dir = self.last_dir.lock().unwrap().clone().map(|d| d.display().to_string()).unwrap_or_default();
                        println!("received {} ({} bytes) in {dir}", meta.name, meta.size);
                        platform::notify(&format!("Received {}", meta.name));
                    }
                    Event::ReceiveFailed { why, .. } => eprintln!("transfer from the client failed: {why}"),
                    Event::Sent { .. } | Event::SendFailed { .. } => {}
                }
            }
            if self.options.clipboard && now >= next_poll {
                next_poll = now + CLIPBOARD_POLL_NS;
                let seq = platform::clipboard_sequence();
                if seq != last_seq {
                    last_seq = seq;
                    if seq != own_seq
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

    fn finish(self: Box<Self>) -> io::Result<()> {
        let FileSink { mut file, part, dest } = *self;
        file.flush()?;
        drop(file);
        std::fs::rename(&part, &dest)
    }

    fn abort(self: Box<Self>) {
        let FileSink { file, part, .. } = *self;
        drop(file);
        let _ = std::fs::remove_file(part);
    }
}

/// A file name that can't escape the folder or collide with a Windows device name.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut clean: String = base.chars().filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c)).collect();
    clean = clean.trim_matches([' ', '.']).to_string();
    let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if clean.is_empty() {
        return "file".into();
    }
    if reserved {
        clean.insert(0, '_');
    }
    // Keep the extension when shortening.
    if clean.chars().count() > 150 {
        let ext: String = clean.rsplit_once('.').filter(|(_, e)| e.len() <= 12).map(|(_, e)| format!(".{e}")).unwrap_or_default();
        let keep = 150 - ext.chars().count();
        clean = clean.chars().take(keep).collect::<String>() + &ext;
    }
    clean
}

/// `name`, or `name (1)`, `name (2)`… if that exists already.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    let taken = |p: &Path| p.exists() || p.with_file_name(format!("{}.part", p.file_name().unwrap().to_string_lossy())).exists();
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !taken(p))
        .unwrap()
}

#[cfg(windows)]
mod platform {
    pub use crate::windows::shell::*;
    pub fn notify(text: &str) {
        crate::windows::tray::notify(text);
    }
}

#[cfg(not(windows))]
mod platform {
    //! Off Windows (the `--file` test mode): files go to `~/Downloads/Streamy`, no clipboard.
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
    pub fn uploads_dir() -> io::Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("no home directory"))?;
        Ok(PathBuf::from(home).join("Downloads/Streamy"))
    }
    pub fn as_user<T>(f: impl FnOnce() -> T) -> T {
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
        let dir = std::env::temp_dir().join(format!("streamy-test-{}", std::process::id()));
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
