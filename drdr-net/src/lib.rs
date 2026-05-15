//! drdr-net — DrDrNet, the DrDrOS binary network protocol (Tier 1).
//!
//! Wire format
//! ───────────
//! Every message is a single self-describing **frame**:
//!
//! ```text
//!   ┌──────────┬──────┬──────────┬────────────────────────────┐
//!   │  len:u32 │ kind │  id:u32  │     payload (len bytes)    │
//!   │   (BE)   │  u8  │   (BE)   │          any bytes         │
//!   └──────────┴──────┴──────────┴────────────────────────────┘
//!     4 bytes   1 byte  4 bytes            variable
//! ```
//!
//! - `len` is the byte count of the payload only (it does NOT include
//!   itself, `kind`, or `id`). Big-endian so a `tcpdump` / `xxd` dump
//!   reads naturally left-to-right.
//! - `kind` is a one-byte tag the application layer assigns meaning to
//!   (request? response? heartbeat?). DrDrNet itself doesn't interpret it.
//! - `id` is a **correlation ID** (Tier 2). When a client fires several
//!   requests down one socket, replies can come back in any order; the
//!   server echoes the request's `id` into its reply so the client knows
//!   which request a given reply answers — the same role JSON-RPC's
//!   `id` field plays. The reserved value `id == 0` means "unsolicited":
//!   a server push, heartbeat, or fire-and-forget notification that
//!   expects no reply. See [`NO_CORRELATION`].
//! - `payload` is arbitrary bytes — typically built with the [`Encoder`]
//!   helpers in this module.
//!
//! `len` is capped at [`MAX_PAYLOAD_LEN`] (16 MiB) so a misbehaving or
//! hostile peer can't tie up the reader with a giant `read_exact` call.
//! Bigger transfers should be chunked into multiple frames.
//!
//! Encoding helpers
//! ────────────────
//! [`Encoder`] writes primitive types into a `Vec<u8>` in big-endian.
//! [`Decoder`] reads them back out of a `&[u8]` with bounds checks that
//! return a descriptive [`DecodeError`] on short / malformed input.
//!
//! Tier 2 (this module) adds the `id` correlation field above, a
//! [`Codec`] trait that types implement, request/reply helpers on
//! [`Conn`], and a real TCP transport in the [`tcp`] submodule
//! (`std::net`, thread-per-connection — no async runtime yet). Tier 3
//! will layer an async runtime on top for fully concurrent,
//! out-of-order request multiplexing.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

/// Cap on `payload` byte length per frame (16 MiB). Larger transfers
/// belong in multiple frames so the reader can never be tricked into
/// allocating an unbounded buffer.
pub const MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;

/// Reserved correlation ID meaning "this frame is unsolicited" — a
/// server push, heartbeat, or fire-and-forget notification that expects
/// no reply. [`Conn::send_typed`] uses this; [`Conn::request`] never
/// hands it out.
pub const NO_CORRELATION: u32 = 0;

/// One on-the-wire message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Application-defined tag — DrDrNet itself attaches no meaning.
    pub kind: u8,
    /// Correlation ID. `0` ([`NO_CORRELATION`]) = unsolicited; a nonzero
    /// value ties a reply back to the request that carried the same id.
    pub id: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    /// An unsolicited frame (`id == 0`). Use this for notifications and
    /// heartbeats — anything the peer is not expected to reply to.
    pub fn new(kind: u8, payload: Vec<u8>) -> Self {
        Self { kind, id: NO_CORRELATION, payload }
    }

    /// A correlated frame carrying an explicit `id`. A client builds a
    /// request with a fresh id; a server builds its reply by echoing the
    /// request's id straight back.
    pub fn with_id(kind: u8, id: u32, payload: Vec<u8>) -> Self {
        Self { kind, id, payload }
    }
}

// ─── Framing ────────────────────────────────────────────────────────

/// Write a complete frame to `w`. Errors propagate from the underlying
/// writer; a payload larger than [`MAX_PAYLOAD_LEN`] is reported as
/// `InvalidInput` rather than truncated silently.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> io::Result<()> {
    if frame.payload.len() > MAX_PAYLOAD_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("payload {} bytes exceeds MAX_PAYLOAD_LEN ({} bytes)", frame.payload.len(), MAX_PAYLOAD_LEN),
        ));
    }
    let len = frame.payload.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&[frame.kind])?;
    w.write_all(&frame.id.to_be_bytes())?;
    w.write_all(&frame.payload)?;
    Ok(())
}

/// Read one full frame from `r`. Blocks until either the entire frame
/// has arrived or the underlying reader returns an error. Reports a
/// `InvalidData` error if the announced length is larger than
/// [`MAX_PAYLOAD_LEN`].
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_PAYLOAD_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("declared payload {len} bytes exceeds MAX_PAYLOAD_LEN"),
        ));
    }
    let mut kind_buf = [0u8; 1];
    r.read_exact(&mut kind_buf)?;
    let mut id_buf = [0u8; 4];
    r.read_exact(&mut id_buf)?;
    let id = u32::from_be_bytes(id_buf);
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Frame { kind: kind_buf[0], id, payload })
}

// ─── Encoder ────────────────────────────────────────────────────────

/// Append-only byte builder for frame payloads. Every multi-byte type
/// goes out big-endian. Strings are length-prefixed (u32 BE) so the
/// reader can read them without seeking past the end.
#[derive(Debug, Default, Clone)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: Vec::with_capacity(cap) }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn write_u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn write_u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn write_u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn write_u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn write_i32(&mut self, v: i32) -> &mut Self {
        self.write_u32(v as u32)
    }

    pub fn write_i64(&mut self, v: i64) -> &mut Self {
        self.write_u64(v as u64)
    }

    /// Length-prefixed (u32 BE) blob. Use this for any variable-size
    /// field so the reader knows where it ends.
    pub fn write_bytes(&mut self, b: &[u8]) -> &mut Self {
        self.write_u32(b.len() as u32);
        self.buf.extend_from_slice(b);
        self
    }

    /// UTF-8 string, encoded the same way as [`write_bytes`].
    pub fn write_str(&mut self, s: &str) -> &mut Self {
        self.write_bytes(s.as_bytes())
    }
}

// ─── Decoder ────────────────────────────────────────────────────────

/// Why decoding failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Reader hit the end of the slice mid-field.
    UnexpectedEof,
    /// A length-prefix announced more bytes than the slice has left.
    LengthMismatch { declared: u32, available: usize },
    /// UTF-8 validation failed on a `read_str` call.
    InvalidUtf8,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnexpectedEof => write!(f, "unexpected EOF"),
            DecodeError::LengthMismatch { declared, available } => {
                write!(f, "field declares {declared} bytes but {available} are left")
            }
            DecodeError::InvalidUtf8 => write!(f, "invalid UTF-8 in string field"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Cursor-based reader for the format [`Encoder`] writes. Borrows from
/// the source slice — strings come back as `&str`, blobs as `&[u8]`.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::UnexpectedEof);
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn read_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub fn read_u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn read_u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_i32(&mut self) -> Result<i32, DecodeError> {
        self.read_u32().map(|v| v as i32)
    }

    pub fn read_i64(&mut self) -> Result<i64, DecodeError> {
        self.read_u64().map(|v| v as i64)
    }

    pub fn read_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.read_u32()?;
        if (len as usize) > self.remaining() {
            return Err(DecodeError::LengthMismatch { declared: len, available: self.remaining() });
        }
        self.take(len as usize)
    }

    pub fn read_str(&mut self) -> Result<&'a str, DecodeError> {
        let bytes = self.read_bytes()?;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)
    }
}

// ─── Codec trait + primitive impls ───────────────────────────────────

/// Self-describing wire encoding for a Rust type.
///
/// Implementors decide how their fields map to the byte stream — the
/// trait only requires that the encoding round-trips through [`Encoder`]
/// and [`Decoder`]. There's no `#[derive(Codec)]` proc macro yet; types
/// hand-roll the two methods (it's usually 4-6 lines).
pub trait Codec: Sized {
    fn encode(&self, enc: &mut Encoder);
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError>;
}

impl Codec for u8 {
    fn encode(&self, enc: &mut Encoder) { enc.write_u8(*self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> { dec.read_u8() }
}
impl Codec for u16 {
    fn encode(&self, enc: &mut Encoder) { enc.write_u16(*self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> { dec.read_u16() }
}
impl Codec for u32 {
    fn encode(&self, enc: &mut Encoder) { enc.write_u32(*self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> { dec.read_u32() }
}
impl Codec for u64 {
    fn encode(&self, enc: &mut Encoder) { enc.write_u64(*self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> { dec.read_u64() }
}
impl Codec for i32 {
    fn encode(&self, enc: &mut Encoder) { enc.write_i32(*self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> { dec.read_i32() }
}
impl Codec for i64 {
    fn encode(&self, enc: &mut Encoder) { enc.write_i64(*self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> { dec.read_i64() }
}
impl Codec for bool {
    fn encode(&self, enc: &mut Encoder) { enc.write_u8(if *self { 1 } else { 0 }); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(dec.read_u8()? != 0)
    }
}
impl Codec for String {
    fn encode(&self, enc: &mut Encoder) { enc.write_str(self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        dec.read_str().map(|s| s.to_owned())
    }
}
impl Codec for Vec<u8> {
    fn encode(&self, enc: &mut Encoder) { enc.write_bytes(self); }
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        dec.read_bytes().map(|b| b.to_vec())
    }
}

/// Pack a `Codec` value into a fresh `Vec<u8>`. Handy for cooking up
/// frame payloads in one expression: `Frame::new(KIND, pack(&msg))`.
pub fn pack<T: Codec>(value: &T) -> Vec<u8> {
    let mut enc = Encoder::new();
    value.encode(&mut enc);
    enc.into_bytes()
}

/// Unpack a `Codec` value from a slice, erroring if the slice has
/// trailing bytes (catches schema mismatches early).
pub fn unpack<T: Codec>(bytes: &[u8]) -> Result<T, DecodeError> {
    let mut dec = Decoder::new(bytes);
    let value = T::decode(&mut dec)?;
    if !dec.is_empty() {
        // Treat trailing bytes as a schema bug: the caller asked for one
        // value but the wire has more. UnexpectedEof is the wrong name
        // — use LengthMismatch which more accurately captures it.
        return Err(DecodeError::LengthMismatch {
            declared: 0,
            available: dec.remaining(),
        });
    }
    Ok(value)
}

// ─── Conn — typed framed stream ──────────────────────────────────────

/// Wraps any Read+Write duplex stream (TcpStream, UnixStream, an
/// in-memory pipe, …) and offers send/recv methods at three levels:
///
///   - `send_frame` / `recv_frame` — raw [`Frame`]s
///   - `send_typed` / `recv_typed` — a `kind` byte plus a [`Codec`]
///     payload, sent *unsolicited* (`id == 0`: notifications, pushes)
///   - `request` / `recv_request` + `reply` — correlated round-trips:
///     the client allocates a fresh id, the server echoes it back
///
/// `Conn` owns the underlying stream so it can flush after every send.
/// If you need to keep the stream for other purposes, hand the trait
/// object across the boundary instead of using `Conn`.
///
/// The synchronous `request` here sends one frame and blocks for the
/// matching reply; it verifies the reply's id but does not yet juggle
/// many in-flight requests at once (that demux map is Tier 3's job).
pub struct Conn<S: Read + Write> {
    stream: S,
    /// Next correlation ID this side will hand out. Starts at 1 because
    /// 0 is reserved ([`NO_CORRELATION`]); wraps back to 1, never 0.
    next_id: u32,
}

impl<S: Read + Write> Conn<S> {
    pub fn new(stream: S) -> Self {
        Self { stream, next_id: 1 }
    }

    /// Hand out the next correlation ID, skipping the reserved 0 on
    /// wrap-around. IDs are per-`Conn`; collisions only matter while a
    /// request is in flight, and 2³² ids is plenty for that window.
    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        id
    }

    /// Give back the inner stream (e.g. to drop it explicitly).
    pub fn into_inner(self) -> S {
        self.stream
    }

    pub fn send_frame(&mut self, frame: &Frame) -> io::Result<()> {
        write_frame(&mut self.stream, frame)?;
        self.stream.flush()
    }

    pub fn recv_frame(&mut self) -> io::Result<Frame> {
        read_frame(&mut self.stream)
    }

    /// Send an **unsolicited** typed message (`id == 0`) with the given
    /// `kind` byte — a notification, push, or heartbeat the peer is not
    /// expected to answer. For request/response use [`Conn::request`].
    pub fn send_typed<T: Codec>(&mut self, kind: u8, msg: &T) -> io::Result<()> {
        self.send_frame(&Frame::new(kind, pack(msg)))
    }

    /// Receive the next frame and decode its payload as `T`. Returns
    /// the frame's `kind` byte alongside so the caller can demultiplex.
    /// Schema mismatches surface as `InvalidData`.
    pub fn recv_typed<T: Codec>(&mut self) -> io::Result<(u8, T)> {
        let frame = self.recv_frame()?;
        let value = unpack::<T>(&frame.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok((frame.kind, value))
    }

    /// **Client side.** Send a request and block for its reply.
    ///
    /// Allocates a fresh correlation id, sends `req` under `req_kind`,
    /// then reads exactly one frame back. The reply must echo the same
    /// id — if it doesn't, that's a protocol violation and we return
    /// `InvalidData` rather than silently handing back a mismatched
    /// answer. Returns the reply's `kind` byte plus the decoded body so
    /// the caller can tell e.g. an OK response from an ERROR one.
    pub fn request<Req: Codec, Resp: Codec>(
        &mut self,
        req_kind: u8,
        req: &Req,
    ) -> io::Result<(u8, Resp)> {
        let id = self.alloc_id();
        self.send_frame(&Frame::with_id(req_kind, id, pack(req)))?;
        let frame = self.recv_frame()?;
        if frame.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("reply correlation id {} does not match request id {id}", frame.id),
            ));
        }
        let value = unpack::<Resp>(&frame.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok((frame.kind, value))
    }

    /// **Server side.** Receive the next request, decoding its body as
    /// `T`. The returned [`Request`] keeps the correlation `id` so the
    /// handler can route it straight into [`Conn::reply`].
    pub fn recv_request<T: Codec>(&mut self) -> io::Result<Request<T>> {
        let frame = self.recv_frame()?;
        let body = unpack::<T>(&frame.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(Request { kind: frame.kind, id: frame.id, body })
    }

    /// **Server side.** Answer a request by echoing its correlation id
    /// back so the client's [`Conn::request`] can match the reply. Pass
    /// the `id` from the [`Request`] you handled.
    pub fn reply<T: Codec>(&mut self, id: u32, reply_kind: u8, msg: &T) -> io::Result<()> {
        self.send_frame(&Frame::with_id(reply_kind, id, pack(msg)))
    }
}

/// A decoded request as seen by a server: the application `kind` tag,
/// the correlation `id` to echo in the reply, and the decoded `body`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<T> {
    pub kind: u8,
    pub id: u32,
    pub body: T,
}

// ─── tcp — std::net transport ────────────────────────────────────────

/// The real-socket transport for DrDrNet (Tier 2).
///
/// Everything above is transport-agnostic — `Conn` works over any
/// `Read + Write`. This module is the thin layer that produces a
/// `Conn<TcpStream>` from an address, for both ends of a connection.
///
/// **No async runtime.** A server here is a classic blocking accept
/// loop that spawns one OS thread per connection. That's the right
/// amount of machinery for DrDrOS's needs today; Tier 3 will introduce
/// async if we ever need thousands of idle connections on one thread.
pub mod tcp {
    use super::Conn;
    use std::io;
    use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};

    /// Connect to a DrDrNet server and wrap the socket in a [`Conn`].
    ///
    /// We set `TCP_NODELAY`, which disables *Nagle's algorithm*. Nagle
    /// makes the kernel hold back tiny writes for a few ms hoping to
    /// coalesce them into one packet — great for bulk streams, but for a
    /// request/reply protocol it adds latency to every single call. We
    /// frame our own messages and want each one on the wire immediately.
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Conn<TcpStream>> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        Ok(Conn::new(stream))
    }

    /// A bound TCP listener that hands back ready [`Conn`]s.
    ///
    /// Binding is split from serving so callers (and tests) can read the
    /// actual [`local_addr`](Listener::local_addr) before accepting —
    /// pass port `0` to let the OS pick a free port, then look it up.
    pub struct Listener {
        inner: TcpListener,
    }

    impl Listener {
        pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
            Ok(Self { inner: TcpListener::bind(addr)? })
        }

        /// The address we actually bound to (resolves port `0`).
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }

        /// Block until one client connects; return its `Conn` and peer
        /// address. Useful for simple one-shot servers and for tests.
        pub fn accept(&self) -> io::Result<(Conn<TcpStream>, SocketAddr)> {
            let (stream, peer) = self.inner.accept()?;
            stream.set_nodelay(true)?;
            Ok((Conn::new(stream), peer))
        }

        /// Blocking accept loop: for every incoming connection, spawn a
        /// thread running `handler`. Returns only if `accept()` itself
        /// errors (a dead listener) — a panicking handler just takes its
        /// own connection's thread down, not the server.
        pub fn serve<H>(&self, handler: H) -> io::Result<()>
        where
            H: Fn(Conn<TcpStream>, SocketAddr) + Send + Sync + 'static,
        {
            let handler = std::sync::Arc::new(handler);
            for stream in self.inner.incoming() {
                let stream = stream?;
                stream.set_nodelay(true)?;
                let peer = stream.peer_addr()?;
                let h = std::sync::Arc::clone(&handler);
                std::thread::spawn(move || h(Conn::new(stream), peer));
            }
            Ok(())
        }
    }

    /// Convenience: [`bind`](Listener::bind) + [`serve`](Listener::serve)
    /// in one call. Blocks forever on success.
    pub fn serve<A, H>(addr: A, handler: H) -> io::Result<()>
    where
        A: ToSocketAddrs,
        H: Fn(Conn<TcpStream>, SocketAddr) + Send + Sync + 'static,
    {
        Listener::bind(addr)?.serve(handler)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let frame = Frame::new(0x42, b"hello drdr".to_vec());
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        // 4 len + 1 kind + 4 id + 10 payload = 19.
        assert_eq!(buf.len(), 19);
        assert_eq!(&buf[..4], &10u32.to_be_bytes());
        assert_eq!(buf[4], 0x42);
        // Frame::new is unsolicited → id == 0.
        assert_eq!(&buf[5..9], &0u32.to_be_bytes());

        let mut cur = Cursor::new(buf);
        let got = read_frame(&mut cur).unwrap();
        assert_eq!(got, frame);
    }

    #[test]
    fn encoder_decoder_roundtrip() {
        let mut enc = Encoder::new();
        enc.write_u8(0xAA)
            .write_u16(0xBEEF)
            .write_u32(0xDEADBEEF)
            .write_i64(-42)
            .write_str("DrDrOS")
            .write_bytes(b"\x00\x01\x02");
        let bytes = enc.into_bytes();

        let mut dec = Decoder::new(&bytes);
        assert_eq!(dec.read_u8().unwrap(), 0xAA);
        assert_eq!(dec.read_u16().unwrap(), 0xBEEF);
        assert_eq!(dec.read_u32().unwrap(), 0xDEADBEEF);
        assert_eq!(dec.read_i64().unwrap(), -42);
        assert_eq!(dec.read_str().unwrap(), "DrDrOS");
        assert_eq!(dec.read_bytes().unwrap(), b"\x00\x01\x02");
        assert!(dec.is_empty());
    }

    #[test]
    fn decoder_short_read_is_eof() {
        let bytes = [0u8; 3];
        let mut dec = Decoder::new(&bytes);
        assert_eq!(dec.read_u32().unwrap_err(), DecodeError::UnexpectedEof);
    }

    #[test]
    fn decoder_lying_length_is_caught() {
        // u32 length = 1000, but only 4 bytes follow.
        let mut bytes = 1000u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let mut dec = Decoder::new(&bytes);
        match dec.read_bytes().unwrap_err() {
            DecodeError::LengthMismatch { declared, available } => {
                assert_eq!(declared, 1000);
                assert_eq!(available, 4);
            }
            other => panic!("expected LengthMismatch, got {other:?}"),
        }
    }

    #[test]
    fn frame_rejects_oversize_payload() {
        let huge = Frame::new(0, vec![0; MAX_PAYLOAD_LEN + 1]);
        let mut buf = Vec::new();
        let err = write_frame(&mut buf, &huge).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn typed_message_via_frame_payload() {
        // A small "join" message: u32 user_id + string username.
        let mut enc = Encoder::new();
        enc.write_u32(7).write_str("gravijet");
        let frame = Frame::new(/* kind = 1 = JOIN */ 1, enc.into_bytes());

        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();

        let mut cur = Cursor::new(buf);
        let got = read_frame(&mut cur).unwrap();
        assert_eq!(got.kind, 1);

        let mut dec = Decoder::new(&got.payload);
        assert_eq!(dec.read_u32().unwrap(), 7);
        assert_eq!(dec.read_str().unwrap(), "gravijet");
    }

    // ─── Tier 2: Codec + Conn ────────────────────────────────────────

    // A typical request struct that consumers would hand-roll a Codec for.
    #[derive(Debug, PartialEq, Eq)]
    struct Join {
        user_id: u32,
        username: String,
    }

    impl Codec for Join {
        fn encode(&self, enc: &mut Encoder) {
            self.user_id.encode(enc);
            self.username.encode(enc);
        }
        fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
            Ok(Self {
                user_id: u32::decode(dec)?,
                username: String::decode(dec)?,
            })
        }
    }

    #[test]
    fn codec_pack_unpack_primitives() {
        for v in [0u32, 1, 42, u32::MAX] {
            assert_eq!(unpack::<u32>(&pack(&v)).unwrap(), v);
        }
        for v in [true, false] {
            assert_eq!(unpack::<bool>(&pack(&v)).unwrap(), v);
        }
        let s = "DrDrOS".to_string();
        assert_eq!(unpack::<String>(&pack(&s)).unwrap(), s);
    }

    #[test]
    fn codec_custom_type_roundtrip() {
        let join = Join { user_id: 7, username: "gravijet".into() };
        let bytes = pack(&join);
        let back: Join = unpack(&bytes).unwrap();
        assert_eq!(join, back);
    }

    #[test]
    fn unpack_rejects_trailing_bytes() {
        // Encode a u32, then append junk.
        let mut bytes = pack(&42u32);
        bytes.extend_from_slice(&[0xAA, 0xBB]);
        match unpack::<u32>(&bytes).unwrap_err() {
            DecodeError::LengthMismatch { available, .. } => assert_eq!(available, 2),
            other => panic!("expected LengthMismatch, got {other:?}"),
        }
    }

    /// In-memory duplex pipe used by the Conn tests. Two halves; each
    /// half writes into a shared VecDeque that the other half reads.
    /// Synchronous and single-threaded — fine for testing protocol
    /// round-trips without bringing in tokio or std::net.
    struct Pipe {
        send: std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<u8>>>,
        recv: std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<u8>>>,
    }

    fn pipe_pair() -> (Pipe, Pipe) {
        use std::cell::RefCell;
        use std::collections::VecDeque;
        use std::rc::Rc;
        let a = Rc::new(RefCell::new(VecDeque::new()));
        let b = Rc::new(RefCell::new(VecDeque::new()));
        (
            Pipe { send: a.clone(), recv: b.clone() },
            Pipe { send: b, recv: a },
        )
    }

    impl io::Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.send.borrow_mut().extend(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    impl io::Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut q = self.recv.borrow_mut();
            let n = q.len().min(buf.len());
            for slot in buf[..n].iter_mut() {
                *slot = q.pop_front().unwrap();
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe empty"));
            }
            Ok(n)
        }
    }

    #[test]
    fn conn_roundtrip_typed_message() {
        let (client, server) = pipe_pair();
        let mut client = Conn::new(client);
        let mut server = Conn::new(server);

        client.send_typed(/* kind=JOIN */ 1, &Join { user_id: 9, username: "ada".into() })
            .unwrap();

        let (kind, msg): (u8, Join) = server.recv_typed().unwrap();
        assert_eq!(kind, 1);
        assert_eq!(msg, Join { user_id: 9, username: "ada".into() });
    }

    #[test]
    fn conn_schema_mismatch_is_invalid_data() {
        // Send a frame with a payload too short to decode as `Join`.
        let (client, server) = pipe_pair();
        let mut client = Conn::new(client);
        let mut server = Conn::new(server);

        // Just a u32, no string — Join::decode expects both.
        client.send_typed(1, &42u32).unwrap();
        let err = server.recv_typed::<Join>().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // ─── Tier 2: correlation IDs ─────────────────────────────────────

    #[test]
    fn frame_roundtrip_preserves_id() {
        let frame = Frame::with_id(7, 0xABCD_1234, b"body".to_vec());
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        // kind byte, then the id as 4 BE bytes, before the payload.
        assert_eq!(buf[4], 7);
        assert_eq!(&buf[5..9], &0xABCD_1234u32.to_be_bytes());

        let got = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(got, frame);
        assert_eq!(got.id, 0xABCD_1234);
    }

    #[test]
    fn recv_request_then_reply_echoes_id() {
        // Drive both sides by hand on the synchronous pipe: the client
        // sends a correlated frame, the server decodes it as a Request,
        // answers with reply(), and the client sees the same id back.
        let (client, server) = pipe_pair();
        let mut client = Conn::new(client);
        let mut server = Conn::new(server);

        client
            .send_frame(&Frame::with_id(/* kind */ 1, /* id */ 77, pack(&Join {
                user_id: 3,
                username: "grace".into(),
            })))
            .unwrap();

        let req: Request<Join> = server.recv_request().unwrap();
        assert_eq!(req.kind, 1);
        assert_eq!(req.id, 77);
        assert_eq!(req.body, Join { user_id: 3, username: "grace".into() });

        server.reply(req.id, /* kind=OK */ 2, &"welcome".to_string()).unwrap();

        let reply = client.recv_frame().unwrap();
        assert_eq!(reply.kind, 2);
        assert_eq!(reply.id, 77);
        assert_eq!(unpack::<String>(&reply.payload).unwrap(), "welcome");
    }

    #[test]
    fn request_rejects_mismatched_reply_id() {
        // Pre-load a reply carrying the wrong id into the channel the
        // client reads from, then call request(): it allocates id 1,
        // sends, reads our planted frame (id 999) and must reject it.
        let (client, server) = pipe_pair();
        let mut client = Conn::new(client);
        let mut server = Conn::new(server);
        server
            .send_frame(&Frame::with_id(2, 999, pack(&"stale".to_string())))
            .unwrap();

        let err = client.request::<u32, String>(1, &42u32).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn alloc_id_skips_reserved_zero_on_wrap() {
        let (client, _server) = pipe_pair();
        let mut conn = Conn::new(client);
        conn.next_id = u32::MAX;
        assert_eq!(conn.alloc_id(), u32::MAX);
        // Next would wrap to 0 (reserved) — must land on 1 instead.
        assert_eq!(conn.alloc_id(), 1);
        assert_eq!(conn.alloc_id(), 2);
    }

    // ─── Tier 2: real TCP transport ──────────────────────────────────

    /// Full round-trip over loopback TCP: a server thread answers two
    /// correlated requests; the client's auto-allocated ids (1 then 2)
    /// must match each reply.
    #[test]
    fn tcp_request_reply_over_loopback() {
        let listener = tcp::Listener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut conn, _peer) = listener.accept().unwrap();
            // Echo-server: double the number, twice.
            for _ in 0..2 {
                let req: Request<u32> = conn.recv_request().unwrap();
                conn.reply(req.id, /* kind=OK */ 200, &(req.body * 2)).unwrap();
            }
        });

        let mut client = tcp::connect(addr).unwrap();
        let (k1, r1): (u8, u32) = client.request(/* kind=DOUBLE */ 100, &21u32).unwrap();
        assert_eq!((k1, r1), (200, 42));
        let (k2, r2): (u8, u32) = client.request(100, &50u32).unwrap();
        assert_eq!((k2, r2), (200, 100));

        server.join().unwrap();
    }
}
