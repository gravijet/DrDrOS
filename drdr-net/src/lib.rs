//! drdr-net — DrDrNet, the DrDrOS binary network protocol (Tier 1).
//!
//! Wire format
//! ───────────
//! Every message is a single self-describing **frame**:
//!
//! ```text
//!   ┌──────────┬──────┬───────────────────────────────┐
//!   │  len:u32 │ kind │       payload (len bytes)     │
//!   │   (BE)   │  u8  │           any bytes           │
//!   └──────────┴──────┴───────────────────────────────┘
//!     4 bytes   1 byte           variable
//! ```
//!
//! - `len` is the byte count of the payload (does NOT include itself or
//!   `kind`). Big-endian so a `tcpdump` / `xxd` dump reads naturally
//!   left-to-right.
//! - `kind` is a one-byte tag the application layer assigns meaning to
//!   (request? response? heartbeat?). DrDrNet itself doesn't interpret it.
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
//! Tier 2 will add request/response correlation IDs, a `Codec` trait
//! that types implement, and a small async runtime; Tier 3 connects
//! the protocol over `tokio` / `std::net::TcpStream` for real apps.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

/// Cap on `payload` byte length per frame (16 MiB). Larger transfers
/// belong in multiple frames so the reader can never be tricked into
/// allocating an unbounded buffer.
pub const MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;

/// One on-the-wire message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Application-defined tag — DrDrNet itself attaches no meaning.
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: u8, payload: Vec<u8>) -> Self {
        Self { kind, payload }
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
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Frame { kind: kind_buf[0], payload })
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
        // 4 len + 1 kind + 10 payload = 15.
        assert_eq!(buf.len(), 15);
        assert_eq!(&buf[..4], &10u32.to_be_bytes());
        assert_eq!(buf[4], 0x42);

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
}
