//! A from-scratch **baseline** JPEG decoder (sequential DCT, Huffman).
//!
//! JPEG shares nothing with DEFLATE: it is DC-prediction + run-length
//! coded AC coefficients, Huffman-entropy-coded, then an inverse DCT and
//! a YCbCr→RGB colour transform. This implements the common baseline
//! (SOF0) path that essentially every camera and "Save as JPEG" produces:
//! 8-bit precision, 1 or 3 components, arbitrary chroma subsampling,
//! restart markers. Progressive (SOF2), arithmetic coding and 12-bit are
//! rejected with a clear message rather than mis-decoded.
//!
//! The output is the same flat RGBA8 [`Image`](crate::png::Image) the rest
//! of the codecs produce, so the image viewer treats a JPEG like any
//! other picture.

use crate::png::Image;

/// Natural-order index for each of the 64 zig-zag positions.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27,
    20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58,
    59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[derive(Clone, Copy, Default)]
struct Component {
    id: u8,
    h: u8,
    v: u8,
    tq: usize,
    // Selected in the scan header:
    td: usize,
    ta: usize,
    pred: i32,
}

#[derive(Clone, Default)]
struct Huff {
    // Canonical-Huffman decode tables (JPEG Annex C / F).
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [usize; 17],
    values: Vec<u8>,
}

impl Huff {
    fn build(counts: &[u8; 16], values: Vec<u8>) -> Huff {
        let mut sizes = Vec::new();
        for (l, &c) in counts.iter().enumerate() {
            for _ in 0..c {
                sizes.push((l + 1) as u8);
            }
        }
        let mut codes = vec![0i32; sizes.len()];
        let mut code = 0i32;
        let mut k = 0;
        if !sizes.is_empty() {
            let mut si = sizes[0];
            while k < sizes.len() {
                while k < sizes.len() && sizes[k] == si {
                    codes[k] = code;
                    code += 1;
                    k += 1;
                }
                code <<= 1;
                si += 1;
            }
        }
        let mut h = Huff { mincode: [0; 17], maxcode: [-1; 17], valptr: [0; 17], values };
        let mut p = 0usize;
        for l in 1..=16 {
            if counts[l - 1] > 0 {
                h.valptr[l] = p;
                h.mincode[l] = codes[p];
                p += counts[l - 1] as usize;
                h.maxcode[l] = codes[p - 1];
            } else {
                h.maxcode[l] = -1;
            }
        }
        h
    }
}

/// Entropy-stream bit reader: MSB-first, unstuffs `FF 00`, and stops
/// feeding bits when it reaches a marker (padding with 1s, per spec).
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u32,
    count: u32,
    marker: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos, bits: 0, count: 0, marker: false }
    }

    fn next_byte(&mut self) -> u8 {
        if self.marker || self.pos >= self.data.len() {
            return 0xFF;
        }
        let b = self.data[self.pos];
        self.pos += 1;
        if b == 0xFF {
            let n = self.data.get(self.pos).copied().unwrap_or(0xD9);
            if n == 0x00 {
                self.pos += 1; // stuffed: a literal 0xFF
                return 0xFF;
            }
            // A real marker: rewind so a restart resync can see it.
            self.pos -= 1;
            self.marker = true;
            return 0xFF;
        }
        b
    }

    fn bit(&mut self) -> u32 {
        if self.count == 0 {
            self.bits = self.next_byte() as u32;
            self.count = 8;
        }
        self.count -= 1;
        (self.bits >> self.count) & 1
    }

    fn receive(&mut self, s: u8) -> i32 {
        let mut v = 0i32;
        for _ in 0..s {
            v = (v << 1) | self.bit() as i32;
        }
        v
    }

    /// Sign-extend an `s`-bit magnitude into a signed coefficient.
    fn receive_extend(&mut self, s: u8) -> i32 {
        if s == 0 {
            return 0;
        }
        let v = self.receive(s);
        if v < (1 << (s - 1)) {
            v + (-1 << s) + 1
        } else {
            v
        }
    }

    fn decode(&mut self, h: &Huff) -> Result<u8, String> {
        let mut code = 0i32;
        for l in 1..=16 {
            code = (code << 1) | self.bit() as i32;
            if h.maxcode[l] >= 0 && code <= h.maxcode[l] {
                let idx = h.valptr[l] + (code - h.mincode[l]) as usize;
                return h.values.get(idx).copied().ok_or_else(|| "jpeg: bad huff index".into());
            }
        }
        Err("jpeg: invalid Huffman code".into())
    }

    /// At a restart boundary: drop partial bits and skip the RSTn marker.
    fn restart(&mut self) {
        self.count = 0;
        self.marker = false;
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xFF {
                let n = self.data[self.pos + 1];
                if (0xD0..=0xD7).contains(&n) {
                    self.pos += 2;
                    return;
                }
            }
            self.pos += 1;
        }
    }
}

/// Decode a baseline JPEG into RGBA8, or a human-readable error.
pub fn decode_jpeg(data: &[u8]) -> Result<Image, String> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err("jpeg: not a JPEG (no SOI)".into());
    }
    let mut pos = 2usize;

    let mut qtables: [[u16; 64]; 4] = [[0; 64]; 4];
    let mut dc_tables: [Option<Huff>; 4] = Default::default();
    let mut ac_tables: [Option<Huff>; 4] = Default::default();
    let mut comps: Vec<Component> = Vec::new();
    let mut width = 0u32;
    let mut height = 0u32;
    let mut restart_interval = 0usize;

    loop {
        if pos + 1 >= data.len() {
            return Err("jpeg: ran out of markers".into());
        }
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        pos += 2;
        match marker {
            0xD9 => return Err("jpeg: EOI before image data".into()),
            0xC0 => {
                // Baseline frame header.
                let prec = data[pos + 2];
                if prec != 8 {
                    return Err("jpeg: only 8-bit precision supported".into());
                }
                height = u16::from_be_bytes([data[pos + 3], data[pos + 4]]) as u32;
                width = u16::from_be_bytes([data[pos + 5], data[pos + 6]]) as u32;
                let nc = data[pos + 7] as usize;
                comps.clear();
                for i in 0..nc {
                    let o = pos + 8 + i * 3;
                    comps.push(Component {
                        id: data[o],
                        h: data[o + 1] >> 4,
                        v: data[o + 1] & 0x0F,
                        tq: data[o + 2] as usize,
                        ..Default::default()
                    });
                }
                pos += seg_len(data, pos)?;
            }
            0xC1 => return Err("jpeg: extended sequential not supported".into()),
            0xC2 => return Err("jpeg: progressive JPEG not supported yet".into()),
            0xC3 | 0xC5..=0xCB | 0xCD..=0xCF => {
                return Err("jpeg: unsupported SOF variant".into());
            }
            0xC4 => {
                let len = seg_len(data, pos)?;
                parse_dht(data, pos, &mut dc_tables, &mut ac_tables, len)?;
                pos += len;
            }
            0xDB => {
                let len = seg_len(data, pos)?;
                parse_dqt(data, pos, &mut qtables, len)?;
                pos += len;
            }
            0xDD => {
                restart_interval =
                    u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
                pos += seg_len(data, pos)?;
            }
            0xDA => {
                // Start of Scan: select tables, then decode entropy data.
                let ns = data[pos + 2] as usize;
                for i in 0..ns {
                    let cs = data[pos + 3 + i * 2];
                    let tdta = data[pos + 4 + i * 2];
                    if let Some(c) = comps.iter_mut().find(|c| c.id == cs) {
                        c.td = (tdta >> 4) as usize;
                        c.ta = (tdta & 0x0F) as usize;
                    }
                }
                let scan_start = pos + seg_len(data, pos)?;
                return decode_scan(
                    data,
                    scan_start,
                    width,
                    height,
                    &mut comps,
                    &qtables,
                    &dc_tables,
                    &ac_tables,
                    restart_interval,
                );
            }
            0xE0..=0xEF | 0xFE => pos += seg_len(data, pos)?, // APPn / COM
            0xD0..=0xD7 | 0x01 => {} // standalone, no payload
            _ => pos += seg_len(data, pos)?,
        }
    }
}

/// Length of a marker segment (the 2-byte big-endian length includes
/// itself), as an advance from the start of the segment payload.
fn seg_len(data: &[u8], pos: usize) -> Result<usize, String> {
    if pos + 1 >= data.len() {
        return Err("jpeg: truncated segment".into());
    }
    Ok(u16::from_be_bytes([data[pos], data[pos + 1]]) as usize)
}

fn parse_dqt(
    data: &[u8],
    pos: usize,
    qtables: &mut [[u16; 64]; 4],
    len: usize,
) -> Result<(), String> {
    let end = pos + len;
    let mut p = pos + 2;
    while p < end {
        let pq = data[p] >> 4;
        let tq = (data[p] & 0x0F) as usize;
        p += 1;
        if tq >= 4 {
            return Err("jpeg: bad quant table id".into());
        }
        for i in 0..64 {
            if pq == 0 {
                qtables[tq][i] = data[p] as u16;
                p += 1;
            } else {
                qtables[tq][i] = u16::from_be_bytes([data[p], data[p + 1]]);
                p += 2;
            }
        }
    }
    Ok(())
}

fn parse_dht(
    data: &[u8],
    pos: usize,
    dc: &mut [Option<Huff>; 4],
    ac: &mut [Option<Huff>; 4],
    len: usize,
) -> Result<(), String> {
    let end = pos + len;
    let mut p = pos + 2;
    while p < end {
        let tc = data[p] >> 4;
        let th = (data[p] & 0x0F) as usize;
        p += 1;
        if th >= 4 {
            return Err("jpeg: bad Huffman table id".into());
        }
        let mut counts = [0u8; 16];
        counts.copy_from_slice(&data[p..p + 16]);
        p += 16;
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        let values = data[p..p + total].to_vec();
        p += total;
        let h = Huff::build(&counts, values);
        if tc == 0 {
            dc[th] = Some(h);
        } else {
            ac[th] = Some(h);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    data: &[u8],
    start: usize,
    width: u32,
    height: u32,
    comps: &mut [Component],
    qtables: &[[u16; 64]; 4],
    dc_tables: &[Option<Huff>; 4],
    ac_tables: &[Option<Huff>; 4],
    restart_interval: usize,
) -> Result<Image, String> {
    if width == 0 || height == 0 || comps.is_empty() {
        return Err("jpeg: missing frame header".into());
    }
    let hmax = comps.iter().map(|c| c.h).max().unwrap() as u32;
    let vmax = comps.iter().map(|c| c.v).max().unwrap() as u32;
    let mcu_w = 8 * hmax;
    let mcu_h = 8 * vmax;
    let mcus_x = width.div_ceil(mcu_w);
    let mcus_y = height.div_ceil(mcu_h);

    // One full-resolution-of-its-own plane per component.
    let mut planes: Vec<Vec<u8>> = Vec::new();
    let mut strides: Vec<u32> = Vec::new();
    for c in comps.iter() {
        let pw = mcus_x * c.h as u32 * 8;
        let ph = mcus_y * c.v as u32 * 8;
        planes.push(vec![0u8; (pw * ph) as usize]);
        strides.push(pw);
    }

    let mut br = BitReader::new(data, start);
    let mut block = [0f32; 64];
    let mut mcu_count = 0usize;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart_interval > 0 && mcu_count > 0 && mcu_count % restart_interval == 0 {
                br.restart();
                for c in comps.iter_mut() {
                    c.pred = 0;
                }
            }
            for ci in 0..comps.len() {
                let (h, v, tq, td, ta) = {
                    let c = &comps[ci];
                    (c.h as u32, c.v as u32, c.tq, c.td, c.ta)
                };
                let dc_t = dc_tables[td].as_ref().ok_or("jpeg: missing DC table")?;
                let ac_t = ac_tables[ta].as_ref().ok_or("jpeg: missing AC table")?;
                for by in 0..v {
                    for bx in 0..h {
                        decode_block(
                            &mut br,
                            &mut block,
                            &mut comps[ci].pred,
                            dc_t,
                            ac_t,
                            &qtables[tq],
                        )?;
                        idct(&mut block);
                        // Place into the component plane.
                        let stride = strides[ci];
                        let px0 = (mx * h + bx) * 8;
                        let py0 = (my * v + by) * 8;
                        for yy in 0..8u32 {
                            for xx in 0..8u32 {
                                let val = (block[(yy * 8 + xx) as usize] + 128.0)
                                    .round()
                                    .clamp(0.0, 255.0) as u8;
                                let o = ((py0 + yy) * stride + px0 + xx) as usize;
                                planes[ci][o] = val;
                            }
                        }
                    }
                }
            }
            mcu_count += 1;
        }
    }

    // Compose RGBA, upsampling chroma by nearest-neighbour.
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let o = ((y * width + x) * 4) as usize;
            if comps.len() == 1 {
                let s = sample(&planes[0], strides[0], comps[0], hmax, vmax, x, y);
                rgba[o] = s;
                rgba[o + 1] = s;
                rgba[o + 2] = s;
            } else {
                let yc = sample(&planes[0], strides[0], comps[0], hmax, vmax, x, y) as f32;
                let cb = sample(&planes[1], strides[1], comps[1], hmax, vmax, x, y) as f32 - 128.0;
                let cr = sample(&planes[2], strides[2], comps[2], hmax, vmax, x, y) as f32 - 128.0;
                rgba[o] = (yc + 1.402 * cr).round().clamp(0.0, 255.0) as u8;
                rgba[o + 1] =
                    (yc - 0.344136 * cb - 0.714136 * cr).round().clamp(0.0, 255.0) as u8;
                rgba[o + 2] = (yc + 1.772 * cb).round().clamp(0.0, 255.0) as u8;
            }
            rgba[o + 3] = 0xFF;
        }
    }
    Ok(Image { w: width, h: height, rgba })
}

/// Nearest-neighbour sample of a component plane at full-image (x, y).
fn sample(plane: &[u8], stride: u32, c: Component, hmax: u32, vmax: u32, x: u32, y: u32) -> u8 {
    let sx = x * c.h as u32 / hmax;
    let sy = y * c.v as u32 / vmax;
    plane.get((sy * stride + sx) as usize).copied().unwrap_or(0)
}

fn decode_block(
    br: &mut BitReader,
    block: &mut [f32; 64],
    pred: &mut i32,
    dc: &Huff,
    ac: &Huff,
    q: &[u16; 64],
) -> Result<(), String> {
    let mut zz = [0i32; 64];
    // DC: difference from the running predictor.
    let t = br.decode(dc)?;
    let diff = br.receive_extend(t);
    *pred += diff;
    zz[0] = *pred * q[0] as i32;
    // AC: run-length of zeros + a coefficient, in zig-zag order.
    let mut k = 1usize;
    while k < 64 {
        let rs = br.decode(ac)?;
        let r = (rs >> 4) as usize;
        let s = rs & 0x0F;
        if s == 0 {
            if r == 15 {
                k += 16; // ZRL: sixteen zeros
                continue;
            }
            break; // EOB
        }
        k += r;
        if k >= 64 {
            break;
        }
        zz[k] = br.receive_extend(s) * q[k] as i32;
        k += 1;
    }
    // Scatter the zig-zag coefficients into the natural-order block.
    for (zi, nat) in ZIGZAG.iter().enumerate() {
        block[*nat] = zz[zi] as f32;
    }
    Ok(())
}

/// Separable float inverse DCT over an 8×8 block (in place).
fn idct(block: &mut [f32; 64]) {
    use std::f32::consts::PI;
    // Precompute the cosine basis once.
    thread_local! {
        static COS: [[f32; 8]; 8] = {
            let mut c = [[0f32; 8]; 8];
            for (u, row) in c.iter_mut().enumerate() {
                for (x, v) in row.iter_mut().enumerate() {
                    *v = (((2 * x + 1) as f32) * u as f32 * PI / 16.0).cos();
                }
            }
            c
        };
    }
    COS.with(|cos| {
        let mut tmp = [0f32; 64];
        // Rows.
        for y in 0..8 {
            for x in 0..8 {
                let mut sum = 0f32;
                for u in 0..8 {
                    let cu = if u == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
                    sum += cu * block[y * 8 + u] * cos[u][x];
                }
                tmp[y * 8 + x] = sum * 0.5;
            }
        }
        // Columns.
        for x in 0..8 {
            for y in 0..8 {
                let mut sum = 0f32;
                for v in 0..8 {
                    let cv = if v == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
                    sum += cv * tmp[v * 8 + x] * cos[v][y];
                }
                block[y * 8 + x] = sum * 0.5;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real baseline JPEG (16×16, solid teal, 4:4:4) written by Pillow.
    const SOLID: &[u8] = &[
        255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 255, 219, 0, 67,
        0, 3, 2, 2, 3, 2, 2, 3, 3, 3, 3, 4, 3, 3, 4, 5, 8, 5, 5, 4, 4, 5, 10, 7, 7, 6, 8, 12, 10,
        12, 12, 11, 10, 11, 11, 13, 14, 18, 16, 13, 14, 17, 14, 11, 11, 16, 22, 16, 17, 19, 20,
        21, 21, 21, 12, 15, 23, 24, 22, 20, 24, 18, 20, 21, 20, 255, 219, 0, 67, 1, 3, 4, 4, 5, 4,
        5, 9, 5, 5, 9, 20, 13, 11, 13, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20,
        20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20,
        20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 255, 192, 0, 17, 8, 0, 16, 0, 16, 3,
        1, 17, 0, 2, 17, 1, 3, 17, 1, 255, 196, 0, 31, 0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0,
        0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 255, 196, 0, 181, 16, 0, 2, 1, 3, 3, 2, 4,
        3, 5, 5, 4, 4, 0, 0, 1, 125, 1, 2, 3, 0, 4, 17, 5, 18, 33, 49, 65, 6, 19, 81, 97, 7, 34,
        113, 20, 50, 129, 145, 161, 8, 35, 66, 177, 193, 21, 82, 209, 240, 36, 51, 98, 114, 130,
        9, 10, 22, 23, 24, 25, 26, 37, 38, 39, 40, 41, 42, 52, 53, 54, 55, 56, 57, 58, 67, 68, 69,
        70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105, 106,
        115, 116, 117, 118, 119, 120, 121, 122, 131, 132, 133, 134, 135, 136, 137, 138, 146, 147,
        148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178, 179,
        180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211,
        212, 213, 214, 215, 216, 217, 218, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 241,
        242, 243, 244, 245, 246, 247, 248, 249, 250, 255, 196, 0, 31, 1, 0, 3, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 255, 196, 0, 181, 17, 0, 2,
        1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119, 0, 1, 2, 3, 17, 4, 5, 33, 49, 6, 18, 65, 81,
        7, 97, 113, 19, 34, 50, 129, 8, 20, 66, 145, 161, 177, 193, 9, 35, 51, 82, 240, 21, 98,
        114, 209, 10, 22, 36, 52, 225, 37, 241, 23, 24, 25, 26, 38, 39, 40, 41, 42, 53, 54, 55,
        56, 57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101,
        102, 103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 130, 131, 132, 133, 134,
        135, 136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166,
        167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198,
        199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 226, 227, 228, 229, 230,
        231, 232, 233, 234, 242, 243, 244, 245, 246, 247, 248, 249, 250, 255, 218, 0, 12, 3, 1, 0,
        2, 17, 3, 17, 0, 63, 0, 202, 160, 254, 103, 10, 0, 40, 0, 160, 15, 255, 217,
    ];

    #[test]
    fn decodes_a_solid_baseline_jpeg() {
        let img = decode_jpeg(SOLID).expect("jpeg should decode");
        assert_eq!((img.w, img.h), (16, 16));
        // The source was solid (40,160,120); JPEG is lossy so allow a
        // small tolerance, but it must land on that colour, not noise.
        let (r, g, b) = (img.rgba[0], img.rgba[1], img.rgba[2]);
        assert!((r as i32 - 40).abs() <= 8, "R was {r}");
        assert!((g as i32 - 160).abs() <= 8, "G was {g}");
        assert!((b as i32 - 120).abs() <= 8, "B was {b}");
    }

    // A real 4:2:0-subsampled baseline JPEG (16×16, left half red, right
    // half blue) — exercises chroma upsampling, the trickiest path.
    const SPLIT: &[u8] = &[
        255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1,
        0, 1, 0, 0, 255, 219, 0, 67, 0, 3, 2, 2, 2, 2, 2, 3,
        2, 2, 2, 3, 3, 3, 3, 4, 6, 4, 4, 4, 4, 4, 8, 6,
        6, 5, 6, 9, 8, 10, 10, 9, 8, 9, 9, 10, 12, 15, 12, 10,
        11, 14, 11, 9, 9, 13, 17, 13, 14, 15, 16, 16, 17, 16, 10, 12,
        18, 19, 18, 16, 19, 15, 16, 16, 16, 255, 219, 0, 67, 1, 3, 3,
        3, 4, 3, 4, 8, 4, 4, 8, 16, 11, 9, 11, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 255, 192,
        0, 17, 8, 0, 16, 0, 16, 3, 1, 34, 0, 2, 17, 1, 3, 17,
        1, 255, 196, 0, 31, 0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0,
        0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9,
        10, 11, 255, 196, 0, 181, 16, 0, 2, 1, 3, 3, 2, 4, 3, 5,
        5, 4, 4, 0, 0, 1, 125, 1, 2, 3, 0, 4, 17, 5, 18, 33,
        49, 65, 6, 19, 81, 97, 7, 34, 113, 20, 50, 129, 145, 161, 8, 35,
        66, 177, 193, 21, 82, 209, 240, 36, 51, 98, 114, 130, 9, 10, 22, 23,
        24, 25, 26, 37, 38, 39, 40, 41, 42, 52, 53, 54, 55, 56, 57, 58,
        67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90,
        99, 100, 101, 102, 103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122,
        131, 132, 133, 134, 135, 136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153,
        154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178, 179, 180, 181, 182, 183,
        184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211, 212, 213,
        214, 215, 216, 217, 218, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 241,
        242, 243, 244, 245, 246, 247, 248, 249, 250, 255, 196, 0, 31, 1, 0, 3,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1,
        2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 255, 196, 0, 181, 17, 0,
        2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119, 0,
        1, 2, 3, 17, 4, 5, 33, 49, 6, 18, 65, 81, 7, 97, 113, 19,
        34, 50, 129, 8, 20, 66, 145, 161, 177, 193, 9, 35, 51, 82, 240, 21,
        98, 114, 209, 10, 22, 36, 52, 225, 37, 241, 23, 24, 25, 26, 38, 39,
        40, 41, 42, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73,
        74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105,
        106, 115, 116, 117, 118, 119, 120, 121, 122, 130, 131, 132, 133, 134, 135, 136,
        137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166,
        167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196,
        197, 198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 226,
        227, 228, 229, 230, 231, 232, 233, 234, 242, 243, 244, 245, 246, 247, 248, 249,
        250, 255, 218, 0, 12, 3, 1, 0, 2, 17, 3, 17, 0, 63, 0, 249,
        210, 188, 10, 189, 246, 188, 10, 191, 112, 250, 25, 127, 204, 239, 254, 229,
        191, 247, 96, 254, 141, 250, 89, 255, 0, 204, 159, 254, 230, 63, 247, 1,
        255, 217,
    ];

    #[test]
    fn decodes_a_subsampled_jpeg() {
        let img = decode_jpeg(SPLIT).expect("subsampled jpeg should decode");
        assert_eq!((img.w, img.h), (16, 16));
        // Pixel (4,8): red-ish; pixel (12,8): blue-ish.
        let left = (8 * 16 + 4) * 4;
        let right = (8 * 16 + 12) * 4;
        assert!(img.rgba[left] > 180 && img.rgba[left + 2] < 70, "left should be red");
        assert!(img.rgba[right + 2] > 180 && img.rgba[right] < 70, "right should be blue");
    }

    #[test]
    fn rejects_non_jpeg() {
        assert!(decode_jpeg(b"\x89PNG\r\n\x1a\n").is_err());
    }
}
