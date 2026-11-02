//! The paired devices: each has a key of its own, so one can be removed without
//! affecting the others. Stored in `clients` in the data folder, one tab-separated
//! line per device, readable only by administrators. See `docs/PAIRING.md`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use secure::Key;

use crate::auth;

#[derive(Clone)]
pub struct Client {
    /// Short random id, what `--remove-client` and the UI refer to.
    pub id: String,
    pub name: String,
    pub key: Key,
    /// Unix seconds.
    pub created: u64,
    pub last_seen: u64,
    pub last_addr: String,
}

pub struct Store {
    path: PathBuf,
    clients: Vec<Client>,
    /// What the file looked like when last read, to notice changes made by another process.
    stamp: Option<(SystemTime, u64)>,
}

fn new_id() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("system random number generator");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn parse(text: &str) -> Vec<Client> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 6 {
                return None;
            }
            Some(Client {
                id: f[0].to_string(),
                name: f[1].to_string(),
                key: Key::parse(f[2]).ok()?,
                created: f[3].parse().ok()?,
                last_seen: f[4].parse().ok()?,
                last_addr: f[5].to_string(),
            })
        })
        .collect()
}

fn format(clients: &[Client]) -> String {
    clients.iter().map(|c| format!("{}\t{}\t{}\t{}\t{}\t{}\n", c.id, c.name, c.key, c.created, c.last_seen, c.last_addr)).collect()
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

pub fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A unix time as `2026-11-02 10:49 UTC`, for listings.
pub fn format_date(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3_600, rem % 3_600 / 60)
}

impl Store {
    /// Opens the store in `dir`. The first time, a key from earlier versions of the server (which had one
    /// key for every device) is kept as a device called "Previous device", so nobody is locked out by
    /// the upgrade; remove it once the devices have paired again.
    pub fn open(dir: &Path) -> Result<Store, String> {
        let path = dir.join("clients");
        let mut store = Store { path: path.clone(), clients: Vec::new(), stamp: None };
        match auth::read_private(&path)? {
            Some(text) => store.clients = parse(&text),
            None => {
                if let Some(text) = auth::read_private(&dir.join("key"))?
                    && let Ok(key) = Key::parse(&text)
                {
                    let now = now_s();
                    store.clients.push(Client { id: new_id(), name: "Previous device".into(), key, created: now, last_seen: 0, last_addr: String::new() });
                    store.save()?;
                    println!("kept the pairing key from an earlier version as the device \"Previous device\"; remove it once your devices have paired with a PIN");
                }
            }
        }
        store.stamp = stamp(&path);
        Ok(store)
    }

    fn save(&mut self) -> Result<(), String> {
        auth::write_private(&self.path, &format(&self.clients))?;
        self.stamp = stamp(&self.path);
        Ok(())
    }

    pub fn list(&self) -> &[Client] {
        &self.clients
    }

    pub fn get(&self, id: &str) -> Option<&Client> {
        self.clients.iter().find(|c| c.id == id)
    }

    /// Adds a device and writes the file. A name that's taken gets " (2)", " (3)"…
    pub fn add(&mut self, name: &str, key: Key) -> Result<Client, String> {
        let base = secure::pair::clean_name(name).replace(['\t', '\n', '\r'], " ");
        let mut name = base.clone();
        let mut n = 2;
        while self.clients.iter().any(|c| c.name.eq_ignore_ascii_case(&name)) {
            name = format!("{base} ({n})");
            n += 1;
        }
        let client = Client { id: new_id(), name, key, created: now_s(), last_seen: 0, last_addr: String::new() };
        self.clients.push(client.clone());
        self.save()?;
        Ok(client)
    }

    /// Removes a device by id or by name, destroying its key.
    pub fn remove(&mut self, id_or_name: &str) -> Result<Option<Client>, String> {
        let by_id = self.clients.iter().position(|c| c.id == id_or_name);
        let by_name = || {
            let mut matches = self.clients.iter().enumerate().filter(|(_, c)| c.name.eq_ignore_ascii_case(id_or_name));
            match (matches.next(), matches.next()) {
                (Some((i, _)), None) => Some(i),
                _ => None,
            }
        };
        let Some(i) = by_id.or_else(by_name) else { return Ok(None) };
        let removed = self.clients.remove(i);
        self.save()?;
        Ok(Some(removed))
    }

    /// Notes that a device connected from `addr`.
    pub fn touch(&mut self, id: &str, addr: SocketAddr) {
        if let Some(c) = self.clients.iter_mut().find(|c| c.id == id) {
            c.last_seen = now_s();
            c.last_addr = addr.ip().to_string();
            if let Err(e) = self.save() {
                eprintln!("clients: {e}");
            }
        }
    }

    /// Picks up changes made to the file by another process (`--remove-client`). True if there were any.
    pub fn reload_if_changed(&mut self) -> bool {
        let now = stamp(&self.path);
        if now == self.stamp {
            return false;
        }
        match auth::read_private(&self.path) {
            Ok(Some(text)) => self.clients = parse(&text),
            Ok(None) => self.clients.clear(),
            Err(e) => {
                eprintln!("clients: {e}");
                return false;
            }
        }
        self.stamp = now;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut r = [0u8; 4];
        getrandom::fill(&mut r).unwrap();
        let dir = std::env::temp_dir().join(format!("dw-clients-{tag}-{}", r.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn devices_are_kept_and_removed_with_their_keys() {
        let dir = temp_dir("a");
        let mut s = Store::open(&dir).unwrap();
        assert!(s.list().is_empty());
        let (k1, k2) = (Key::generate(), Key::generate());
        let a = s.add("Jamie's MacBook", k1.clone()).unwrap();
        let b = s.add("jamie's macbook", k2.clone()).unwrap();
        assert_eq!(b.name, "jamie's macbook (2)", "names stay distinct");
        // A fresh open sees them.
        let mut again = Store::open(&dir).unwrap();
        assert_eq!(again.list().len(), 2);
        assert!(again.get(&a.id).unwrap().key == k1);
        // Removing by name, and by id.
        assert!(again.remove("Jamie's MacBook").unwrap().is_some());
        assert!(again.remove("nobody").unwrap().is_none());
        assert_eq!(again.list().len(), 1);
        assert!(again.remove(&b.id).unwrap().is_some());
        assert!(Store::open(&dir).unwrap().list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_key_from_an_earlier_version_becomes_a_device_once() {
        let dir = temp_dir("b");
        let old = Key::generate();
        std::fs::write(dir.join("key"), format!("{old}\n")).unwrap();
        let mut s = Store::open(&dir).unwrap();
        assert_eq!(s.list().len(), 1);
        assert_eq!(s.list()[0].name, "Previous device");
        assert!(s.list()[0].key == old);
        // Removing it must not bring it back on the next start.
        s.remove("Previous device").unwrap();
        assert!(Store::open(&dir).unwrap().list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn another_process_changing_the_file_is_noticed() {
        let dir = temp_dir("c");
        let mut running = Store::open(&dir).unwrap();
        let mut cli = Store::open(&dir).unwrap();
        let c = cli.add("Laptop", Key::generate()).unwrap();
        assert!(running.reload_if_changed());
        assert_eq!(running.list().len(), 1);
        assert!(!running.reload_if_changed(), "nothing new");
        cli.remove(&c.id).unwrap();
        assert!(running.reload_if_changed());
        assert!(running.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dates_are_formatted() {
        assert_eq!(format_date(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_date(1_793_616_540), "2026-11-02 10:49 UTC");
        assert_eq!(format_date(951_782_400 + 3_661), "2000-02-29 01:01 UTC");
    }

    #[test]
    fn garbage_lines_are_skipped() {
        let parsed = parse("junk\n\nid1\tName\tnot-a-key\t1\t2\tx\n");
        assert!(parsed.is_empty());
    }
}
