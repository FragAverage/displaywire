//! The PCs this Mac knows: name, address and the key this Mac was given when it paired
//! (see `docs/PAIRING.md`). One file in `~/Library/Application Support/DisplayWire`, mode
//! 0600 since it holds keys. Earlier versions kept a list of recent addresses and one key
//! file per address; the first run imports those.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use secure::Key;

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    /// What is connected to: `host` or `host:port`.
    pub address: String,
    /// What it calls itself (or the address, until it has said).
    pub name: String,
    /// `None` until this Mac has paired with it.
    pub key: Option<Key>,
    /// Unix seconds of the last connection, 0 for never.
    pub last_used: u64,
}

fn file() -> Option<PathBuf> {
    crate::dialog::state_file("pcs")
}

fn parse(text: &str) -> Vec<Server> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 4 || f[0].is_empty() {
                return None;
            }
            Some(Server { address: f[0].into(), name: f[1].into(), key: Key::parse(f[2]).ok(), last_used: f[3].parse().unwrap_or(0) })
        })
        .collect()
}

fn format(servers: &[Server]) -> String {
    servers
        .iter()
        .map(|s| format!("{}\t{}\t{}\t{}\n", s.address, s.name.replace(['\t', '\n'], " "), s.key.as_ref().map_or_else(|| "-".to_string(), |k| k.to_string()), s.last_used))
        .collect()
}

/// Saved PCs, most recently used first (never-used ones in the order they were added).
pub fn load() -> Vec<Server> {
    let Some(path) = file() else { return Vec::new() };
    let mut list = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(_) => {
            let imported = import_old();
            if !imported.is_empty() {
                let _ = write(&imported);
            }
            imported
        }
    };
    list.sort_by_key(|s| std::cmp::Reverse(s.last_used));
    list
}

/// The recent-servers list and per-address key files from before this store existed.
fn import_old() -> Vec<Server> {
    crate::dialog::recent_servers()
        .into_iter()
        // An old discovery bug saved a broadcast address (a Windows server answered from it) as if it were a PC.
        .filter(|address| !address.split(':').next().is_some_and(|h| h.ends_with(".255")))
        .map(|address| {
            let key = crate::keys::load_old(&address);
            Server { name: address.clone(), address, key, last_used: 0 }
        })
        .collect()
}

fn write(servers: &[Server]) -> Result<(), String> {
    let path = file().ok_or("no home directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // Empty and private first, then filled in, so the keys are never readable by others.
    std::fs::write(&path, "").map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    std::fs::write(&path, format(servers)).map_err(|e| e.to_string())
}

/// Whether two addresses name the same host (the port is compared separately, with `port`).
pub fn same_host(a: &str, b: &str) -> bool {
    let host = |s: &str| match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h.to_lowercase(),
        _ => s.trim().to_lowercase(),
    };
    host(a) == host(b)
}

pub fn port(s: &str) -> u16 {
    s.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(9000)
}

pub fn find(address: &str) -> Option<Server> {
    load().into_iter().find(|s| same_host(&s.address, address) && port(&s.address) == port(address))
}

pub fn key_for(address: &str) -> Option<Key> {
    find(address).and_then(|s| s.key)
}

/// Adds the PC, or updates it. `None` for name or key leaves what is saved.
pub fn upsert(address: &str, name: Option<&str>, key: Option<Key>) -> Result<(), String> {
    let mut list = load();
    match list.iter_mut().find(|s| same_host(&s.address, address) && port(&s.address) == port(address)) {
        Some(s) => {
            if let Some(n) = name {
                s.name = n.to_string();
            }
            if key.is_some() {
                s.key = key;
            }
        }
        None => list.push(Server { address: address.trim().to_string(), name: name.unwrap_or(address.trim()).to_string(), key, last_used: 0 }),
    }
    write(&list)
}

pub fn touch(address: &str) {
    let mut list = load();
    if let Some(s) = list.iter_mut().find(|s| same_host(&s.address, address) && port(&s.address) == port(address)) {
        s.last_used = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let _ = write(&list);
    }
}

/// Forgets the PC and the key for it. (The PC itself still lists this Mac until it is removed there.)
pub fn forget(address: &str) -> Result<(), String> {
    let mut list = load();
    list.retain(|s| !(same_host(&s.address, address) && port(&s.address) == port(address)));
    write(&list)
}

/// Forgets only the key, e.g. after the PC removed this Mac: the PC stays listed, to be paired again.
pub fn unpair(address: &str) -> Result<(), String> {
    let mut list = load();
    for s in list.iter_mut().filter(|s| same_host(&s.address, address) && port(&s.address) == port(address)) {
        s.key = None;
    }
    write(&list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip_and_junk_is_skipped() {
        let k = Key::generate();
        let list = vec![
            Server { address: "gaming-pc".into(), name: "Gaming PC".into(), key: Some(k.clone()), last_used: 42 },
            Server { address: "10.0.0.5:9100".into(), name: "10.0.0.5:9100".into(), key: None, last_used: 0 },
        ];
        let text = format(&list);
        assert_eq!(parse(&text), list);
        assert!(parse("junk\n\n\t\t\t\n").is_empty());
    }

    #[test]
    fn addresses_match_ignoring_case_and_the_default_port() {
        assert!(same_host("Gaming-PC", "gaming-pc:9000") && port("gaming-pc") == port("gaming-pc:9000"));
        assert!(same_host("1.2.3.4:9100", "1.2.3.4:9100"));
        assert!(same_host("1.2.3.4:9100", "1.2.3.4") && port("1.2.3.4:9100") != port("1.2.3.4"), "other port, other server");
        assert!(!same_host("a", "b"));
    }
}
