//! The connection to the server: the socket, the pairing key, and the current
//! encrypted session. Shared by the receiver thread and the input sender.

use std::io;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};

use secure::{Key, Session};

pub struct Link {
    sock: UdpSocket,
    key: Key,
    session: Mutex<Option<Arc<Session>>>,
}

impl Link {
    /// `sock` must be connected to the server.
    pub fn new(sock: UdpSocket, key: Key) -> Link {
        Link { sock, key, session: Mutex::new(None) }
    }

    pub fn key(&self) -> &Key {
        &self.key
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().unwrap().clone()
    }

    pub fn set_session(&self, session: Option<Arc<Session>>) {
        *self.session.lock().unwrap() = session;
    }

    /// Encrypts and sends a message. Before the handshake completes there is
    /// no session and it is dropped (input and pings resume by themselves).
    pub fn send(&self, plain: &[u8]) {
        if let Some(session) = self.session() {
            let mut out = [0u8; proto::MAX_DATAGRAM];
            let n = session.seal(plain, &mut out);
            let _ = self.sock.send(&out[..n]);
        }
    }

    /// Sends a datagram as is (handshake messages).
    pub fn send_raw(&self, datagram: &[u8]) {
        let _ = self.sock.send(datagram);
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.sock.recv(buf)
    }

    pub fn set_read_timeout(&self, t: std::time::Duration) -> io::Result<()> {
        self.sock.set_read_timeout(Some(t))
    }
}
