//! Pairs with a server from the command line, to test PIN pairing without the Mac app.
//!
//!     pairprobe <server:port> --name "Test device" [--pin 123456]
//!
//! Prints where pairing stands; asks for the PIN on stdin unless `--pin` is given, and
//! prints the key it receives.

use std::io::Write;
use std::time::Duration;

use clap::Parser;
use pairing::{Pairing, Progress};

#[derive(Parser)]
struct Cli {
    /// Server address, with the port (e.g. 192.168.1.50:9000).
    server: std::net::SocketAddr,
    #[arg(long, default_value = "pairprobe")]
    name: String,
    /// The PIN, if you already know it (otherwise it is asked for when the PC has shown one).
    #[arg(long)]
    pin: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    let pairing = Pairing::start(cli.server, &cli.name).expect("start pairing");
    let mut shown = None;
    loop {
        let p = pairing.progress();
        if shown.as_ref() != Some(&p) {
            println!("{p:?}");
            shown = Some(p.clone());
            match p {
                Progress::NeedPin => {
                    let pin = cli.pin.clone().unwrap_or_else(|| {
                        print!("PIN shown on the PC: ");
                        let _ = std::io::stdout().flush();
                        let mut line = String::new();
                        let _ = std::io::stdin().read_line(&mut line);
                        line.trim().to_string()
                    });
                    pairing.submit_pin(&pin);
                }
                Progress::WrongPin { .. } => pairing.retry(),
                Progress::Done { key, server_name } => {
                    println!("paired with {server_name:?}; this device's key: {key}");
                    return;
                }
                p if p.finished() => std::process::exit(1),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
