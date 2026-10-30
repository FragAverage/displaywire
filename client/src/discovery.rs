//! Finds displaywire servers on the local network: broadcasts a query (see
//! `proto::discover`) and collects the answers.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use proto::discover;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The computer's name, as it announces itself.
    pub name: String,
    /// Address to connect to: its IP, with `:port` if not the default.
    pub address: String,
}

/// Asks for `wait`, returns who answered.
pub fn browse(wait: Duration) -> Vec<Found> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return Vec::new() };
    let _ = sock.set_broadcast(true);
    let mut nonce = [0u8; 8];
    for (i, b) in nonce.iter_mut().enumerate() {
        *b = (timing::now_ns() >> (i * 3)) as u8 ^ std::process::id() as u8;
    }
    // Every IPv4 interface's own broadcast address, plus the global one.
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if !i.is_loopback()
                && let if_addrs::IfAddr::V4(v4) = i.addr
                && let Some(b) = v4.broadcast
            {
                targets.push(b);
            }
        }
    }
    targets.sort();
    targets.dedup();
    let query = discover::query(nonce);
    let deadline = Instant::now() + wait;
    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 256];
    let mut sent = 0;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        // Ask again a couple of times: one datagram can get lost on Wi-Fi.
        if sent < 3 && left < wait.mul_f32(1.0 - sent as f32 * 0.3) {
            for t in &targets {
                let _ = sock.send_to(&query, SocketAddr::new(IpAddr::V4(*t), 9000));
            }
            sent += 1;
        }
        let _ = sock.set_read_timeout(Some(left.min(Duration::from_millis(100))));
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let Some((got, a)) = discover::parse_reply(&buf[..n]) else { continue };
        if got != nonce {
            continue;
        }
        let address = if a.port == 9000 { from.ip().to_string() } else { format!("{}:{}", from.ip(), a.port) };
        if !found.iter().any(|f| f.address == address) {
            found.push(Found { name: a.name, address });
        }
    }
    found
}
