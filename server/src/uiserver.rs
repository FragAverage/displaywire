//! The server's side of the device-management window. The window is a separate process
//! (`displaywire-server --ui`), so a problem in it can never interrupt the stream. It
//! talks to this one over a loopback TCP connection, one text line per request, and has
//! to open with a random token that only the server (which launches it) and
//! administrators (it's in a file only they can read, for `--pair` over SSH) know.
//!
//! ```text
//! hello <token>   (first line)      state            -> lines, then "end"
//! pair-start | pair-stop | pair-allow | pair-deny    -> "ok"
//! remove <id>                                        -> "ok" or "error <why>"
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::pairing::{Outcome, View};
use crate::{Shared, auth, clients};

pub struct Handle {
    pub port: u16,
    pub token: String,
}

/// Starts listening on a loopback port. Writes `ui` in the data folder with the port and token.
pub fn start(shared: Arc<Shared>, dir: &Path) -> Result<Handle, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("ui listener: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).expect("system random number generator");
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    auth::write_private(&dir.join("ui"), &format!("{port} {token}\n"))?;
    let expected = token.clone();
    std::thread::Builder::new()
        .name("ui-server".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let (shared, token) = (shared.clone(), expected.clone());
                let _ = std::thread::Builder::new().name("ui-client".into()).spawn(move || serve(stream, &shared, &token));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Handle { port, token })
}

fn serve(stream: TcpStream, shared: &Shared, token: &str) {
    let Ok(mut out) = stream.try_clone() else { return };
    let mut lines = BufReader::new(stream).lines();
    // Whoever can't say the token gets nothing, not even an error.
    let ok = matches!(lines.next(), Some(Ok(l)) if l.strip_prefix("hello ").is_some_and(|t| same(t.trim(), token)));
    if !ok {
        return;
    }
    shared.ui_open.fetch_add(1, Relaxed);
    for line in lines.map_while(Result::ok) {
        let reply = command(shared, line.trim());
        if out.write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
    shared.ui_open.fetch_sub(1, Relaxed);
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn command(shared: &Shared, line: &str) -> String {
    let now = timing::now_ns();
    let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
    match verb {
        "state" => state(shared, now),
        "pair-start" => {
            shared.pairing.start(now);
            "ok\n".into()
        }
        "pair-stop" => {
            shared.pairing.stop(now);
            "ok\n".into()
        }
        "pair-allow" => {
            shared.pairing.allow(now);
            "ok\n".into()
        }
        "pair-deny" => {
            shared.pairing.deny(now);
            "ok\n".into()
        }
        "remove" => match shared.clients.lock().unwrap().remove(arg.trim()) {
            Ok(Some(c)) => {
                println!("removed \"{}\"; its key no longer works", c.name);
                "ok\n".into()
            }
            Ok(None) => "error no such device\n".into(),
            Err(e) => format!("error {}\n", e.replace('\n', " ")),
        },
        _ => "error unknown command\n".into(),
    }
}

fn state(shared: &Shared, now: u64) -> String {
    let mut out = String::new();
    let snap = shared.pairing.snapshot(now);
    out += &match snap.view {
        View::Off => "pairing off\n".to_string(),
        View::Listening { secs_left } => format!("pairing listening\t{secs_left}\n"),
        View::Asking { name, addr, secs_left } => format!("pairing asking\t{secs_left}\t{name}\t{addr}\n"),
        View::Pin { name, addr, pin, tries_left, secs_left } => format!("pairing pin\t{secs_left}\t{name}\t{addr}\t{pin}\t{tries_left}\n"),
    };
    if let Some(last) = snap.last {
        out += &match last {
            Outcome::Paired(n) => format!("result paired\t{n}\n"),
            Outcome::Denied(n) => format!("result denied\t{n}\n"),
            Outcome::Locked(n) => format!("result locked\t{n}\n"),
            Outcome::TimedOut(n) => format!("result timed-out\t{n}\n"),
            Outcome::Cancelled(n) => format!("result cancelled\t{n}\n"),
        };
    }
    let connected = shared.link.lock().unwrap().as_ref().map(|l| l.client.to_string());
    for c in shared.clients.lock().unwrap().list() {
        let here = connected.as_deref() == Some(c.id.as_str());
        out += &format!("device {}\t{}\t{}\t{}\t{}\t{}\n", c.id, c.name, c.created, c.last_seen, c.last_addr, u8::from(here));
    }
    if shared.ui_front.swap(false, Relaxed) {
        out += "front\n";
    }
    out += &format!("now {}\n", clients::now_s());
    out += "end\n";
    out
}

/// A request/response connection, for the window and for `--pair`.
pub struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    pub fn connect(port: u16, token: &str) -> Result<Client, String> {
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map_err(|e| format!("can't reach the server: {e}"))?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        let mut c = Client { reader: BufReader::new(stream), writer };
        writeln!(c.writer, "hello {token}").map_err(|e| e.to_string())?;
        Ok(c)
    }

    /// Reads the port and token the running server wrote, then connects (needs administrator rights).
    pub fn connect_from_file(dir: &Path) -> Result<Client, String> {
        let text = auth::read_private(&dir.join("ui"))?.ok_or("the server isn't running (no ui file)")?;
        let (port, token) = text.trim().split_once(' ').ok_or("damaged ui file")?;
        Client::connect(port.parse().map_err(|_| "damaged ui file")?, token)
    }

    /// Sends one command; returns the reply lines (without the final `end`/`ok`).
    pub fn request(&mut self, line: &str) -> Result<Vec<String>, String> {
        writeln!(self.writer, "{line}").map_err(|e| e.to_string())?;
        let mut lines = Vec::new();
        loop {
            let mut l = String::new();
            if self.reader.read_line(&mut l).map_err(|e| e.to_string())? == 0 {
                return Err("the server closed the connection".into());
            }
            let l = l.trim_end_matches(['\r', '\n']).to_string();
            if l == "end" || l == "ok" {
                return Ok(lines);
            }
            if let Some(e) = l.strip_prefix("error ") {
                return Err(e.to_string());
            }
            lines.push(l);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::Pairing;

    fn shared() -> (Arc<Shared>, std::path::PathBuf) {
        let mut r = [0u8; 4];
        getrandom::fill(&mut r).unwrap();
        let dir = std::env::temp_dir().join(format!("dw-ui-{}", r.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        std::fs::create_dir_all(&dir).unwrap();
        (Arc::new(Shared::new(clients::Store::open(&dir).unwrap(), Pairing::new("PC"))), dir)
    }

    #[test]
    fn the_window_can_list_remove_and_drive_pairing() {
        let (shared, dir) = shared();
        shared.clients.lock().unwrap().add("Jamie's MacBook", secure::Key::generate()).unwrap();
        let handle = start(shared.clone(), &dir).unwrap();
        let mut ui = Client::connect(handle.port, &handle.token).unwrap();
        let s = ui.request("state").unwrap();
        assert!(s.iter().any(|l| l == "pairing off"));
        let device = s.iter().find(|l| l.starts_with("device ")).expect("a device line");
        let id = device["device ".len()..].split('\t').next().unwrap().to_string();
        ui.request("pair-start").unwrap();
        assert!(ui.request("state").unwrap().iter().any(|l| l.starts_with("pairing listening")));
        ui.request("pair-stop").unwrap();
        assert!(ui.request("remove nobody").is_err());
        ui.request(&format!("remove {id}")).unwrap();
        assert!(shared.clients.lock().unwrap().list().is_empty());
        assert!(ui.request("bogus").is_err());
        // The file for `--pair` works too.
        let mut from_file = Client::connect_from_file(&dir).unwrap();
        assert!(from_file.request("state").is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_connection_without_the_token_gets_nothing() {
        let (shared, dir) = shared();
        let handle = start(shared.clone(), &dir).unwrap();
        let mut stranger = TcpStream::connect((Ipv4Addr::LOCALHOST, handle.port)).unwrap();
        stranger.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        writeln!(stranger, "hello not-the-token").unwrap();
        writeln!(stranger, "pair-start").unwrap();
        let mut buf = String::new();
        let n = BufReader::new(stranger).read_line(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "closed without a word");
        assert_eq!(shared.pairing.snapshot(timing::now_ns()).view, View::Off, "and nothing was done");
        assert_eq!(shared.ui_open.load(Relaxed), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
