//! A from-scratch DEFLATE (RFC 1951) + zlib (RFC 1950) decompressor.
//!
//! This is the keystone of DrDrCodec: PNG image data, every ZIP entry
//! (and therefore DOCX), and most PDF content streams are DEFLATE. Get
//! this one algorithm right and four file formats open. The design
//! follows Mark Adler's reference "puff" — a canonical-Huffman decoder
//! driven by a least-significant-bit-first bit reader — re-expressed in
//! safe Rust with explicit error returns (a decoder must never panic on
//! hostile input).

/// LSB-first bit reader over a byte slice.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bitbuf: 0, bitcnt: 0 }
    }

    /// Read `n` bits (0..=24) as an integer, least-significant bit first.
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        while self.bitcnt < n {
            if self.pos >= self.data.len() {
                return Err("inflate: out of input".into());
            }
            self.bitbuf |= (self.data[self.pos] as u32) << self.bitcnt;
            self.pos += 1;
            self.bitcnt += 8;
        }
        let val = self.bitbuf & ((1u32 << n) - 1);
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(val)
    }

    /// Drop any partial bits and re-align to a byte boundary (stored block).
    fn align(&mut self) {
        self.bitbuf = 0;
        self.bitcnt = 0;
    }
}

/// A canonical Huffman code, stored as the "counts + symbols" form puff
/// uses: `counts[len]` codes have length `len`, and `symbols` lists every
/// symbol ordered first by code length then by symbol value.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    /// Build a canonical Huffman decoder from per-symbol code lengths
    /// (0 = symbol unused).
    fn build(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // Offsets of each length group within `symbols`.
        let mut offsets = [0u16; 16];
        for len in 1..16 {
            offsets[len] = offsets[len - 1] + counts[len - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    /// Decode one symbol, reading bits as needed (puff's `decode`).
    fn decode(&self, br: &mut BitReader) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("inflate: bad Huffman code".into())
    }
}

// Length / distance base values and extra-bit counts (RFC 1951 §3.2.5).
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Decode one compressed block body given its literal/length and distance
/// Huffman tables, appending into `out`.
fn inflate_block(
    br: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(br)?;
        if sym == 256 {
            return Ok(()); // end of block
        }
        if sym < 256 {
            out.push(sym as u8);
            continue;
        }
        let sym = (sym - 257) as usize;
        if sym >= LEN_BASE.len() {
            return Err("inflate: bad length symbol".into());
        }
        let len = LEN_BASE[sym] as usize + br.bits(LEN_EXTRA[sym] as u32)? as usize;
        let dsym = dist.decode(br)? as usize;
        if dsym >= DIST_BASE.len() {
            return Err("inflate: bad distance symbol".into());
        }
        let distance = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym] as u32)? as usize;
        if distance > out.len() {
            return Err("inflate: distance too far back".into());
        }
        let start = out.len() - distance;
        for i in 0..len {
            let b = out[start + i];
            out.push(b);
        }
    }
}

/// Fixed Huffman tables (RFC 1951 §3.2.6).
fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit = [0u8; 288];
    for (i, l) in lit.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist = [5u8; 30];
    (Huffman::build(&lit), Huffman::build(&dist))
}

/// Read the dynamic Huffman tables for one block (RFC 1951 §3.2.7).
fn dynamic_tables(br: &mut BitReader) -> Result<(Huffman, Huffman), String> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut cl_lengths = [0u8; 19];
    for &o in ORDER.iter().take(hclen) {
        cl_lengths[o] = br.bits(3)? as u8;
    }
    let cl = Huffman::build(&cl_lengths);

    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        let sym = cl.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err("inflate: repeat with no previous length".into());
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                for _ in 0..rep {
                    if i >= total {
                        return Err("inflate: length repeat overflow".into());
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let rep = 3 + br.bits(3)? as usize;
                for _ in 0..rep {
                    if i >= total {
                        return Err("inflate: zero-run overflow".into());
                    }
                    lengths[i] = 0;
                    i += 1;
                }
            }
            18 => {
                let rep = 11 + br.bits(7)? as usize;
                for _ in 0..rep {
                    if i >= total {
                        return Err("inflate: zero-run overflow".into());
                    }
                    lengths[i] = 0;
                    i += 1;
                }
            }
            _ => return Err("inflate: bad code-length symbol".into()),
        }
    }
    let lit = Huffman::build(&lengths[..hlit]);
    let dist = Huffman::build(&lengths[hlit..]);
    Ok((lit, dist))
}

/// Inflate a raw DEFLATE stream into the decompressed bytes.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut br = BitReader::new(data);
    let mut out = Vec::new();
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                // Stored: align, read LEN/NLEN, copy raw bytes.
                br.align();
                if br.pos + 4 > br.data.len() {
                    return Err("inflate: truncated stored header".into());
                }
                let len = br.data[br.pos] as usize | ((br.data[br.pos + 1] as usize) << 8);
                br.pos += 4; // skip LEN + NLEN
                if br.pos + len > br.data.len() {
                    return Err("inflate: truncated stored block".into());
                }
                out.extend_from_slice(&br.data[br.pos..br.pos + len]);
                br.pos += len;
            }
            1 => {
                let (lit, dist) = fixed_tables();
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            _ => return Err("inflate: reserved block type".into()),
        }
        if bfinal == 1 {
            return Ok(out);
        }
        if out.len() > 256 * 1024 * 1024 {
            return Err("inflate: output too large".into());
        }
    }
}

/// Decompress a zlib stream (RFC 1950): a 2-byte header, the DEFLATE
/// payload, and a 4-byte Adler-32 trailer we verify.
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 6 {
        return Err("zlib: too short".into());
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0f != 8 {
        return Err("zlib: not DEFLATE".into());
    }
    if (cmf as u16 * 256 + flg as u16) % 31 != 0 {
        return Err("zlib: bad header check".into());
    }
    if flg & 0x20 != 0 {
        return Err("zlib: preset dictionary unsupported".into());
    }
    let out = inflate(&data[2..])?;
    // Verify the Adler-32 trailer (last 4 bytes, big-endian).
    let want = u32::from_be_bytes([
        data[data.len() - 4],
        data[data.len() - 3],
        data[data.len() - 2],
        data[data.len() - 1],
    ]);
    if adler32(&out) != want {
        return Err("zlib: Adler-32 mismatch".into());
    }
    Ok(out)
}

/// Adler-32 checksum (RFC 1950).
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated with Python zlib (see commit message): the message below
    // compressed at level 9, both as a zlib stream and as raw DEFLATE.
    const MSG: &[u8] = b"hello, hello, hello world! DrDrOS inflate test 1234567890\nhello, hello, hello world! DrDrOS inflate test 1234567890\nhello, hello, hello world! DrDrOS inflate test 1234567890\n";
    const ZLIB: &[u8] = &[
        120, 218, 203, 72, 205, 201, 201, 215, 81, 200, 64, 162, 20, 202, 243, 139, 114, 82, 20,
        21, 92, 138, 92, 138, 252, 131, 21, 50, 243, 210, 114, 18, 75, 82, 21, 74, 82, 139, 75, 20,
        12, 141, 140, 77, 76, 205, 204, 45, 44, 13, 184, 50, 6, 64, 39, 0, 129, 137, 55, 144,
    ];
    const RAW: &[u8] = &[
        203, 72, 205, 201, 201, 215, 81, 200, 64, 162, 20, 202, 243, 139, 114, 82, 20, 21, 92, 138,
        92, 138, 252, 131, 21, 50, 243, 210, 114, 18, 75, 82, 21, 74, 82, 139, 75, 20, 12, 141,
        140, 77, 76, 205, 204, 45, 44, 13, 184, 50, 6, 64, 39, 0,
    ];

    #[test]
    fn inflates_raw_deflate() {
        assert_eq!(inflate(RAW).unwrap(), MSG);
    }

    #[test]
    fn decompresses_zlib_with_checksum() {
        assert_eq!(zlib_decompress(ZLIB).unwrap(), MSG);
    }

    #[test]
    fn stored_block_roundtrips() {
        // A hand-built stored (uncompressed) DEFLATE block: BFINAL=1,
        // BTYPE=00, then LEN/NLEN and the literal bytes.
        let payload = b"raw bytes";
        let mut d = vec![0x01]; // BFINAL=1, BTYPE=00 (in the low 3 bits)
        let len = payload.len() as u16;
        d.extend_from_slice(&len.to_le_bytes());
        d.extend_from_slice(&(!len).to_le_bytes());
        d.extend_from_slice(payload);
        assert_eq!(inflate(&d).unwrap(), payload);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(inflate(&[0xff, 0xff, 0xff]).is_err());
        assert!(zlib_decompress(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).is_err());
    }
}
