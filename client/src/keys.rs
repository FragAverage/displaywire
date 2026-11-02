//! Pairing keys, now kept with the saved PCs in `servers`. Earlier versions kept one file
//! per server address in `~/Library/Application Support/DisplayWire/keys`; those are still
//! read, once, to import them. See `docs/PAIRING.md`.

use std::path::PathBuf;

use secure::Key;

use crate::servers;

fn old_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/DisplayWire/keys"))
}

/// The server's address without a port, as an old file name.
fn file_name(server: &str) -> String {
    let host = match server.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => server,
    };
    host.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// A key from the old per-address files (or the `default` one), for importing.
pub fn load_old(server: &str) -> Option<Key> {
    let dir = old_dir()?;
    [file_name(server), "default".into()]
        .iter()
        .find_map(|name| std::fs::read_to_string(dir.join(name)).ok().and_then(|t| Key::parse(&t).ok()))
}

/// The key this Mac was given by `server`.
pub fn load(server: &str) -> Option<Key> {
    servers::key_for(server)
}

pub fn save(server: &str, key: &Key) -> Result<(), String> {
    servers::upsert(server, None, Some(key.clone()))
}

#[cfg(test)]
mod tests {
    use super::file_name;

    #[test]
    fn old_names_ignore_the_port_and_case() {
        assert_eq!(file_name("Gaming-PC"), "gaming-pc");
        assert_eq!(file_name("gaming-pc:9000"), "gaming-pc");
        assert_eq!(file_name("100.64.0.1:9000"), "100.64.0.1");
        assert_eq!(file_name("../etc/x"), ".._etc_x");
        assert_eq!(file_name("fe80::1"), "fe80__1");
    }
}
