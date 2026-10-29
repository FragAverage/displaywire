//! decodebench: VideoToolbox hardware decode latency for HEVC (Annex B with
//! AUDs, `.h265`) and AV1 (`.ivf`) streams.
//!
//!   scripts/make-clips.sh
//!   cargo run --release -p decodebench -- bench/clips/*

#[cfg(target_os = "macos")]
mod av1;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("decodebench only runs on macOS");
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    use clap::Parser;
    let cli = mac::Cli::parse();
    timing::now_ns();
    for path in &cli.files {
        mac::bench(path, &cli)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering::Relaxed};

    use clap::Parser;
    use timing::{Samples, ms, now_ns, sleep_until};

    use videotoolbox::{self as vt, Format, Output, Session};

    use crate::av1;

    const SEC: u64 = 1_000_000_000;

    #[derive(Parser)]
    #[command(about = "Measure VideoToolbox hardware decode latency")]
    pub struct Cli {
        /// `.h265` (Annex B with access unit delimiters) or `.ivf` (AV1) files.
        #[arg(required = true)]
        pub files: Vec<PathBuf>,
        /// Frame rate for the paced run.
        #[arg(long, default_value_t = 120)]
        pub fps: u64,
        /// Leading frames left out of the statistics.
        #[arg(long, default_value_t = 10)]
        pub warmup: usize,
    }

    struct Clip {
        codec: u32,
        format: Format,
        frames: Vec<Vec<u8>>,
    }

    fn load(path: &Path) -> Result<Clip, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if path.extension().is_some_and(|e| e == "ivf") {
            let s = av1::parse(&data)?;
            Ok(Clip { codec: vt::CODEC_AV1, format: Format::av1(s.width, s.height, &s.av1c)?, frames: s.frames })
        } else {
            let samples: Vec<_> = hevc::access_units(&data).into_iter().map(hevc::to_sample).collect();
            let ps = samples.iter().find_map(|s| s.param_sets.clone()).ok_or("stream has no VPS/SPS/PPS")?;
            let frames = samples.into_iter().map(|s| s.data).filter(|d| !d.is_empty()).collect::<Vec<_>>();
            if frames.is_empty() {
                return Err("no frames (stream needs access unit delimiters)".into());
            }
            Ok(Clip { codec: vt::CODEC_HEVC, format: Format::hevc(&ps)?, frames })
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// One frame at a time, each submitted as soon as the last finished.
        Burst,
        /// One frame at a time at the stream's frame rate, like live streaming.
        Paced,
        /// Asynchronous submission to find maximum throughput.
        Throughput,
    }

    pub fn bench(path: &Path, cli: &Cli) -> Result<(), String> {
        let clip = load(path)?;
        let bytes: usize = clip.frames.iter().map(Vec::len).sum();
        let mbps = bytes as f64 * 8.0 * cli.fps as f64 / clip.frames.len() as f64 / 1e6;
        println!(
            "{}: {} frames, {mbps:.0} Mbps at {} fps, hardware decoder: {}",
            path.display(),
            clip.frames.len(),
            cli.fps,
            if vt::hardware_decode_supported(clip.codec) { "yes" } else { "NO" }
        );
        for mode in [Mode::Burst, Mode::Paced, Mode::Throughput] {
            run(&clip, mode, cli)?;
        }
        println!();
        Ok(())
    }

    fn run(clip: &Clip, mode: Mode, cli: &Cli) -> Result<(), String> {
        let n = clip.frames.len();
        let done: Arc<Vec<AtomicU64>> = Arc::new((0..n).map(|_| AtomicU64::new(0)).collect());
        let errors = Arc::new((AtomicU32::new(0), AtomicI32::new(0)));
        let pixel_format = Arc::new(std::sync::OnceLock::new());
        let session = {
            let (done, errors, pixel_format) = (done.clone(), errors.clone(), pixel_format.clone());
            Session::new(
                clip.format.clone(),
                None,
                Box::new(move |out: Output| {
                    let now = now_ns();
                    match out.image {
                        Some(img) if out.status == 0 => {
                            done[out.index].store(now, Relaxed);
                            pixel_format.get_or_init(|| img.pixel_format());
                        }
                        _ => {
                            errors.0.fetch_add(1, Relaxed);
                            errors.1.store(out.status, Relaxed);
                        }
                    }
                }),
            )?
        };
        let mut submit = vec![0u64; n];
        let start = now_ns() + 10_000_000;
        for (i, frame) in clip.frames.iter().enumerate() {
            if mode == Mode::Paced {
                sleep_until(start + i as u64 * SEC / cli.fps);
            }
            submit[i] = now_ns();
            session.decode(i, frame, mode == Mode::Throughput)?;
        }
        session.finish();

        let mut latency = Samples::default();
        let mut last_done = 0;
        for i in cli.warmup.min(n)..n {
            let done = done[i].load(Relaxed);
            if done > 0 {
                latency.push((done - submit[i]) as i64);
                last_done = last_done.max(done);
            }
        }
        let s = latency.summary();
        match mode {
            Mode::Throughput => {
                let first = submit[cli.warmup.min(n - 1)];
                let fps = s.count as f64 * 1e9 / (last_done.saturating_sub(first)).max(1) as f64;
                println!("  throughput         {fps:.0} fps max   output {}", pixel_format.get().map_or("?", |s| s.as_str()));
            }
            _ => println!(
                "  {:<18} p50 {:.2}  p99 {:.2}  max {:.2} ms",
                if mode == Mode::Burst { "latency, burst" } else { "latency, paced" },
                ms(s.p50),
                ms(s.p99),
                ms(s.max)
            ),
        }
        drop(session);
        let (errors, last) = (errors.0.load(Relaxed), errors.1.load(Relaxed));
        if errors > 0 || s.count < n - cli.warmup.min(n) {
            println!("    {errors} decode errors (last OSStatus {last}), {} frames decoded", s.count);
        }
        Ok(())
    }
}
