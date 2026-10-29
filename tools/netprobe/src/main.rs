//! netprobe: streams paced fake video frames over UDP and reports how they
//! arrive (loss, burst loss, jitter, frame delivery, RTT), to size FEC,
//! pacing and bitrate before the real pipeline exists.
//!
//!   Windows host:  netprobe server
//!   Mac client:    netprobe client <windows-ip> --mbps 80 --fps 120

use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use clap::{Parser, Subcommand};
use proto::probe::{DATA_HEADER_LEN, Data, Done, Hello, Msg};
use timing::{Samples, Summary, ms, now_ns, sleep_until};
use transport::{is_transient, udp_socket, wlan};

const DEFAULT_PORT: u16 = 9000;
const SEC: u64 = 1_000_000_000;
const PING_INTERVAL_NS: u64 = 10_000_000;

#[derive(Parser)]
#[command(about = "Measure how a paced fake video stream survives the network")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run on the streaming host: waits for clients and streams to them.
    Server {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Leave Windows Wi-Fi background scanning and power settings alone.
        #[arg(long)]
        no_wifi_tuning: bool,
    },
    /// Run on the viewing machine: requests a stream and reports what arrives.
    Client {
        /// Server address, `host` or `host:port`.
        server: String,
        #[arg(long, default_value_t = 80.0)]
        mbps: f64,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u16).range(1..=1000))]
        fps: u16,
        /// Fraction of the frame interval each frame's packets are spread over (0 = burst).
        #[arg(long, default_value_t = 0.5)]
        pacing: f64,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u16).range(1..))]
        secs: u16,
        /// Datagram size in bytes.
        #[arg(long, default_value_t = proto::MAX_DATAGRAM as u16)]
        packet: u16,
    },
}

fn main() -> io::Result<()> {
    now_ns();
    match Cli::parse().cmd {
        Cmd::Server { port, no_wifi_tuning } => server(port, !no_wifi_tuning),
        Cmd::Client { server, mbps, fps, pacing, secs, packet } => client(
            &server,
            Hello {
                kbps: (mbps.max(0.001) * 1000.0) as u32,
                fps,
                pacing_permille: (pacing.clamp(0.0, 1.0) * 1000.0) as u16,
                packet_len: packet.clamp(DATA_HEADER_LEN as u16, proto::MAX_DATAGRAM as u16),
                secs,
            },
        ),
    }
}

// ---------------------------------------------------------------- server

fn server(port: u16, wifi_tuning: bool) -> io::Result<()> {
    let _tuning = wifi_tuning.then(wlan::tune).flatten();
    let sock = udp_socket(SocketAddr::from(([0, 0, 0, 0], port)))?;
    println!("netprobe server on udp/{port}, waiting for a client");
    let mut buf = [0u8; 2048];
    loop {
        sock.set_read_timeout(None)?;
        let (n, peer) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if is_transient(&e) => continue,
            Err(e) => return Err(e),
        };
        let Some(Msg::Hello(h)) = Msg::decode(&buf[..n]) else { continue };
        println!(
            "{peer}: {:.1} Mbps, {} fps, pacing {:.2}, {} B packets, {} s",
            h.kbps as f64 / 1000.0,
            h.fps,
            h.pacing_permille as f64 / 1000.0,
            h.packet_len,
            h.secs
        );
        let d = session(&sock, peer, h)?;
        println!(
            "{peer}: done, sent {} packets, {} send errors, {} late frames, max start lateness {:.2} ms",
            d.packets_sent,
            d.send_errors,
            d.late_frames,
            ms(d.max_late_ns as i64)
        );
    }
}

fn session(sock: &UdpSocket, peer: SocketAddr, h: Hello) -> io::Result<Done> {
    let stop = Arc::new(AtomicBool::new(false));
    let ctl = {
        let sock = sock.try_clone()?;
        let stop = stop.clone();
        std::thread::spawn(move || answer_pings(&sock, peer, &stop))
    };
    let done = stream(sock, peer, h, &stop);
    stop.store(true, Relaxed);
    let _ = ctl.join();

    let mut buf = [0u8; 64];
    let n = Msg::Done(done).encode(&mut buf);
    for _ in 0..5 {
        let _ = sock.send_to(&buf[..n], peer);
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(done)
}

fn answer_pings(sock: &UdpSocket, peer: SocketAddr, stop: &AtomicBool) {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
    let mut buf = [0u8; 2048];
    while !stop.load(Relaxed) {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        if from != peer {
            continue;
        }
        match Msg::decode(&buf[..n]) {
            Some(Msg::Ping { id, t_ns }) => {
                let n = Msg::Pong { id, t_ns }.encode(&mut buf);
                let _ = sock.send_to(&buf[..n], peer);
            }
            Some(Msg::Bye) => stop.store(true, Relaxed),
            _ => {}
        }
    }
}

fn stream(sock: &UdpSocket, peer: SocketAddr, h: Hello, stop: &AtomicBool) -> Done {
    let fps = h.fps as u64;
    let interval_ns = SEC / fps;
    let len = h.packet_len as usize;
    let frame_bytes = h.kbps as u64 * 1000 / 8 / fps;
    let count = frame_bytes.div_ceil(len as u64).clamp(1, u16::MAX as u64) as u16;
    let spread_ns = interval_ns * h.pacing_permille as u64 / 1000;
    let frames = h.secs as u64 * fps;

    let mut pkt = vec![0u8; len];
    let mut done = Done { packets_sent: 0, send_errors: 0, late_frames: 0, max_late_ns: 0 };
    let mut seq = 0u32;
    let start = now_ns() + 10_000_000;
    for frame in 0..frames {
        if stop.load(Relaxed) {
            break;
        }
        let t0 = start + frame * SEC / fps;
        sleep_until(t0);
        let late = now_ns() - t0;
        done.max_late_ns = done.max_late_ns.max(late);
        if late > interval_ns / 2 {
            done.late_frames += 1;
        }
        for idx in 0..count {
            if idx > 0 && spread_ns > 0 {
                sleep_until(t0 + spread_ns * idx as u64 / count as u64);
            }
            Msg::Data(Data { seq, frame: frame as u32, idx, count, send_ns: now_ns() }).encode(&mut pkt);
            match sock.send_to(&pkt, peer) {
                Ok(_) => done.packets_sent += 1,
                Err(_) => done.send_errors += 1,
            }
            seq = seq.wrapping_add(1);
        }
    }
    done
}

// ---------------------------------------------------------------- client

fn client(server: &str, hello: Hello) -> io::Result<()> {
    let addr = transport::resolve(server, DEFAULT_PORT)?;
    let sock = udp_socket(transport::any_local(addr))?;
    sock.connect(addr)?;
    sock.set_read_timeout(Some(Duration::from_millis(2)))?;
    println!("requesting stream from {addr}");

    let mut rx = [0u8; 2048];
    let mut tx = [0u8; 64];
    let send = |m: Msg, tx: &mut [u8]| {
        let n = m.encode(tx);
        let _ = sock.send(&tx[..n]);
    };

    let mut stats = Stats::default();
    let begin = now_ns();
    let mut next_hello = begin;
    let mut next_ping = begin;
    let mut ping_id = 0u32;
    let mut next_report = u64::MAX;
    let mut last_rx = begin;
    let mut server_done = None;
    loop {
        let now = now_ns();
        if stats.received == 0 {
            if now - begin > 5 * SEC {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("no reply from {addr}: is `netprobe server` running and udp/{} allowed through the firewall?", addr.port()),
                ));
            }
            if now >= next_hello {
                send(Msg::Hello(hello), &mut tx);
                next_hello = now + SEC / 5;
            }
        } else if now - last_rx > 2 * SEC {
            println!("stream stalled for 2 s, stopping");
            break;
        }
        if now >= next_ping {
            send(Msg::Ping { id: ping_id, t_ns: now }, &mut tx);
            ping_id += 1;
            next_ping = now + PING_INTERVAL_NS;
        }
        if now >= next_report {
            stats.report_window((now - stats.first_rx) / SEC);
            next_report += SEC;
        }

        let n = match sock.recv(&mut rx) {
            Ok(n) => n,
            Err(e) if is_transient(&e) => continue,
            Err(e) => return Err(e),
        };
        let rx_ns = now_ns();
        match Msg::decode(&rx[..n]) {
            Some(Msg::Data(d)) => {
                if stats.received == 0 {
                    next_report = rx_ns + SEC;
                }
                stats.on_data(d, n, rx_ns);
                last_rx = rx_ns;
            }
            Some(Msg::Pong { t_ns, .. }) => stats.on_rtt(rx_ns as i64 - t_ns as i64),
            Some(Msg::Done(d)) => {
                server_done = Some(d);
                break;
            }
            _ => {}
        }
    }
    for _ in 0..3 {
        send(Msg::Bye, &mut tx);
    }
    stats.finish();
    stats.print_summary(&hello, server_done);
    Ok(())
}

struct FrameAcc {
    id: u32,
    count: u16,
    got: u16,
    /// Worst one-way delay among the frame's packets: when the frame completes.
    max_owd: i64,
}

/// Per-second slice of the stats, printed live.
#[derive(Default)]
struct Window {
    bytes: u64,
    packets: u64,
    gaps: u64,
    reordered: u64,
    max_burst: u32,
    owd: Samples,
    frames: u64,
    incomplete: u64,
    rtt: Samples,
}

#[derive(Default)]
struct Stats {
    received: u64,
    bytes: u64,
    first_rx: u64,
    last_rx: u64,
    first_seq: Option<u32>,
    highest_seq: u32,
    reordered: u64,
    /// Packets for frames that had already been closed.
    late_pkts: u64,
    /// Length of every run of missing sequence numbers.
    bursts: Vec<u32>,
    /// One-way delay per packet (rx - send). Contains the unknown clock
    /// offset between machines, so only differences from the minimum mean anything.
    owd: Samples,
    owd_min: Option<i64>,
    /// Worst packet delay per complete frame (same offset caveat).
    frame_owd: Samples,
    /// (packets in frame, packets received) per frame.
    frames: Vec<(u16, u16)>,
    cur: Option<FrameAcc>,
    rtt: Samples,
    win: Window,
}

impl Stats {
    fn on_data(&mut self, d: Data, len: usize, rx: u64) {
        if self.received == 0 {
            self.first_rx = rx;
        }
        self.received += 1;
        self.bytes += len as u64;
        self.last_rx = rx;
        self.win.packets += 1;
        self.win.bytes += len as u64;

        if self.first_seq.is_none() {
            self.first_seq = Some(d.seq);
            self.highest_seq = d.seq;
        } else if d.seq > self.highest_seq {
            let gap = d.seq - self.highest_seq - 1;
            if gap > 0 {
                self.bursts.push(gap);
                self.win.gaps += gap as u64;
                self.win.max_burst = self.win.max_burst.max(gap);
            }
            self.highest_seq = d.seq;
        } else {
            self.reordered += 1;
            self.win.reordered += 1;
        }

        let owd = rx as i64 - d.send_ns as i64;
        self.owd.push(owd);
        self.win.owd.push(owd);
        self.owd_min = Some(self.owd_min.map_or(owd, |m| m.min(owd)));

        match self.cur.as_ref().map(|f| f.id) {
            Some(id) if id == d.frame => {
                let f = self.cur.as_mut().unwrap();
                f.got += 1;
                f.max_owd = f.max_owd.max(owd);
            }
            Some(id) if d.frame < id => self.late_pkts += 1,
            prev => {
                if let Some(f) = self.cur.take() {
                    self.close_frame(f);
                }
                // Frames that vanished entirely.
                for _ in prev.map_or(d.frame, |id| id + 1)..d.frame {
                    self.push_frame(d.count, 0);
                }
                self.cur = Some(FrameAcc { id: d.frame, count: d.count, got: 1, max_owd: owd });
            }
        }
    }

    fn close_frame(&mut self, f: FrameAcc) {
        if f.got >= f.count {
            self.frame_owd.push(f.max_owd);
        }
        self.push_frame(f.count, f.got);
    }

    fn push_frame(&mut self, count: u16, got: u16) {
        self.frames.push((count, got));
        self.win.frames += 1;
        if got < count {
            self.win.incomplete += 1;
        }
    }

    fn on_rtt(&mut self, rtt: i64) {
        self.rtt.push(rtt);
        self.win.rtt.push(rtt);
    }

    fn finish(&mut self) {
        if let Some(f) = self.cur.take() {
            self.close_frame(f);
        }
    }

    fn report_window(&mut self, t: u64) {
        let w = std::mem::take(&mut self.win);
        let (mut owd, mut rtt) = (w.owd, w.rtt);
        let base = self.owd_min.unwrap_or(0);
        let j = owd.summary();
        let r = rtt.summary();
        let lost = w.gaps.saturating_sub(w.reordered);
        println!(
            "{t:>4}s  {:>6.1} Mbps  loss {:>5.2}% (max burst {:>2})  frames {:>4} ({} incomplete)  jitter p50 {:.2} p99 {:.2} max {:.2} ms  rtt p50 {:.2} p99 {:.2} ms",
            w.bytes as f64 * 8.0 / 1e6,
            pct(lost, w.packets + lost),
            w.max_burst,
            w.frames,
            w.incomplete,
            ms(j.p50 - base),
            ms(j.p99 - base),
            ms(j.max - base),
            ms(r.p50),
            ms(r.p99),
        );
    }

    fn print_summary(&mut self, h: &Hello, server: Option<Done>) {
        let Some(first_seq) = self.first_seq else {
            println!("no data received");
            return;
        };
        let expected = match server {
            Some(d) => d.packets_sent + d.send_errors as u64,
            None => self.highest_seq.wrapping_sub(first_seq) as u64 + 1,
        };
        let lost = expected.saturating_sub(self.received);
        let secs = (self.last_rx - self.first_rx).max(1) as f64 / 1e9;

        println!();
        println!(
            "== {:.1} Mbps, {} fps, pacing {:.2}, {} B packets, {} s ==",
            h.kbps as f64 / 1000.0,
            h.fps,
            h.pacing_permille as f64 / 1000.0,
            h.packet_len,
            h.secs
        );
        println!(
            "throughput  {:.1} Mbps received",
            self.bytes as f64 * 8.0 / 1e6 / secs
        );
        println!(
            "packets     {} / {} received, loss {:.3}%, reordered {}, too late for their frame {}",
            self.received,
            expected,
            pct(lost, expected),
            self.reordered,
            self.late_pkts
        );
        let burst_hist = [(1, 1), (2, 2), (3, 5), (6, 20), (21, u32::MAX)]
            .iter()
            .map(|&(lo, hi)| {
                let n = self.bursts.iter().filter(|&&b| b >= lo && b <= hi).count();
                let label = if hi == u32::MAX { format!("{lo}+") } else if lo == hi { lo.to_string() } else { format!("{lo}-{hi}") };
                format!("{label}:{n}")
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!(
            "loss bursts {} (max {})  by length  {burst_hist}",
            self.bursts.len(),
            self.bursts.iter().max().unwrap_or(&0)
        );
        let base = self.owd_min.unwrap_or(0);
        print_dist("packet delay vs best", self.owd.summary(), base);
        print_dist("frame complete vs best", self.frame_owd.summary(), base);
        print_dist("rtt", self.rtt.summary(), 0);

        let incomplete_at = |parity: f64| {
            let bad = self
                .frames
                .iter()
                .filter(|&&(count, got)| {
                    let lost = count.saturating_sub(got) as f64;
                    lost > (count as f64 * parity).ceil()
                })
                .count();
            pct(bad as u64, self.frames.len() as u64)
        };
        println!(
            "frames      {}  unrecoverable: no FEC {:.3}%  FEC 10% {:.3}%  20% {:.3}%  30% {:.3}%",
            self.frames.len(),
            incomplete_at(0.0),
            incomplete_at(0.10),
            incomplete_at(0.20),
            incomplete_at(0.30)
        );
        match server {
            Some(d) => println!(
                "server      {} send errors, {} late frames, max frame start lateness {:.2} ms",
                d.send_errors,
                d.late_frames,
                ms(d.max_late_ns as i64)
            ),
            None => println!("server      no end-of-stream report (lost or stream stalled)"),
        }
    }
}

fn print_dist(label: &str, s: Summary, base: i64) {
    if s.count == 0 {
        return;
    }
    println!(
        "{label:<22} p50 {:.2}  p99 {:.2}  p99.9 {:.2}  max {:.2} ms",
        ms(s.p50 - base),
        ms(s.p99 - base),
        ms(s.p999 - base),
        ms(s.max - base)
    );
}

fn pct(n: u64, d: u64) -> f64 {
    if d == 0 { 0.0 } else { n as f64 * 100.0 / d as f64 }
}
