//! Streamy wire format. All integers are little-endian.

pub mod probe;
pub mod stream;

/// First two bytes of every datagram.
pub const MAGIC: [u8; 2] = *b"SY";

/// Largest datagram we send. Fits Tailscale/WireGuard's 1280-byte MTU after
/// IPv6 (40) + UDP (8) headers, so nothing ever fragments.
pub const MAX_DATAGRAM: usize = 1200;

/// What encryption adds to a datagram (see the `secure` crate).
pub const SECURE_OVERHEAD: usize = 31;

/// Largest message before encryption: what fits in a datagram once it is sealed.
pub const MAX_PLAIN: usize = MAX_DATAGRAM - SECURE_OVERHEAD;

pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.buf[self.pos..self.pos + b.len()].copy_from_slice(b);
        self.pos += b.len();
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    pub(crate) fn len(&self) -> usize {
        self.pos
    }
}

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.buf.get(self.pos..self.pos + N)?.try_into().ok()?;
        self.pos += N;
        Some(b)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let rest = &self.buf[self.pos..];
        self.pos = self.buf.len();
        rest
    }

    pub(crate) fn magic(&mut self) -> Option<()> {
        (self.take::<2>()? == MAGIC).then_some(())
    }
}
