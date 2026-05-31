//! drdr-font — DrDrFont, the DrDrOS bitmap font renderer.
//!
//! Every glyph is an 8×16 monochrome bitmap stored as `[u8; 16]`: each
//! byte is one row, bit 7 is the leftmost pixel, bit 0 the rightmost.
//! A "1" bit paints `fg`, a "0" bit paints `bg`.
//!
//! DrDrFont now covers the **full printable ASCII range** (`0x20`..=`0x7E`)
//! — every letter, digit and symbol DrDrOS's UI needs. Anything outside
//! that range renders as the [`TOFU`] glyph (a hollow box) so a missing
//! character is visible rather than silently swallowed.
//!
//! The bitmaps are hand-authored — no external font files, no parsing.
//! To keep ~95 glyphs reviewable, each one is drawn as **pixel art**: an
//! array of 11 short text rows where `#` is an on-pixel and anything else
//! is off. [`art`] folds that into the real `[u8; 16]` cell at compile
//! time (it's a `const fn`), so there is zero runtime cost and the source
//! literally looks like the letter. Every pixel of DrDrOS's UI is still
//! something we drew on purpose.

use drdr_fb::{Framebuffer, Pixel};

/// Glyph width in pixels. The renderer hard-codes this — it's the width
/// of a single byte's bit pattern, and shifting strides by 8 is cheap.
pub const GLYPH_WIDTH: u32 = 8;
/// Glyph height in pixels. 16 rows give us enough vertical room for
/// descenders ('g', 'p') without crowding ascenders ('D', 'h').
pub const GLYPH_HEIGHT: u32 = 16;

// ─── Pixel-art DSL ───────────────────────────────────────────────────

/// Turn one art row (up to 8 chars) into one bitmap byte. `#` lights a
/// pixel; every other char (`.`, space, …) leaves it dark. Bit 7 is the
/// leftmost column, matching the wire layout `draw_glyph` expects.
const fn row(s: &str) -> u8 {
    let b = s.as_bytes();
    let mut out = 0u8;
    let mut i = 0;
    while i < 8 && i < b.len() {
        if b[i] == b'#' {
            out |= 1 << (7 - i as u8);
        }
        i += 1;
    }
    out
}

/// Fold 11 art rows into a full 8×16 cell. The art block is placed at
/// cell rows 4..=14, which gives a 4px top margin, a baseline at cell
/// row 10 (art row 6), and four rows (11..=14) of descender space for
/// `g j p q y`. Rows 0..=3 and 15 stay blank for inter-line spacing.
const fn art(rows: [&str; 11]) -> [u8; 16] {
    let mut g = [0u8; 16];
    let mut i = 0;
    while i < 11 {
        g[i + 4] = row(rows[i]);
        i += 1;
    }
    g
}

/// "Tofu" — the fallback glyph for characters we haven't drawn (i.e.
/// non-printable or non-ASCII). A hollow box: the universal "missing
/// glyph" sign, so gaps are obvious instead of invisible.
const TOFU: [u8; 16] = art([
    "######",
    "#....#",
    "#....#",
    "#....#",
    "#....#",
    "#....#",
    "######",
    "......",
    "......",
    "......",
    "......",
]);

/// Look up the bitmap for an ASCII byte. Returns [`TOFU`] for anything
/// outside printable ASCII (`0x20`..=`0x7E`).
pub fn glyph_for(c: u8) -> &'static [u8; 16] {
    match c {
        b' ' => &SPACE,
        b'!' => &BANG,
        b'"' => &QUOTE,
        b'#' => &HASH,
        b'$' => &DOLLAR,
        b'%' => &PERCENT,
        b'&' => &AMP,
        b'\'' => &APOS,
        b'(' => &LPAREN,
        b')' => &RPAREN,
        b'*' => &STAR,
        b'+' => &PLUS,
        b',' => &COMMA,
        b'-' => &DASH,
        b'.' => &DOT,
        b'/' => &SLASH,
        b'0' => &DIGIT_0,
        b'1' => &DIGIT_1,
        b'2' => &DIGIT_2,
        b'3' => &DIGIT_3,
        b'4' => &DIGIT_4,
        b'5' => &DIGIT_5,
        b'6' => &DIGIT_6,
        b'7' => &DIGIT_7,
        b'8' => &DIGIT_8,
        b'9' => &DIGIT_9,
        b':' => &COLON,
        b';' => &SEMI,
        b'<' => &LT,
        b'=' => &EQ,
        b'>' => &GT,
        b'?' => &QUESTION,
        b'@' => &AT,
        b'A' => &UP_A,
        b'B' => &UP_B,
        b'C' => &UP_C,
        b'D' => &UP_D,
        b'E' => &UP_E,
        b'F' => &UP_F,
        b'G' => &UP_G,
        b'H' => &UP_H,
        b'I' => &UP_I,
        b'J' => &UP_J,
        b'K' => &UP_K,
        b'L' => &UP_L,
        b'M' => &UP_M,
        b'N' => &UP_N,
        b'O' => &UP_O,
        b'P' => &UP_P,
        b'Q' => &UP_Q,
        b'R' => &UP_R,
        b'S' => &UP_S,
        b'T' => &UP_T,
        b'U' => &UP_U,
        b'V' => &UP_V,
        b'W' => &UP_W,
        b'X' => &UP_X,
        b'Y' => &UP_Y,
        b'Z' => &UP_Z,
        b'[' => &LBRACK,
        b'\\' => &BACKSLASH,
        b']' => &RBRACK,
        b'^' => &CARET,
        b'_' => &UNDERSCORE,
        b'`' => &BACKTICK,
        b'a' => &LO_A,
        b'b' => &LO_B,
        b'c' => &LO_C,
        b'd' => &LO_D,
        b'e' => &LO_E,
        b'f' => &LO_F,
        b'g' => &LO_G,
        b'h' => &LO_H,
        b'i' => &LO_I,
        b'j' => &LO_J,
        b'k' => &LO_K,
        b'l' => &LO_L,
        b'm' => &LO_M,
        b'n' => &LO_N,
        b'o' => &LO_O,
        b'p' => &LO_P,
        b'q' => &LO_Q,
        b'r' => &LO_R,
        b's' => &LO_S,
        b't' => &LO_T,
        b'u' => &LO_U,
        b'v' => &LO_V,
        b'w' => &LO_W,
        b'x' => &LO_X,
        b'y' => &LO_Y,
        b'z' => &LO_Z,
        b'{' => &LBRACE,
        b'|' => &PIPE,
        b'}' => &RBRACE,
        b'~' => &TILDE,
        _ => &TOFU,
    }
}

/// Draw one ASCII character at `(x, y)` (top-left corner of the cell).
/// Non-ASCII chars and characters outside [`glyph_for`]'s set render as
/// tofu. Out-of-bounds pixels are clipped by [`Framebuffer::put_pixel`].
pub fn draw_glyph(fb: &mut Framebuffer, x: u32, y: u32, c: char, fg: Pixel, bg: Pixel) {
    // Non-ASCII collapses to tofu; we don't decode UTF-8 yet.
    let byte = if c.is_ascii() { c as u8 } else { 0 };
    let glyph = glyph_for(byte);
    for (row, &bits) in glyph.iter().enumerate() {
        for col in 0..8u32 {
            // Bit 7 is the leftmost pixel, so shift = 7 - col.
            let lit = (bits >> (7 - col)) & 1 != 0;
            let color = if lit { fg } else { bg };
            fb.put_pixel(x + col, y + row as u32, color);
        }
    }
}

/// Draw an ASCII string left-to-right starting at `(x, y)`. Each glyph
/// occupies an 8-pixel-wide cell — no kerning, no proportional spacing.
/// Strings that run past the right edge are clipped per-pixel.
pub fn draw_text(fb: &mut Framebuffer, x: u32, y: u32, text: &str, fg: Pixel, bg: Pixel) {
    let mut cursor_x = x;
    for c in text.chars() {
        draw_glyph(fb, cursor_x, y, c, fg, bg);
        cursor_x = cursor_x.saturating_add(GLYPH_WIDTH);
    }
}

// ─── Anti-aliased rendering ──────────────────────────────────────────
//
// The 8×16 bitmap is crisp but visibly *pixelated* — its 1-bit edges
// staircase, which is exactly the "looks retro / blocky" complaint. We
// keep the same hand-drawn glyphs (no second font, no TTF parser) and
// soften their edges in software instead: treat the 1-bit cell as a
// coverage field and *resample* it. Where the bitmap transitions on→off
// the resample lands a partial value, so the renderer can blend the
// foreground a fraction of the way toward the background — a smooth,
// modern edge from the very same pixel art.
//
// Two paths:
//   - `draw_text_aa` / `draw_glyph_aa`: 1×, opaque, edges blended over a
//     known `bg`. Drop-in for body text and window chrome; works on any
//     framebuffer (it writes opaque colours, never reads back).
//   - `draw_glyph_scaled_aa`: the big logo/icon path. Supersamples the
//     glyph at the *target* (scaled) resolution so a 4× icon letter is a
//     smooth rounded shape, not a staircase of fat square pixels.

/// Coverage (0.0 = off, 1.0 = on) of the glyph at integer cell
/// `(col, row)`. Out-of-range is empty so edges fade to nothing.
#[inline]
fn glyph_bit(glyph: &[u8; 16], col: i32, row: i32) -> f32 {
    if row < 0 || row >= 16 || col < 0 || col >= 8 {
        return 0.0;
    }
    if glyph[row as usize] & (0x80u8 >> col as u8) != 0 {
        1.0
    } else {
        0.0
    }
}

/// Bilinear sample of the 1-bit glyph at fractional glyph-pixel position
/// `(fx, fy)` (pixel centres sit on integer coordinates). Between two
/// cells this returns the linear blend, which is what produces the soft
/// edge.
#[inline]
fn sample_bilinear(glyph: &[u8; 16], fx: f32, fy: f32) -> f32 {
    let x0 = fx.floor();
    let y0 = fy.floor();
    let tx = fx - x0;
    let ty = fy - y0;
    let (x0, y0) = (x0 as i32, y0 as i32);
    let c00 = glyph_bit(glyph, x0, y0);
    let c10 = glyph_bit(glyph, x0 + 1, y0);
    let c01 = glyph_bit(glyph, x0, y0 + 1);
    let c11 = glyph_bit(glyph, x0 + 1, y0 + 1);
    let top = c00 * (1.0 - tx) + c10 * tx;
    let bot = c01 * (1.0 - tx) + c11 * tx;
    top * (1.0 - ty) + bot * ty
}

/// Per-byte 1× anti-aliased coverage, computed once and cached. Each
/// entry is the 8×16 cell as `u8` alpha (0..255). Independent of colour,
/// so a single table serves every fg/bg pair.
fn aa_table() -> &'static [[u8; 128]] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<[u8; 128]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0u16..256)
            .map(|b| {
                let glyph = glyph_for(b as u8);
                let mut cov = [0u8; 128];
                for row in 0..16u32 {
                    for col in 0..8u32 {
                        // The glyph strokes are only 1px wide, so naively
                        // resampling would *dim* them (a 1px feature never
                        // reaches full coverage). Instead we keep every
                        // "on" pixel fully opaque — the letter stays crisp
                        // at full strength — and add a soft halo only on
                        // the surrounding "off" pixels. That fills the
                        // diagonal staircase steps without washing the text
                        // out: smoothing, not blurring.
                        if glyph_bit(glyph, col as i32, row as i32) > 0.5 {
                            cov[(row * 8 + col) as usize] = 255;
                            continue;
                        }
                        // 2×2 supersample of the neighbourhood (wider
                        // ±0.42 so a diagonal neighbour contributes), then
                        // damp it a little so halos stay subtle.
                        let mut c = 0.0f32;
                        for oy in [-0.42f32, 0.42] {
                            for ox in [-0.42f32, 0.42] {
                                c += sample_bilinear(glyph, col as f32 + ox, row as f32 + oy);
                            }
                        }
                        cov[(row * 8 + col) as usize] = (c * 0.25 * 0.85 * 255.0).round() as u8;
                    }
                }
                cov
            })
            .collect()
    })
}

/// Anti-aliased single glyph at `(x, y)`, edges blended over `bg`.
/// Writes opaque colours (no read-back), so it is correct on the real
/// device framebuffer as well as the canonical back buffer.
pub fn draw_glyph_aa(fb: &mut Framebuffer, x: u32, y: u32, c: char, fg: Pixel, bg: Pixel) {
    let byte = if c.is_ascii() { c as u8 } else { 0 };
    let cov = &aa_table()[byte as usize];
    for row in 0..16u32 {
        for col in 0..8u32 {
            let a = cov[(row * 8 + col) as usize];
            let color = match a {
                0 => bg,
                255 => fg,
                _ => bg.lerp(fg, a),
            };
            fb.put_pixel(x + col, y + row, color);
        }
    }
}

/// Anti-aliased string — the smooth counterpart to [`draw_text`].
pub fn draw_text_aa(fb: &mut Framebuffer, x: u32, y: u32, text: &str, fg: Pixel, bg: Pixel) {
    let mut cursor_x = x;
    for c in text.chars() {
        draw_glyph_aa(fb, cursor_x, y, c, fg, bg);
        cursor_x = cursor_x.saturating_add(GLYPH_WIDTH);
    }
}

/// Anti-aliased *scaled* glyph: render one character `scale`× larger with
/// smooth edges, alpha-composited (`blend_pixel`) over whatever is already
/// in `fb`. This is the big-logo path — desktop icons, the wallpaper
/// wordmark, the help-panel title — where the old pixel-replication looked
/// blocky. Only correct on the canonical back buffer (it composites).
pub fn draw_glyph_scaled_aa(fb: &mut Framebuffer, x: u32, y: u32, ch: char, fg: Pixel, scale: u32) {
    let scale = scale.max(1);
    let byte = if ch.is_ascii() { ch as u8 } else { 0 };
    let glyph = glyph_for(byte);
    let w = GLYPH_WIDTH * scale;
    let h = GLYPH_HEIGHT * scale;
    let inv = 1.0 / scale as f32;
    for oy in 0..h {
        for ox in 0..w {
            // 2×2 supersample inside this output pixel, mapped back into
            // glyph-pixel space (centres aligned).
            let mut c = 0.0f32;
            for sy in [0.25f32, 0.75] {
                for sx in [0.25f32, 0.75] {
                    let gx = (ox as f32 + sx) * inv - 0.5;
                    let gy = (oy as f32 + sy) * inv - 0.5;
                    c += sample_bilinear(glyph, gx, gy);
                }
            }
            let a = (c * 0.25 * 255.0).round() as u8;
            if a == 0 {
                continue;
            }
            fb.blend_pixel(x + ox, y + oy, Pixel::rgba(fg.r, fg.g, fg.b, a));
        }
    }
}

/// Anti-aliased scaled glyph blended over a *known* solid `bg` (opaque
/// writes, no read-back) — the variant for the boot splash, which paints
/// straight onto the device framebuffer where `blend_pixel` can't read.
pub fn draw_glyph_scaled_aa_over(
    fb: &mut Framebuffer,
    x: u32,
    y: u32,
    ch: char,
    fg: Pixel,
    bg: Pixel,
    scale: u32,
) {
    let scale = scale.max(1);
    let byte = if ch.is_ascii() { ch as u8 } else { 0 };
    let glyph = glyph_for(byte);
    let w = GLYPH_WIDTH * scale;
    let h = GLYPH_HEIGHT * scale;
    let inv = 1.0 / scale as f32;
    for oy in 0..h {
        for ox in 0..w {
            let mut c = 0.0f32;
            for sy in [0.25f32, 0.75] {
                for sx in [0.25f32, 0.75] {
                    let gx = (ox as f32 + sx) * inv - 0.5;
                    let gy = (oy as f32 + sy) * inv - 0.5;
                    c += sample_bilinear(glyph, gx, gy);
                }
            }
            let a = (c * 0.25 * 255.0).round() as u8;
            let color = match a {
                0 => continue,
                255 => fg,
                _ => bg.lerp(fg, a),
            };
            fb.put_pixel(x + ox, y + oy, color);
        }
    }
}

// ─── Glyph bitmaps ───────────────────────────────────────────────────
// Drawn on a 6-wide grid inside the 8-wide cell (1px side bearing). Read
// each block top-to-bottom: it *is* the letter. Caps fill art rows 0..6;
// lowercase x-height starts at row 2; descenders use rows 7..10.

const SPACE: [u8; 16] = [0; 16];

// Punctuation & symbols.
const BANG: [u8; 16] = art([
    "..#...", "..#...", "..#...", "..#...", "..#...",
    "......", "..#...", "......", "......", "......", "......",
]);
const QUOTE: [u8; 16] = art([
    ".#.#..", ".#.#..", ".#.#..", "......", "......",
    "......", "......", "......", "......", "......", "......",
]);
const HASH: [u8; 16] = art([
    ".#.#..", ".#.#..", "######", ".#.#..", "######",
    ".#.#..", ".#.#..", "......", "......", "......", "......",
]);
const DOLLAR: [u8; 16] = art([
    "..#...", ".####.", "#.#...", ".###..", "..#.#.",
    "####..", "..#...", "......", "......", "......", "......",
]);
const PERCENT: [u8; 16] = art([
    "##...#", "##..#.", "...#..", "..#...", ".#..##",
    "#..##.", "......", "......", "......", "......", "......",
]);
const AMP: [u8; 16] = art([
    ".##...", "#..#..", "#.#...", ".#....", "#.#.#.",
    "#..#..", ".##.#.", "......", "......", "......", "......",
]);
const APOS: [u8; 16] = art([
    "..#...", "..#...", "..#...", "......", "......",
    "......", "......", "......", "......", "......", "......",
]);
const LPAREN: [u8; 16] = art([
    "...#..", "..#...", ".#....", ".#....", ".#....",
    "..#...", "...#..", "......", "......", "......", "......",
]);
const RPAREN: [u8; 16] = art([
    ".#....", "..#...", "...#..", "...#..", "...#..",
    "..#...", ".#....", "......", "......", "......", "......",
]);
const STAR: [u8; 16] = art([
    "......", "#.#.#.", ".###..", "#####.", ".###..",
    "#.#.#.", "......", "......", "......", "......", "......",
]);
const PLUS: [u8; 16] = art([
    "......", "..#...", "..#...", "#####.", "..#...",
    "..#...", "......", "......", "......", "......", "......",
]);
const COMMA: [u8; 16] = art([
    "......", "......", "......", "......", "......",
    "......", ".##...", ".##...", "..#...", ".#....", "......",
]);
const DASH: [u8; 16] = art([
    "......", "......", "......", "#####.", "......",
    "......", "......", "......", "......", "......", "......",
]);
const DOT: [u8; 16] = art([
    "......", "......", "......", "......", "......",
    ".##...", ".##...", "......", "......", "......", "......",
]);
const SLASH: [u8; 16] = art([
    "....#.", "....#.", "...#..", "..#...", ".#....",
    "#.....", "#.....", "......", "......", "......", "......",
]);
const COLON: [u8; 16] = art([
    "......", "..#...", "..#...", "......", "......",
    "..#...", "..#...", "......", "......", "......", "......",
]);
const SEMI: [u8; 16] = art([
    "......", ".##...", ".##...", "......", "......",
    ".##...", ".##...", "..#...", ".#....", "......", "......",
]);
const LT: [u8; 16] = art([
    "...#..", "..#...", ".#....", "#.....", ".#....",
    "..#...", "...#..", "......", "......", "......", "......",
]);
const EQ: [u8; 16] = art([
    "......", "......", "#####.", "......", "#####.",
    "......", "......", "......", "......", "......", "......",
]);
const GT: [u8; 16] = art([
    ".#....", "..#...", "...#..", "....#.", "...#..",
    "..#...", ".#....", "......", "......", "......", "......",
]);
const QUESTION: [u8; 16] = art([
    ".###..", "#...#.", "....#.", "...#..", "..#...",
    "......", "..#...", "......", "......", "......", "......",
]);
const AT: [u8; 16] = art([
    ".####.", "#....#", "#.##.#", "#.##.#", "#.###.",
    "#.....", ".####.", "......", "......", "......", "......",
]);

// Digits.
const DIGIT_0: [u8; 16] = art([
    ".###..", "#...#.", "#..##.", "#.#.#.", "##..#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const DIGIT_1: [u8; 16] = art([
    "..#...", ".##...", "..#...", "..#...", "..#...",
    "..#...", ".###..", "......", "......", "......", "......",
]);
const DIGIT_2: [u8; 16] = art([
    ".###..", "#...#.", "....#.", "...#..", "..#...",
    ".#....", "#####.", "......", "......", "......", "......",
]);
const DIGIT_3: [u8; 16] = art([
    "####..", "....#.", "...#..", ".###..", "....#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const DIGIT_4: [u8; 16] = art([
    "...#..", "..##..", ".#.#..", "#..#..", "#####.",
    "...#..", "...#..", "......", "......", "......", "......",
]);
const DIGIT_5: [u8; 16] = art([
    "#####.", "#.....", "####..", "....#.", "....#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const DIGIT_6: [u8; 16] = art([
    ".###..", "#.....", "#.....", "####..", "#...#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const DIGIT_7: [u8; 16] = art([
    "#####.", "....#.", "...#..", "..#...", "..#...",
    "..#...", "..#...", "......", "......", "......", "......",
]);
const DIGIT_8: [u8; 16] = art([
    ".###..", "#...#.", "#...#.", ".###..", "#...#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const DIGIT_9: [u8; 16] = art([
    ".###..", "#...#.", "#...#.", ".####.", "....#.",
    "....#.", ".###..", "......", "......", "......", "......",
]);

// Uppercase A–Z.
const UP_A: [u8; 16] = art([
    ".###..", "#...#.", "#...#.", "#####.", "#...#.",
    "#...#.", "#...#.", "......", "......", "......", "......",
]);
const UP_B: [u8; 16] = art([
    "####..", "#...#.", "#...#.", "####..", "#...#.",
    "#...#.", "####..", "......", "......", "......", "......",
]);
const UP_C: [u8; 16] = art([
    ".###..", "#...#.", "#.....", "#.....", "#.....",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const UP_D: [u8; 16] = art([
    "###...", "#..#..", "#...#.", "#...#.", "#...#.",
    "#..#..", "###...", "......", "......", "......", "......",
]);
const UP_E: [u8; 16] = art([
    "#####.", "#.....", "#.....", "####..", "#.....",
    "#.....", "#####.", "......", "......", "......", "......",
]);
const UP_F: [u8; 16] = art([
    "#####.", "#.....", "#.....", "####..", "#.....",
    "#.....", "#.....", "......", "......", "......", "......",
]);
const UP_G: [u8; 16] = art([
    ".###..", "#...#.", "#.....", "#.##..", "#...#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const UP_H: [u8; 16] = art([
    "#...#.", "#...#.", "#...#.", "#####.", "#...#.",
    "#...#.", "#...#.", "......", "......", "......", "......",
]);
const UP_I: [u8; 16] = art([
    ".###..", "..#...", "..#...", "..#...", "..#...",
    "..#...", ".###..", "......", "......", "......", "......",
]);
const UP_J: [u8; 16] = art([
    "..###.", "...#..", "...#..", "...#..", "#..#..",
    "#..#..", ".##...", "......", "......", "......", "......",
]);
const UP_K: [u8; 16] = art([
    "#...#.", "#..#..", "#.#...", "##....", "#.#...",
    "#..#..", "#...#.", "......", "......", "......", "......",
]);
const UP_L: [u8; 16] = art([
    "#.....", "#.....", "#.....", "#.....", "#.....",
    "#.....", "#####.", "......", "......", "......", "......",
]);
const UP_M: [u8; 16] = art([
    "#...#.", "##.##.", "#.#.#.", "#.#.#.", "#...#.",
    "#...#.", "#...#.", "......", "......", "......", "......",
]);
const UP_N: [u8; 16] = art([
    "#...#.", "##..#.", "#.#.#.", "#.#.#.", "#..##.",
    "#...#.", "#...#.", "......", "......", "......", "......",
]);
const UP_O: [u8; 16] = art([
    ".###..", "#...#.", "#...#.", "#...#.", "#...#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const UP_P: [u8; 16] = art([
    "####..", "#...#.", "#...#.", "####..", "#.....",
    "#.....", "#.....", "......", "......", "......", "......",
]);
const UP_Q: [u8; 16] = art([
    ".###..", "#...#.", "#...#.", "#...#.", "#.#.#.",
    "#..#..", ".##.#.", "......", "......", "......", "......",
]);
const UP_R: [u8; 16] = art([
    "####..", "#...#.", "#...#.", "####..", "#.#...",
    "#..#..", "#...#.", "......", "......", "......", "......",
]);
const UP_S: [u8; 16] = art([
    ".####.", "#.....", "#.....", ".###..", "....#.",
    "....#.", "####..", "......", "......", "......", "......",
]);
const UP_T: [u8; 16] = art([
    "#####.", "..#...", "..#...", "..#...", "..#...",
    "..#...", "..#...", "......", "......", "......", "......",
]);
const UP_U: [u8; 16] = art([
    "#...#.", "#...#.", "#...#.", "#...#.", "#...#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const UP_V: [u8; 16] = art([
    "#...#.", "#...#.", "#...#.", "#...#.", ".#.#..",
    ".#.#..", "..#...", "......", "......", "......", "......",
]);
const UP_W: [u8; 16] = art([
    "#...#.", "#...#.", "#...#.", "#.#.#.", "#.#.#.",
    "##.##.", "#...#.", "......", "......", "......", "......",
]);
const UP_X: [u8; 16] = art([
    "#...#.", "#...#.", ".#.#..", "..#...", ".#.#..",
    "#...#.", "#...#.", "......", "......", "......", "......",
]);
const UP_Y: [u8; 16] = art([
    "#...#.", "#...#.", ".#.#..", "..#...", "..#...",
    "..#...", "..#...", "......", "......", "......", "......",
]);
const UP_Z: [u8; 16] = art([
    "#####.", "....#.", "...#..", "..#...", ".#....",
    "#.....", "#####.", "......", "......", "......", "......",
]);

// Lowercase a–z. x-height occupies art rows 2..6; ascenders use 0..6;
// descenders (g j p q y) run into rows 7..9.
const LO_A: [u8; 16] = art([
    "......", "......", ".###..", "....#.", ".####.",
    "#...#.", ".####.", "......", "......", "......", "......",
]);
const LO_B: [u8; 16] = art([
    "#.....", "#.....", "####..", "#...#.", "#...#.",
    "#...#.", "####..", "......", "......", "......", "......",
]);
const LO_C: [u8; 16] = art([
    "......", "......", ".###..", "#.....", "#.....",
    "#.....", ".###..", "......", "......", "......", "......",
]);
const LO_D: [u8; 16] = art([
    "....#.", "....#.", ".####.", "#...#.", "#...#.",
    "#...#.", ".####.", "......", "......", "......", "......",
]);
const LO_E: [u8; 16] = art([
    "......", "......", ".###..", "#...#.", "#####.",
    "#.....", ".###..", "......", "......", "......", "......",
]);
const LO_F: [u8; 16] = art([
    "..##..", ".#....", ".#....", "####..", ".#....",
    ".#....", ".#....", "......", "......", "......", "......",
]);
const LO_G: [u8; 16] = art([
    "......", "......", ".####.", "#...#.", "#...#.",
    ".####.", "....#.", "#...#.", ".###..", "......", "......",
]);
const LO_H: [u8; 16] = art([
    "#.....", "#.....", "####..", "#...#.", "#...#.",
    "#...#.", "#...#.", "......", "......", "......", "......",
]);
const LO_I: [u8; 16] = art([
    "..#...", "......", ".##...", "..#...", "..#...",
    "..#...", ".###..", "......", "......", "......", "......",
]);
const LO_J: [u8; 16] = art([
    "...#..", "......", "..##..", "...#..", "...#..",
    "...#..", "...#..", "#..#..", ".##...", "......", "......",
]);
const LO_K: [u8; 16] = art([
    "#.....", "#.....", "#..#..", "#.#...", "##....",
    "#.#...", "#..#..", "......", "......", "......", "......",
]);
const LO_L: [u8; 16] = art([
    ".##...", "..#...", "..#...", "..#...", "..#...",
    "..#...", ".###..", "......", "......", "......", "......",
]);
const LO_M: [u8; 16] = art([
    "......", "......", "##.#..", "#.#.#.", "#.#.#.",
    "#.#.#.", "#...#.", "......", "......", "......", "......",
]);
const LO_N: [u8; 16] = art([
    "......", "......", "####..", "#...#.", "#...#.",
    "#...#.", "#...#.", "......", "......", "......", "......",
]);
const LO_O: [u8; 16] = art([
    "......", "......", ".###..", "#...#.", "#...#.",
    "#...#.", ".###..", "......", "......", "......", "......",
]);
const LO_P: [u8; 16] = art([
    "......", "......", "####..", "#...#.", "#...#.",
    "####..", "#.....", "#.....", "#.....", "......", "......",
]);
const LO_Q: [u8; 16] = art([
    "......", "......", ".####.", "#...#.", "#...#.",
    ".####.", "....#.", "....#.", "....#.", "......", "......",
]);
const LO_R: [u8; 16] = art([
    "......", "......", "#.##..", "##..#.", "#.....",
    "#.....", "#.....", "......", "......", "......", "......",
]);
const LO_S: [u8; 16] = art([
    "......", "......", ".####.", "#.....", ".###..",
    "....#.", "####..", "......", "......", "......", "......",
]);
const LO_T: [u8; 16] = art([
    ".#....", ".#....", "####..", ".#....", ".#....",
    ".#....", "..##..", "......", "......", "......", "......",
]);
const LO_U: [u8; 16] = art([
    "......", "......", "#...#.", "#...#.", "#...#.",
    "#...#.", ".####.", "......", "......", "......", "......",
]);
const LO_V: [u8; 16] = art([
    "......", "......", "#...#.", "#...#.", "#...#.",
    ".#.#..", "..#...", "......", "......", "......", "......",
]);
const LO_W: [u8; 16] = art([
    "......", "......", "#...#.", "#.#.#.", "#.#.#.",
    "#.#.#.", ".#.#..", "......", "......", "......", "......",
]);
const LO_X: [u8; 16] = art([
    "......", "......", "#...#.", ".#.#..", "..#...",
    ".#.#..", "#...#.", "......", "......", "......", "......",
]);
const LO_Y: [u8; 16] = art([
    "......", "......", "#...#.", "#...#.", "#...#.",
    ".####.", "....#.", "#...#.", ".###..", "......", "......",
]);
const LO_Z: [u8; 16] = art([
    "......", "......", "#####.", "...#..", "..#...",
    ".#....", "#####.", "......", "......", "......", "......",
]);

// More symbols.
const LBRACK: [u8; 16] = art([
    ".###..", ".#....", ".#....", ".#....", ".#....",
    ".#....", ".###..", "......", "......", "......", "......",
]);
const BACKSLASH: [u8; 16] = art([
    "#.....", "#.....", ".#....", "..#...", "...#..",
    "....#.", "....#.", "......", "......", "......", "......",
]);
const RBRACK: [u8; 16] = art([
    ".###..", "...#..", "...#..", "...#..", "...#..",
    "...#..", ".###..", "......", "......", "......", "......",
]);
const CARET: [u8; 16] = art([
    "..#...", ".#.#..", "#...#.", "......", "......",
    "......", "......", "......", "......", "......", "......",
]);
const UNDERSCORE: [u8; 16] = art([
    "......", "......", "......", "......", "......",
    "......", "######", "......", "......", "......", "......",
]);
const BACKTICK: [u8; 16] = art([
    ".#....", "..#...", "......", "......", "......",
    "......", "......", "......", "......", "......", "......",
]);
const LBRACE: [u8; 16] = art([
    "..##..", "..#...", "..#...", ".#....", "..#...",
    "..#...", "..##..", "......", "......", "......", "......",
]);
const PIPE: [u8; 16] = art([
    "..#...", "..#...", "..#...", "..#...", "..#...",
    "..#...", "..#...", "......", "......", "......", "......",
]);
const RBRACE: [u8; 16] = art([
    ".##...", "..#...", "..#...", "...#..", "..#...",
    "..#...", ".##...", "......", "......", "......", "......",
]);
const TILDE: [u8; 16] = art([
    "......", "......", ".#..#.", "#.##.#", "#..#..",
    "......", "......", "......", "......", "......", "......",
]);

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Every printable ASCII char (0x20..=0x7E) must have a real glyph,
    /// not tofu — that's the whole promise this rewrite makes.
    #[test]
    fn full_printable_ascii_coverage() {
        let mut missing = Vec::new();
        for c in 0x20u8..=0x7E {
            if c == b' ' {
                continue; // space is legitimately all-blank
            }
            if glyph_for(c) == &TOFU || glyph_for(c) == &SPACE {
                missing.push(c as char);
            }
        }
        assert!(missing.is_empty(), "no glyph for: {missing:?}");
    }

    /// Bytes outside printable ASCII fall back to tofu.
    #[test]
    fn non_printable_is_tofu() {
        assert_eq!(glyph_for(0x07), &TOFU); // bell
        assert_eq!(glyph_for(0x7F), &TOFU); // DEL
        assert_eq!(glyph_for(0xFF), &TOFU);
    }

    /// `row` packs left-to-right with bit 7 = leftmost column.
    #[test]
    fn row_bit_order() {
        assert_eq!(row("#......."), 0b1000_0000);
        assert_eq!(row(".......#"), 0b0000_0001);
        assert_eq!(row("##......"), 0b1100_0000);
        assert_eq!(row("........"), 0);
    }

    /// `art` lands the block at cell rows 4..=14 (4px top margin).
    #[test]
    fn art_vertical_placement() {
        let g = art([
            "######", ".", ".", ".", ".", ".", ".", ".", ".", ".", ".",
        ]);
        assert_eq!(g[0..4], [0, 0, 0, 0]); // top margin blank
        assert_eq!(g[4], 0b1111_1100); // first art row
        assert_eq!(g[15], 0); // trailing blank
    }

    #[test]
    fn draw_text_smoke() {
        let mut fb = Framebuffer::in_memory(64, 16);
        // Should not panic and should light *some* pixels for "Ag".
        draw_text(&mut fb, 0, 0, "Ag", Pixel::WHITE, Pixel::BLACK);
    }

    #[test]
    fn aa_softens_edges() {
        // The whole point of the AA path: a diagonal-edged glyph must
        // produce at least one *partial* coverage value (a grey edge),
        // not just hard 0/255 like the bitmap. 'A' has diagonals.
        let cov = &aa_table()[b'A' as usize];
        let partials = cov.iter().filter(|&&a| a > 0 && a < 255).count();
        assert!(partials > 0, "AA produced no soft edge pixels");
        // …and a solid full-coverage interior must still exist (we didn't
        // blur the letter into mush).
        assert!(cov.iter().any(|&a| a == 255));
    }

    #[test]
    fn aa_blends_toward_background_on_an_edge() {
        // draw_glyph_aa writes opaque colours that, on an edge cell, lie
        // strictly between fg and bg — proof the blend is happening.
        let mut fb = Framebuffer::in_memory(8, 16);
        draw_glyph_aa(&mut fb, 0, 0, 'A', Pixel::WHITE, Pixel::BLACK);
        let mut saw_grey = false;
        for y in 0..16 {
            for x in 0..8 {
                let p = fb.get_pixel(x, y);
                if p.r > 0 && p.r < 255 {
                    saw_grey = true;
                }
            }
        }
        assert!(saw_grey, "no anti-aliased (grey) pixel found");
    }
}
