//! The webOS toolchain links against glibc 2.12 (it targets every webOS version) but Rust's
//! standard library expects 2.17's `getauxval`. This reads the auxiliary vector itself; a TV
//! with a newer glibc still uses this one, which gives the same answers.

use std::io::Read;

#[unsafe(no_mangle)]
pub extern "C" fn getauxval(kind: u32) -> u32 {
    let mut auxv = Vec::new();
    if std::fs::File::open("/proc/self/auxv").and_then(|mut f| f.read_to_end(&mut auxv)).is_err() {
        return 0;
    }
    // Pairs of native words (u32 on armv7): type, value; type 0 ends it.
    for pair in auxv.chunks_exact(8) {
        let (k, v) = (u32::from_ne_bytes(pair[..4].try_into().unwrap()), u32::from_ne_bytes(pair[4..].try_into().unwrap()));
        if k == 0 {
            break;
        }
        if k == kind {
            return v;
        }
    }
    0
}
