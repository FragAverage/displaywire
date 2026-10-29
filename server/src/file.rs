//! Loops an HEVC Annex B file at a fixed frame rate. Stands in for screen
//! capture when testing the network path and client.

use std::path::Path;

use timing::{now_ns, sleep_until};

use crate::{EncodedFrame, Source};

pub struct FileSource {
    units: Vec<(Vec<u8>, bool)>,
    next: usize,
    interval_ns: u64,
    next_due: u64,
}

impl FileSource {
    pub fn open(path: &Path, fps: u32) -> Result<FileSource, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let units: Vec<_> = hevc::access_units(&data).into_iter().map(|au| (au.to_vec(), hevc::is_keyframe(au))).collect();
        if !units.first().is_some_and(|u| u.1) {
            return Err(format!("{}: must start with a keyframe and use access unit delimiters", path.display()));
        }
        println!("streaming {} ({} frames) at {fps} fps", path.display(), units.len());
        Ok(FileSource { units, next: 0, interval_ns: 1_000_000_000 / fps as u64, next_due: now_ns() })
    }
}

impl Source for FileSource {
    fn next_frame(&mut self, force_keyframe: bool, _timeout_ms: u32) -> Result<Option<EncodedFrame>, String> {
        sleep_until(self.next_due);
        self.next_due = self.next_due.max(now_ns().saturating_sub(self.interval_ns)) + self.interval_ns;
        if force_keyframe || self.next == self.units.len() {
            self.next = 0;
        }
        let (data, keyframe) = self.units[self.next].clone();
        self.next += 1;
        Ok(Some(EncodedFrame { data, keyframe, capture_ns: now_ns() }))
    }
}
