//! UDP plumbing shared by the server, client and tools.

pub mod transfer;
pub mod video;
pub mod wlan;

use std::io::{self, ErrorKind};
use std::net::{Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};

/// UDP socket with large buffers and DSCP EF (Wi-Fi WMM voice queue, the
/// shortest contention window).
pub fn udp_socket(bind: SocketAddr) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    let _ = s.set_recv_buffer_size(8 << 20);
    let _ = s.set_send_buffer_size(8 << 20);
    if bind.is_ipv4() {
        let _ = s.set_tos_v4(0xB8);
    }
    s.bind(&bind.into())?;
    Ok(s.into())
}

/// Read errors that just mean "nothing useful arrived".
pub fn is_transient(e: &io::Error) -> bool {
    // ConnectionReset/Refused: ICMP port-unreachable from a peer that went away
    // (Windows) or isn't up yet (macOS).
    matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused
    )
}

/// Resolves `host`, `host:port`, `ipv6` or `[ipv6]:port`.
pub fn resolve(server: &str, default_port: u16) -> io::Result<SocketAddr> {
    let with_port = if server.parse::<SocketAddr>().is_ok() || server.rsplit_once(':').is_some_and(|(h, _)| !h.contains(':')) {
        server.to_string()
    } else if server.contains(':') {
        format!("[{server}]:{default_port}")
    } else {
        format!("{server}:{default_port}")
    };
    with_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, format!("cannot resolve {server}")))
}

/// Unspecified local address of the same family as `peer`, port 0.
pub fn any_local(peer: SocketAddr) -> SocketAddr {
    if peer.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() }
}
