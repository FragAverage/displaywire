//! Release signing, run by CI: `dwsign keygen`, or `dwsign release VERSION MAC.zip WINDOWS.zip OUT_DIR`
//! (signing key in the `UPDATE_SIGNING_KEY` environment variable).

use std::path::Path;

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["keygen"] => {
            let (seed, public) = update::sign::generate()?;
            println!("private {seed}\npublic  {public}");
            Ok(())
        }
        ["release", version, mac, windows, out] => {
            let seed = std::env::var("UPDATE_SIGNING_KEY").map_err(|_| "UPDATE_SIGNING_KEY is not set")?;
            let seed = seed.trim();
            if update::sign::public_key(seed)? != update::PUBLIC_KEY {
                return Err("the signing key is not the one the apps trust (update::PUBLIC_KEY)".into());
            }
            let text = update::sign::manifest(version, Path::new(mac), Path::new(windows))?;
            let sig = update::sign::signature(seed, text.as_bytes())?;
            // Check what we are about to publish the way the apps will.
            update::verify_manifest(text.as_bytes(), &sig, update::PUBLIC_KEY)?;
            let out = Path::new(out);
            std::fs::write(out.join("latest.txt"), &text).map_err(|e| e.to_string())?;
            std::fs::write(out.join("latest.txt.sig"), &sig).map_err(|e| e.to_string())?;
            println!("{text}");
            Ok(())
        }
        _ => Err("usage: dwsign keygen | dwsign release VERSION MAC.zip WINDOWS.zip OUT_DIR".into()),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
