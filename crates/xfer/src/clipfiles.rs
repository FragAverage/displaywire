//! Files copied to the clipboard on one computer, pasted on the other (and files dropped on
//! the stream window, which travel the same way as `Kind::DropFile`).
//!
//! When files are copied, they are sent at once, as a batch: each one's name carries the
//! batch, its place in it and the batch's size (`batch/index/count/name`). They arrive in a
//! folder of their own, and when the whole batch is there the receiving side puts those files
//! on its clipboard, so pasting them (in Finder or Explorer) copies them from that folder.
//! Copying again starts a new batch and the last one's folder is deleted.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{Endpoint, Kind, Meta, Sink};

/// A batch bigger than this isn't sent: copying a whole disk shouldn't start a transfer.
pub const MAX_BATCH_BYTES: u64 = 2 << 30;
/// Files in a batch.
pub const MAX_BATCH_FILES: usize = 200;

fn batch_name(batch: u32, index: u32, count: u32, file: &str) -> String {
    format!("{batch:08x}/{index}/{count}/{file}")
}

/// `(batch, index, count, file name)`.
fn parse(name: &str) -> Option<(u32, u32, u32, &str)> {
    let mut it = name.splitn(4, '/');
    let batch = u32::from_str_radix(it.next()?, 16).ok()?;
    let index = it.next()?.parse().ok()?;
    let count = it.next()?.parse().ok()?;
    let file = it.next()?;
    (count > 0 && count as usize <= MAX_BATCH_FILES && index < count).then_some((batch, index, count, file))
}

/// Sends `paths` as a new batch. Folders aren't sent (Finder and Explorer copy them as one
/// item, which would need zipping); returns how many files were sent.
pub fn send(endpoint: &mut Endpoint, paths: &[PathBuf], kind: Kind) -> Result<usize, String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    for p in paths.iter().take(MAX_BATCH_FILES) {
        let Ok(f) = File::open(p) else { continue };
        let Ok(md) = f.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        total += md.len();
        let name = p.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        files.push((f, md.len(), name));
    }
    if files.is_empty() {
        return Err("only files are copied across, not folders".into());
    }
    if total > MAX_BATCH_BYTES {
        return Err(format!("{} MB is too much to copy across: send it instead", total >> 20));
    }
    let batch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u32) | 1;
    let count = files.len() as u32;
    for (i, (f, size, name)) in files.into_iter().enumerate() {
        endpoint.send(Meta { kind, size, name: batch_name(batch, i as u32, count, &name) }, Box::new(f));
    }
    Ok(count as usize)
}

/// The receiving side.
pub struct Inbox {
    dir: PathBuf,
    /// The batch arriving: its id and where each of its files went, by index.
    batch: Option<(u32, HashMap<u32, PathBuf>)>,
    /// Where each file being received is going (its full transfer name → path).
    pending: HashMap<String, PathBuf>,
}

impl Inbox {
    /// Files arrive in folders inside `dir`, which holds nothing else.
    pub fn new(dir: PathBuf) -> Arc<Mutex<Inbox>> {
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(Mutex::new(Inbox { dir, batch: None, pending: HashMap::new() }))
    }

    /// Where an arriving clipboard file goes (`Endpoint`'s sink factory calls this).
    pub fn sink(&mut self, meta: &Meta) -> io::Result<Box<dyn Sink>> {
        let (batch, _, _, file) = parse(&meta.name).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad clipboard file name"))?;
        if self.batch.as_ref().is_none_or(|b| b.0 != batch) {
            // A new copy: the last one's files go.
            if let Some((old, _)) = self.batch.take() {
                let _ = std::fs::remove_dir_all(self.dir.join(format!("{old:08x}")));
            }
            self.batch = Some((batch, HashMap::new()));
            self.pending.clear();
        }
        let dir = self.dir.join(format!("{batch:08x}"));
        std::fs::create_dir_all(&dir)?;
        let sink = PartFile::create(&dir, file)?;
        self.pending.insert(meta.name.clone(), sink.dest.clone());
        Ok(Box::new(sink))
    }

    /// A clipboard file arrived; when it completes its batch, the batch's files in order.
    pub fn received(&mut self, meta: &Meta) -> Option<Vec<PathBuf>> {
        let (batch, index, count, _) = parse(&meta.name)?;
        let path = self.pending.remove(&meta.name)?;
        let (current, got) = self.batch.as_mut()?;
        if *current != batch {
            return None;
        }
        got.insert(index, path);
        (got.len() == count as usize).then(|| (0..count).filter_map(|i| got.get(&i).cloned()).collect())
    }
}

/// A file being received: written to `<name>.part`, renamed when complete.
pub struct PartFile {
    file: File,
    part: PathBuf,
    pub dest: PathBuf,
}

impl PartFile {
    /// In `dir`, named after `name` made safe and not replacing anything there.
    pub fn create(dir: &Path, name: &str) -> io::Result<PartFile> {
        let dest = crate::unique_path(dir, &crate::safe_file_name(name));
        let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap().to_string_lossy()));
        let file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&part)?;
        Ok(PartFile { file, part, dest })
    }
}

impl Sink for PartFile {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        let PartFile { file, part, dest } = *self;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&part, &dest)
    }

    fn abort(self: Box<Self>) {
        let PartFile { file, part, .. } = *self;
        drop(file);
        let _ = std::fs::remove_file(&part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_the_batch() {
        let n = batch_name(0xabc, 2, 3, "a/b.txt");
        assert_eq!(parse(&n), Some((0xabc, 2, 3, "a/b.txt")));
        assert_eq!(parse("zz/0/1/x"), None);
        assert_eq!(parse("1/3/3/x"), None);
        assert_eq!(parse("1/0/0/x"), None);
    }

    #[test]
    fn a_batch_completes_and_the_next_replaces_it() {
        let dir = std::env::temp_dir().join(format!("dw-clipfiles-{}", std::process::id()));
        let inbox = Inbox::new(dir.clone());
        let mut ib = inbox.lock().unwrap();
        let metas: Vec<_> = (0..2).map(|i| Meta { kind: Kind::ClipboardFile, size: 1, name: batch_name(7, i, 2, &format!("f{i}.txt")) }).collect();
        for m in &metas {
            let mut s = ib.sink(m).unwrap();
            s.write_at(0, b"x").unwrap();
            s.finish().unwrap();
        }
        assert_eq!(ib.received(&metas[1]), None);
        let paths = ib.received(&metas[0]).unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("f0.txt") && paths[0].exists());
        let next = Meta { kind: Kind::ClipboardFile, size: 1, name: batch_name(8, 0, 1, "g.txt") };
        ib.sink(&next).unwrap().finish().unwrap();
        assert!(!paths[0].exists(), "the last batch's files are deleted");
        assert!(ib.received(&next).unwrap()[0].exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
