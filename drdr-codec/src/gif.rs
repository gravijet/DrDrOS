//! A from-scratch GIF decoder (87a / 89a, first frame).
//!
//! GIF stores indexed-colour pixels compressed with a variable-width
//! **LZW** code stream — a different beast from DEFLATE, so it gets its
//! own decoder here. We parse the logical-screen descriptor + colour
//! table, walk the block stream to the first image, LZW-decode its index
//! data, de-interlace if needed, and map indices through the palette into
//! the same flat RGBA8 [`Image`](crate::png::Image) the viewer paints.
//!
//! Scope: the first frame of a single- or multi-frame GIF (the still you
//! want to look at), honest about the rest. Transparency is honoured by
//! compositing the transparent index over white.

use crate::png::Image;

/// Decode a GIF's first frame into RGBA8, or a human-readable error.
pub fn decode_gif(data: &[u8]) -> Result<Image, String> {
    if data.len() < 13 || (&data[0..6] != b"GIF87a" && &data[0..6] != b"GIF89a") {
        return Err("gif: bad signature".into());
    }
    let screen_w = u16::from_le_bytes([data[6], data[7]]) as u32;
    let screen_h = u16::from_le_bytes([data[8], data[9]]) as u32;
    let packed = data[10];
    let gct_flag = packed & 0x80 != 0;
    let gct_size = 2usize << (packed & 0x07); // 2^(N+1)
    let mut pos = 13usize;

    let global_palette = if gct_flag {
        let p = read_palette(data, pos, gct_size)?;
        pos += gct_size * 3;
        p
    } else {
        Vec::new()
    };

    let mut transparent: Option<u8> = None;

    // Walk blocks until we hit the first image descriptor (0x2C).
    loop {
        let b = *data.get(pos).ok_or("gif: truncated before image")?;
        pos += 1;
        match b {
            0x3B => return Err("gif: no image data".into()),
            0x21 => {
                // Extension: a label, then length-prefixed sub-blocks.
                let label = *data.get(pos).ok_or("gif: truncated ext")?;
                pos += 1;
                if label == 0xF9 {
                    // Graphic Control Extension — carries transparency.
                    let size = *data.get(pos).ok_or("gif: bad gce")? as usize;
                    if size >= 4 && pos + 1 + size <= data.len() {
                        let flags = data[pos + 1];
                        if flags & 0x01 != 0 {
                            transparent = Some(data[pos + 4]);
                        }
                    }
                }
                pos = skip_sub_blocks(data, pos)?;
            }
            0x2C => {
                return decode_image_block(
                    data,
                    pos,
                    screen_w,
                    screen_h,
                    &global_palette,
                    transparent,
                );
            }
            other => return Err(format!("gif: unknown block 0x{other:02X}")),
        }
    }
}

fn read_palette(data: &[u8], pos: usize, entries: usize) -> Result<Vec<[u8; 3]>, String> {
    let end = pos + entries * 3;
    if end > data.len() {
        return Err("gif: truncated colour table".into());
    }
    Ok((0..entries)
        .map(|i| {
            let o = pos + i * 3;
            [data[o], data[o + 1], data[o + 2]]
        })
        .collect())
}

/// Advance `pos` past a chain of length-prefixed sub-blocks (terminated
/// by a zero-length block). `pos` must point at the first length byte.
fn skip_sub_blocks(data: &[u8], mut pos: usize) -> Result<usize, String> {
    loop {
        let len = *data.get(pos).ok_or("gif: truncated sub-block")? as usize;
        pos += 1;
        if len == 0 {
            return Ok(pos);
        }
        pos += len;
        if pos > data.len() {
            return Err("gif: sub-block overruns file".into());
        }
    }
}

/// Gather a chain of sub-blocks into one contiguous Vec (the LZW stream).
fn gather_sub_blocks(data: &[u8], mut pos: usize) -> Result<(Vec<u8>, usize), String> {
    let mut out = Vec::new();
    loop {
        let len = *data.get(pos).ok_or("gif: truncated data")? as usize;
        pos += 1;
        if len == 0 {
            return Ok((out, pos));
        }
        let end = pos + len;
        if end > data.len() {
            return Err("gif: data sub-block overruns file".into());
        }
        out.extend_from_slice(&data[pos..end]);
        pos = end;
    }
}

fn decode_image_block(
    data: &[u8],
    mut pos: usize,
    screen_w: u32,
    screen_h: u32,
    global_palette: &[[u8; 3]],
    transparent: Option<u8>,
) -> Result<Image, String> {
    if pos + 9 > data.len() {
        return Err("gif: truncated image descriptor".into());
    }
    let iw = u16::from_le_bytes([data[pos + 4], data[pos + 5]]) as u32;
    let ih = u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as u32;
    let ipacked = data[pos + 8];
    let lct_flag = ipacked & 0x80 != 0;
    let interlaced = ipacked & 0x40 != 0;
    let lct_size = 2usize << (ipacked & 0x07);
    pos += 9;

    let palette = if lct_flag {
        let p = read_palette(data, pos, lct_size)?;
        pos += lct_size * 3;
        p
    } else {
        global_palette.to_vec()
    };
    if palette.is_empty() {
        return Err("gif: no colour table".into());
    }

    let min_code = *data.get(pos).ok_or("gif: missing LZW code size")?;
    pos += 1;
    let (stream, _next) = gather_sub_blocks(data, pos)?;

    let w = if iw > 0 { iw } else { screen_w };
    let h = if ih > 0 { ih } else { screen_h };
    let indices = lzw_decode(&stream, min_code, (w * h) as usize)?;

    // Map indices → RGBA, de-interlacing into row-major order.
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (store_row, chunk) in indices.chunks(w as usize).enumerate() {
        let y = if interlaced {
            deinterlace_row(store_row as u32, h)
        } else {
            store_row as u32
        };
        if y >= h {
            continue;
        }
        for (x, &idx) in chunk.iter().enumerate() {
            let o = ((y * w + x as u32) * 4) as usize;
            if Some(idx) == transparent {
                // Composite the transparent index over white.
                rgba[o] = 0xFF;
                rgba[o + 1] = 0xFF;
                rgba[o + 2] = 0xFF;
                rgba[o + 3] = 0x00;
            } else {
                let c = palette.get(idx as usize).copied().unwrap_or([0, 0, 0]);
                rgba[o] = c[0];
                rgba[o + 1] = c[1];
                rgba[o + 2] = c[2];
                rgba[o + 3] = 0xFF;
            }
        }
    }
    Ok(Image { w, h, rgba })
}

/// The actual screen row a stored interlaced row maps to (the 4-pass
/// GIF interlace order: 0,8,…  4,12,…  2,6,…  1,3,…).
fn deinterlace_row(store_row: u32, h: u32) -> u32 {
    let pass1 = h.div_ceil(8); // rows starting at 0, step 8
    let pass2 = (h + 3) / 8; // rows starting at 4, step 8
    let pass3 = (h + 1) / 4; // rows starting at 2, step 4
    if store_row < pass1 {
        store_row * 8
    } else if store_row < pass1 + pass2 {
        (store_row - pass1) * 8 + 4
    } else if store_row < pass1 + pass2 + pass3 {
        (store_row - pass1 - pass2) * 4 + 2
    } else {
        (store_row - pass1 - pass2 - pass3) * 2 + 1
    }
}

/// LZW decode the GIF index stream (LSB-first, variable code width).
fn lzw_decode(stream: &[u8], min_code: u8, expected: usize) -> Result<Vec<u8>, String> {
    if min_code >= 12 {
        return Err("gif: bad LZW min code size".into());
    }
    let clear = 1usize << min_code;
    let eoi = clear + 1;

    // The dictionary: index → byte sequence. 0..clear are literals; the
    // two specials (clear, eoi) occupy clear and clear+1 as placeholders.
    let mut dict: Vec<Vec<u8>> = Vec::with_capacity(4096);
    let reset = |dict: &mut Vec<Vec<u8>>| {
        dict.clear();
        for i in 0..clear {
            dict.push(vec![i as u8]);
        }
        dict.push(Vec::new()); // clear
        dict.push(Vec::new()); // eoi
    };
    reset(&mut dict);
    let mut code_size = min_code as usize + 1;

    let mut out = Vec::with_capacity(expected);
    let mut bitpos = 0usize;
    let total_bits = stream.len() * 8;
    let mut prev: Option<usize> = None;

    let read_code = |bitpos: &mut usize, code_size: usize| -> Option<usize> {
        if *bitpos + code_size > total_bits {
            return None;
        }
        let mut code = 0usize;
        for i in 0..code_size {
            let bp = *bitpos + i;
            let bit = (stream[bp >> 3] >> (bp & 7)) & 1;
            code |= (bit as usize) << i;
        }
        *bitpos += code_size;
        Some(code)
    };

    while let Some(code) = read_code(&mut bitpos, code_size) {
        if code == clear {
            reset(&mut dict);
            code_size = min_code as usize + 1;
            prev = None;
            continue;
        }
        if code == eoi {
            break;
        }
        let entry = if code < dict.len() {
            dict[code].clone()
        } else if code == dict.len() {
            // The classic "KwKwK" case: code not yet in the table.
            let p = prev.ok_or("gif: LZW code before clear")?;
            let mut e = dict[p].clone();
            e.push(dict[p][0]);
            e
        } else {
            return Err("gif: LZW code out of range".into());
        };
        out.extend_from_slice(&entry);
        if let Some(p) = prev {
            let mut ne = dict[p].clone();
            ne.push(entry[0]);
            dict.push(ne);
            if dict.len() == (1 << code_size) && code_size < 12 {
                code_size += 1;
            }
        }
        prev = Some(code);
        if out.len() >= expected {
            break;
        }
    }

    out.resize(expected, 0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real 4×4 GIF written by Pillow (4-colour palette). The reference
    // pixel grid below is Pillow's own decode of the same bytes.
    const GIF: &[u8] = &[
        71, 73, 70, 56, 55, 97, 4, 0, 4, 0, 129, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 0, 0, 0,
        255, 44, 0, 0, 0, 0, 4, 0, 4, 0, 0, 8, 16, 0, 5, 8, 8, 16, 64, 32, 193, 1, 3, 0, 0, 64,
        168, 48, 32, 0, 59,
    ];

    #[test]
    fn decodes_a_real_pillow_gif() {
        let img = decode_gif(GIF).expect("gif should decode");
        assert_eq!((img.w, img.h), (4, 4));
        // Pillow's reference decode, row-major RGB.
        let expect: [(u8, u8, u8); 16] = [
            (255, 0, 0), (255, 0, 0), (0, 255, 0), (0, 255, 0),
            (255, 0, 0), (255, 0, 0), (0, 255, 0), (0, 255, 0),
            (0, 0, 255), (0, 0, 255), (255, 255, 0), (255, 255, 0),
            (0, 0, 255), (0, 0, 255), (255, 255, 0), (255, 255, 0),
        ];
        for (i, (r, g, b)) in expect.iter().enumerate() {
            assert_eq!(img.rgba[i * 4], *r, "pixel {i} R");
            assert_eq!(img.rgba[i * 4 + 1], *g, "pixel {i} G");
            assert_eq!(img.rgba[i * 4 + 2], *b, "pixel {i} B");
        }
    }

    #[test]
    fn rejects_non_gif() {
        assert!(decode_gif(b"not a gif at all").is_err());
    }
}
