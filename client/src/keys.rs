//! Pairing keys, kept in `~/Library/Application Support/Streamy/keys`, one file
//! per server address (mode 0600) plus `default`, the key used last. See
//! `docs/AUTH.md`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use secure::Key;

fn dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/Streamy/keys"))
}

/// The server's address without a port, as a file name.
fn file_name(server: &str) -> String {
    let host = match server.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => server,
    };
    host.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// The key saved for `server`, else the last one used.
pub fn load(server: &str) -> Option<Key> {
    let dir = dir()?;
    [file_name(server), "default".into()]
        .iter()
        .find_map(|name| std::fs::read_to_string(dir.join(name)).ok().and_then(|t| Key::parse(&t).ok()))
}

pub fn save(server: &str, key: &Key) -> Result<(), String> {
    let dir = dir().ok_or("no home directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for name in [file_name(server), "default".into()] {
        let path = dir.join(name);
        std::fs::write(&path, "").map_err(|e| e.to_string())?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{key}\n")).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::file_name;

    #[test]
    fn names_ignore_the_port_and_case() {
        assert_eq!(file_name("Gaming-PC"), "gaming-pc");
        assert_eq!(file_name("gaming-pc:9000"), "gaming-pc");
        assert_eq!(file_name("100.64.0.1:9000"), "100.64.0.1");
        assert_eq!(file_name("../etc/x"), ".._etc_x");
        assert_eq!(file_name("fe80::1"), "fe80__1");
    }
}
