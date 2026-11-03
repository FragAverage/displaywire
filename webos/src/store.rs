//! Paired PCs: one line per PC, `address key name`, in a file only this user can read.

use std::path::PathBuf;

use secure::Key;

pub struct Server {
    pub address: String,
    pub key: Key,
    pub name: String,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: Option<PathBuf>) -> Store {
        let dir = dir.unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".displaywire-tv"));
        Store { dir }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("servers")
    }

    pub fn load(&self) -> Result<Vec<Server>, String> {
        let text = match std::fs::read_to_string(self.file()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", self.file().display())),
        };
        Ok(text
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, ' ');
                let address = parts.next()?.to_string();
                let key = Key::parse(parts.next()?).ok()?;
                Some(Server { address, key, name: parts.next().unwrap_or("").to_string() })
            })
            .collect())
    }

    pub fn find(&self, address: &str) -> Result<Option<Server>, String> {
        Ok(self.load()?.into_iter().find(|s| s.address.eq_ignore_ascii_case(address)))
    }

    pub fn upsert(&self, address: &str, name: &str, key: &Key) -> Result<(), String> {
        let mut servers = self.load()?;
        servers.retain(|s| !s.address.eq_ignore_ascii_case(address));
        servers.push(Server { address: address.to_string(), key: key.clone(), name: name.to_string() });
        let text: String = servers.iter().map(|s| format!("{} {} {}\n", s.address, s.key, s.name)).collect();
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        write_private(&self.file(), &text).map_err(|e| format!("{}: {e}", self.file().display()))
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(text.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
}
