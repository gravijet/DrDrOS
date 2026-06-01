//! A from-scratch PNG decoder (the common subset: 8-bit and 16-bit
//! greyscale / RGB / palette / with-alpha). It parses the chunk stream,
//! concatenates the `IDAT` data, [`zlib_decompress`](crate::inflate)es it,
//! reverses the per-scanline filters, and produces a flat RGBA8 buffer the
//! image viewer can paint. Interlaced (Adam7) PNGs and <8-bit non-palette
//! depths are rejected with a clear error rather than mis-decoded.

use crate::inflate::zlib_decompress;

/// A decoded image: row-major RGBA, 8 bits per channel.
pub struct Image {
    pub w: u32,
    pub h: u32,
    /// `w * h * 4` bytes, R,G,B,A per pixel.
    pub rgba: Vec<u8>,
}

const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Decode a PNG into RGBA8, or return a human-readable error.
pub fn decode_png(data: &[u8]) -> Result<Image, String> {
    if data.len() < 8 || data[..8] != SIG {
        return Err("png: bad signature".into());
    }
    let mut pos = 8usize;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut interlace = 0u8;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let kind = &data[pos + 4..pos + 8];
        let body_start = pos + 8;
        if body_start + len + 4 > data.len() {
            return Err("png: truncated chunk".into());
        }
        let body = &data[body_start..body_start + len];
        match kind {
            b"IHDR" => {
                if len < 13 {
                    return Err("png: short IHDR".into());
                }
                width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                bit_depth = body[8];
                color_type = body[9];
                interlace = body[12];
            }
            b"PLTE" => {
                for c in body.chunks(3) {
                    if c.len() == 3 {
                        palette.push([c[0], c[1], c[2]]);
                    }
                }
            }
            b"tRNS" => trns = body.to_vec(),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos = body_start + len + 4; // skip body + CRC
    }

    if width == 0 || height == 0 {
        return Err("png: no IHDR".into());
    }
    if interlace != 0 {
        return Err("png: interlaced PNG not supported".into());
    }
    if width as u64 * height as u64 > 64 * 1024 * 1024 {
        return Err("png: image too large".into());
    }

    let channels = match color_type {
        0 => 1, // greyscale
        2 => 3, // RGB
        3 => 1, // palette index
        4 => 2, // grey + alpha
        6 => 4, // RGBA
        _ => return Err("png: unknown colour type".into()),
    };
    if !matches!(bit_depth, 8 | 16) && !(color_type == 3 && matches!(bit_depth, 1 | 2 | 4 | 8)) {
        return Err(format!("png: unsupported bit depth {bit_depth}"));
    }

    let raw = zlib_decompress(&idat)?;
    let bits_per_pixel = channels * bit_depth as usize;
    let stride = (width as usize * bits_per_pixel + 7) / 8;
    let bpp = bits_per_pixel.div_ceil(8).max(1); // bytes per pixel for filtering
    let expected = (stride + 1) * height as usize;
    if raw.len() < expected {
        return Err("png: not enough image data".into());
    }

    // Reverse the scanline filters into `lines` (stride bytes per row).
    let mut lines = vec![0u8; stride * height as usize];
    let mut prev_start = 0usize;
    let mut have_prev = false;
    for y in 0..height as usize {
        let in_off = y * (stride + 1);
        let filter = raw[in_off];
        let src = &raw[in_off + 1..in_off + 1 + stride];
        let out_off = y * stride;
        for x in 0..stride {
            let a = if x >= bpp { lines[out_off + x - bpp] } else { 0 };
            let b = if have_prev { lines[prev_start + x] } else { 0 };
            let c = if have_prev && x >= bpp { lines[prev_start + x - bpp] } else { 0 };
            let v = src[x] as i32;
            let recon = match filter {
                0 => v,
                1 => v + a as i32,
                2 => v + b as i32,
                3 => v + ((a as i32 + b as i32) / 2),
                4 => v + paeth(a as i32, b as i32, c as i32),
                _ => return Err("png: bad filter type".into()),
            };
            lines[out_off + x] = (recon & 0xff) as u8;
        }
        prev_start = out_off;
        have_prev = true;
    }

    // Expand each scanline into RGBA8.
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        let row = &lines[y * stride..(y + 1) * stride];
        for x in 0..width as usize {
            let (r, g, b, a) = sample(row, x, color_type, bit_depth, &palette, &trns)?;
            let o = (y * width as usize + x) * 4;
            rgba[o] = r;
            rgba[o + 1] = g;
            rgba[o + 2] = b;
            rgba[o + 3] = a;
        }
    }
    Ok(Image { w: width, h: height, rgba })
}

fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Read one pixel from a filtered scanline as RGBA8.
fn sample(
    row: &[u8],
    x: usize,
    color_type: u8,
    bit_depth: u8,
    palette: &[[u8; 3]],
    trns: &[u8],
) -> Result<(u8, u8, u8, u8), String> {
    // 16-bit: take the high byte of each channel (downsample to 8-bit).
    let step = bit_depth as usize / 8;
    match color_type {
        0 => {
            let v = read_channel(row, x, bit_depth, 1, 0);
            Ok((v, v, v, 255))
        }
        2 => {
            let base = x * 3 * step;
            let r = row[base];
            let g = row[base + step];
            let b = row[base + 2 * step];
            Ok((r, g, b, 255))
        }
        3 => {
            let idx = read_index(row, x, bit_depth) as usize;
            let c = palette.get(idx).ok_or("png: palette index out of range")?;
            let a = trns.get(idx).copied().unwrap_or(255);
            Ok((c[0], c[1], c[2], a))
        }
        4 => {
            let base = x * 2 * step;
            let v = row[base];
            let a = row[base + step];
            Ok((v, v, v, a))
        }
        6 => {
            let base = x * 4 * step;
            Ok((row[base], row[base + step], row[base + 2 * step], row[base + 3 * step]))
        }
        _ => Err("png: unsupported colour type".into()),
    }
}

/// Read an 8/16-bit channel `ch` of pixel `x` (greyscale helper).
fn read_channel(row: &[u8], x: usize, bit_depth: u8, channels: usize, ch: usize) -> u8 {
    let step = bit_depth as usize / 8;
    if step == 0 {
        // sub-byte greyscale: treat as palette-style index scaled up.
        let idx = read_index(row, x, bit_depth);
        let max = (1u16 << bit_depth) - 1;
        return ((idx as u16 * 255) / max) as u8;
    }
    row[(x * channels + ch) * step]
}

/// Read a sub-byte (1/2/4/8-bit) palette / greyscale index for pixel `x`.
fn read_index(row: &[u8], x: usize, bit_depth: u8) -> u8 {
    match bit_depth {
        8 => row[x],
        4 => {
            let byte = row[x / 2];
            if x % 2 == 0 { byte >> 4 } else { byte & 0x0f }
        }
        2 => {
            let byte = row[x / 4];
            let shift = 6 - (x % 4) * 2;
            (byte >> shift) & 0x03
        }
        1 => {
            let byte = row[x / 8];
            let shift = 7 - (x % 8);
            (byte >> shift) & 0x01
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 2x2 RGBA PNG (red, green / blue, white) built by Python.
    const TINY_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6,
        0, 0, 0, 114, 182, 13, 36, 0, 0, 0, 18, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240,
        31, 12, 129, 52, 24, 0, 0, 73, 200, 9, 247, 3, 217, 100, 241, 0, 0, 0, 0, 73, 69, 78, 68,
        174, 66, 96, 130,
    ];

    #[test]
    fn decodes_a_tiny_rgba_png() {
        let img = decode_png(TINY_PNG).expect("png should decode");
        assert_eq!((img.w, img.h), (2, 2));
        // Top-left red, top-right green, bottom-left blue, bottom-right white.
        assert_eq!(&img.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&img.rgba[4..8], &[0, 255, 0, 255]);
        assert_eq!(&img.rgba[8..12], &[0, 0, 255, 255]);
        assert_eq!(&img.rgba[12..16], &[255, 255, 255, 255]);
    }

    #[test]
    fn rejects_non_png() {
        assert!(decode_png(b"not a png at all").is_err());
    }
}
