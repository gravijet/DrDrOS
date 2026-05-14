//! drdr-font — DrDrFont, the DrDrOS bitmap font renderer.
//!
//! Every glyph is an 8×16 monochrome bitmap stored as `[u8; 16]`: each
//! byte is one row, bit 7 is the leftmost pixel, bit 0 the rightmost.
//! A "1" bit paints `fg`, a "0" bit paints `bg`. Phase 1 ships a starter
//! set covering ASCII digits, the letters needed to spell "DrDrOS",
//! "booting", "Phase", and a handful of punctuation marks. Unknown
//! characters render as the [`TOFU`] glyph (an empty 8×16 frame) so
//! missing characters are visible rather than silently swallowed.
//!
//! The bitmaps are hand-authored — no external font files, no parsing.
//! That keeps drdr-font tiny, copyright-clean, and inspectable: every
//! pixel of DrDrOS's UI is something we drew on purpose.

use drdr_fb::{Framebuffer, Pixel};

/// Glyph width in pixels. The renderer hard-codes this — it's the width
/// of a single byte's bit pattern, and shifting strides by 8 is cheap.
pub const GLYPH_WIDTH: u32 = 8;
/// Glyph height in pixels. 16 rows give us enough vertical room for
/// descenders ('g', 'p') without crowding ascenders ('D', 'h').
pub const GLYPH_HEIGHT: u32 = 16;

/// "Tofu" — the fallback glyph for characters we haven't drawn yet.
/// A hollow 8×16 box, the universal sign for "missing glyph".
const TOFU: [u8; 16] = [
    0b11111111,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b10000001,
    0b11111111,
];

/// Look up the bitmap for an ASCII byte. Returns [`TOFU`] for anything
/// outside the hand-authored set so callers can render the result without
/// worrying about missing data.
pub fn glyph_for(c: u8) -> &'static [u8; 16] {
    match c {
        b' ' => &SPACE,
        b'.' => &DOT,
        b':' => &COLON,
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
        b'D' => &UPPER_D,
        b'O' => &UPPER_O,
        b'P' => &UPPER_P,
        b'R' => &UPPER_R,
        b'S' => &UPPER_S,
        b'a' => &LOWER_A,
        b'b' => &LOWER_B,
        b'e' => &LOWER_E,
        b'g' => &LOWER_G,
        b'h' => &LOWER_H,
        b'i' => &LOWER_I,
        b'n' => &LOWER_N,
        b'o' => &LOWER_O,
        b'r' => &LOWER_R,
        b's' => &LOWER_S,
        b't' => &LOWER_T,
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

// ─── Glyph bitmaps ───────────────────────────────────────────────────
// Each row is one byte, top to bottom. Read the binary literal left-to-
// right as if it were the pixel row: `0b10000001` = pixel on, six off,
// pixel on. Designs follow the classic VGA 8×16 style — chunky strokes,
// 1-pixel descenders, baseline at row 12.

const SPACE: [u8; 16] = [0; 16];

const DOT: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00011000,
    0b00011000,
    0b00000000,
    0b00000000,
    0b00000000,
];

const COLON: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00011000,
    0b00011000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00011000,
    0b00011000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_0: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_1: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00011000,
    0b00111000,
    0b01111000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b01111110,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_2: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b00000011,
    0b00000110,
    0b00001100,
    0b00011000,
    0b00110000,
    0b01100000,
    0b11000000,
    0b11111111,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_3: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b00000011,
    0b00000110,
    0b00011100,
    0b00000110,
    0b00000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_4: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000110,
    0b00001110,
    0b00011110,
    0b00110110,
    0b01100110,
    0b11000110,
    0b11111111,
    0b00000110,
    0b00000110,
    0b00000110,
    0b00000110,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_5: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b11111111,
    0b11000000,
    0b11000000,
    0b11000000,
    0b11111100,
    0b00000110,
    0b00000011,
    0b00000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_6: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11000000,
    0b11111100,
    0b11100110,
    0b11000011,
    0b11000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_7: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b11111111,
    0b00000011,
    0b00000110,
    0b00001100,
    0b00011000,
    0b00011000,
    0b00110000,
    0b00110000,
    0b00110000,
    0b00110000,
    0b00110000,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_8: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const DIGIT_9: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11000011,
    0b11000011,
    0b01100111,
    0b00111111,
    0b00000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const UPPER_D: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b11111100,
    0b01100110,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100110,
    0b11111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const UPPER_O: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const UPPER_P: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b11111100,
    0b01100110,
    0b01100011,
    0b01100011,
    0b01100110,
    0b01111100,
    0b01100000,
    0b01100000,
    0b01100000,
    0b01100000,
    0b11110000,
    0b00000000,
    0b00000000,
    0b00000000,
];

const UPPER_R: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b11111100,
    0b01100110,
    0b01100011,
    0b01100011,
    0b01100110,
    0b01111100,
    0b01101100,
    0b01100110,
    0b01100110,
    0b01100011,
    0b11110011,
    0b00000000,
    0b00000000,
    0b00000000,
];

const UPPER_S: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00111110,
    0b01100011,
    0b11000000,
    0b11000000,
    0b01110000,
    0b00111100,
    0b00001110,
    0b00000011,
    0b00000011,
    0b11000110,
    0b01111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_A: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00111100,
    0b00000110,
    0b00111110,
    0b01100110,
    0b11000110,
    0b11000110,
    0b11001110,
    0b01110110,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_B: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b11000000,
    0b01000000,
    0b01000000,
    0b01111100,
    0b01100110,
    0b01000011,
    0b01000011,
    0b01000011,
    0b01000011,
    0b01100110,
    0b11011100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_E: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11111111,
    0b11000000,
    0b11000000,
    0b01100011,
    0b00111110,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_G: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00111111,
    0b01100110,
    0b11000110,
    0b11000110,
    0b01111100,
    0b01000000,
    0b00111110,
    0b01000011,
    0b01000011,
    0b00111110,
    0b00000000,
];

const LOWER_H: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b11000000,
    0b01000000,
    0b01000000,
    0b01011100,
    0b01100110,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b11100111,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_I: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00011000,
    0b00011000,
    0b00000000,
    0b00111000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_N: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b11011100,
    0b01100110,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b01100011,
    0b11100111,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_O: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00111100,
    0b01100110,
    0b11000011,
    0b11000011,
    0b11000011,
    0b11000011,
    0b01100110,
    0b00111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_R: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b11011100,
    0b01110110,
    0b01100110,
    0b01100000,
    0b01100000,
    0b01100000,
    0b01100000,
    0b11110000,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_S: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00000000,
    0b00111110,
    0b01100011,
    0b01100000,
    0b00111100,
    0b00000110,
    0b00000011,
    0b11000110,
    0b01111100,
    0b00000000,
    0b00000000,
    0b00000000,
];

const LOWER_T: [u8; 16] = [
    0b00000000,
    0b00000000,
    0b00010000,
    0b00110000,
    0b00110000,
    0b11111100,
    0b00110000,
    0b00110000,
    0b00110000,
    0b00110000,
    0b00110000,
    0b00110011,
    0b00011110,
    0b00000000,
    0b00000000,
    0b00000000,
];
