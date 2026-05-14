//! drdr-ui — DrDrUI, the DrDrOS GUI framework (Tier 1).
//!
//! A small immediate-mode widget framework that paints directly onto a
//! [`drdr_fb::Framebuffer`] using [`drdr_font`]'s 8×16 bitmap glyphs.
//! No X11. No Wayland. No widget tree, no retained scene graph —
//! widgets are drawn in one pass per frame.
//!
//! Tier 1 ships the bare essentials:
//!
//!   - [`Rect`] — a region of pixels (x, y, w, h).
//!   - [`Theme`] — the palette every widget reads from.
//!   - [`Widget`] — a trait every drawable thing implements.
//!   - [`Label`], [`Button`], [`Frame`] — three primitive widgets.
//!   - [`VBox`], [`HBox`] — two layout containers.
//!
//! There's no input handling yet — Tier 2 wires a Linux evdev reader
//! that turns key presses into widget events. The pure draw pass shipped
//! here is enough for boot screens, splash windows, and static panels.
//!
//! Coordinate system: (0, 0) is the top-left pixel; +x goes right, +y
//! goes down — same as the framebuffer underneath.

use drdr_fb::{Framebuffer, Pixel};
use drdr_font::{draw_text, GLYPH_HEIGHT, GLYPH_WIDTH};

// Re-export framebuffer primitives so a single `use drdr_ui::*` is
// enough for an app to draw widgets onto a Framebuffer it built.
pub use drdr_fb::{Framebuffer as Fb, Pixel as Px};

// ─── Geometry ────────────────────────────────────────────────────────

/// Axis-aligned rectangle in pixel coordinates. Origin is the top-left
/// corner; width and height extend right and down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Shrink this rect by `pad` pixels on all sides. Returns a zero-size
    /// rect (with the same top-left) if padding would invert the size.
    pub fn shrink(self, pad: u32) -> Self {
        let pad2 = pad.saturating_mul(2);
        Self {
            x: self.x + pad.min(self.w),
            y: self.y + pad.min(self.h),
            w: self.w.saturating_sub(pad2),
            h: self.h.saturating_sub(pad2),
        }
    }
}

// ─── Theme ───────────────────────────────────────────────────────────

/// A flat color palette shared by every widget in a draw pass.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Default text color.
    pub fg: Pixel,
    /// Window / background fill.
    pub bg: Pixel,
    /// Highlight color for focused / pressed widgets.
    pub accent: Pixel,
    /// Text color when painted over `accent` (must contrast).
    pub accent_fg: Pixel,
    /// Color for 1-pixel borders.
    pub border: Pixel,
}

impl Theme {
    /// The default DrDrOS theme: deep blue background, soft white text,
    /// teal accent. Phase 5 introduces DrDrTheme for user customisation.
    pub const DRDR: Self = Self {
        fg: Pixel::rgb(0xE0, 0xE6, 0xF0),
        bg: Pixel::rgb(0x10, 0x18, 0x40),
        accent: Pixel::rgb(0x36, 0xA0, 0x9F),
        accent_fg: Pixel::WHITE,
        border: Pixel::rgb(0x4A, 0x5A, 0x80),
    };
}

// ─── Widget trait ────────────────────────────────────────────────────

/// Anything that can paint itself within a bounding rect.
///
/// `draw` is called once per frame with the rect the widget should fill.
/// Implementations are responsible for clipping themselves to that rect —
/// the framebuffer-level routines already clip per pixel, but widgets
/// should still honour `bounds` to avoid bleeding into siblings.
///
/// `preferred_size` returns the natural size in pixels — used by layout
/// containers to decide how much space to grant each child.
pub trait Widget {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme);
    fn preferred_size(&self) -> (u32, u32);
}

// ─── Primitive widgets ───────────────────────────────────────────────

/// A single line of text. No background fill — the parent paints first.
pub struct Label {
    pub text: String,
}

impl Label {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl Widget for Label {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme) {
        let text_h = GLYPH_HEIGHT;
        let y = bounds.y + bounds.h.saturating_sub(text_h) / 2;
        draw_text(fb, bounds.x, y, &self.text, theme.fg, theme.bg);
    }

    fn preferred_size(&self) -> (u32, u32) {
        (GLYPH_WIDTH * self.text.chars().count() as u32, GLYPH_HEIGHT)
    }
}

/// A bordered button. `focused = true` paints with the theme's accent.
pub struct Button {
    pub text: String,
    pub focused: bool,
}

impl Button {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), focused: false }
    }

    pub fn focused(mut self, on: bool) -> Self {
        self.focused = on;
        self
    }
}

impl Widget for Button {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme) {
        let (bg, fg) = if self.focused {
            (theme.accent, theme.accent_fg)
        } else {
            (theme.bg, theme.fg)
        };
        fb.fill_rect(bounds.x, bounds.y, bounds.w, bounds.h, bg);
        draw_border(fb, bounds, theme.border);

        let text_w = GLYPH_WIDTH * self.text.chars().count() as u32;
        let inner = bounds.shrink(2);
        let x = inner.x + inner.w.saturating_sub(text_w) / 2;
        let y = inner.y + inner.h.saturating_sub(GLYPH_HEIGHT) / 2;
        draw_text(fb, x, y, &self.text, fg, bg);
    }

    fn preferred_size(&self) -> (u32, u32) {
        let w = GLYPH_WIDTH * self.text.chars().count() as u32 + 12;
        let h = GLYPH_HEIGHT + 8;
        (w, h)
    }
}

/// A titled box containing one child widget.
pub struct Frame {
    pub title: String,
    pub child: Box<dyn Widget>,
    pub padding: u32,
}

impl Frame {
    pub fn new(title: impl Into<String>, child: Box<dyn Widget>) -> Self {
        Self { title: title.into(), child, padding: 8 }
    }
}

impl Widget for Frame {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme) {
        fb.fill_rect(bounds.x, bounds.y, bounds.w, bounds.h, theme.bg);
        draw_border(fb, bounds, theme.border);

        if !self.title.is_empty() && bounds.w > GLYPH_WIDTH * 4 {
            let tx = bounds.x + 6;
            let ty = bounds.y;
            let strip_w = GLYPH_WIDTH * self.title.chars().count() as u32 + 4;
            fb.fill_rect(tx.saturating_sub(2), ty, strip_w, 1, theme.bg);
            draw_text(fb, tx, ty.saturating_sub(GLYPH_HEIGHT / 4), &self.title, theme.fg, theme.bg);
        }

        let inner = bounds.shrink(self.padding);
        self.child.draw(fb, inner, theme);
    }

    fn preferred_size(&self) -> (u32, u32) {
        let (cw, ch) = self.child.preferred_size();
        let pad2 = self.padding * 2;
        (cw + pad2, ch + pad2)
    }
}

// ─── Layout containers ───────────────────────────────────────────────

/// Vertical stack. Children get their preferred height (clipped to the
/// remaining vertical space) and the container's full inner width.
pub struct VBox {
    pub children: Vec<Box<dyn Widget>>,
    pub gap: u32,
}

impl VBox {
    pub fn new() -> Self {
        Self { children: Vec::new(), gap: 4 }
    }

    pub fn with_gap(mut self, gap: u32) -> Self {
        self.gap = gap;
        self
    }

    pub fn push(mut self, child: Box<dyn Widget>) -> Self {
        self.children.push(child);
        self
    }
}

impl Default for VBox {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for VBox {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme) {
        let mut y = bounds.y;
        for (i, child) in self.children.iter().enumerate() {
            let (_, ph) = child.preferred_size();
            if y >= bounds.y + bounds.h {
                break;
            }
            let h = ph.min(bounds.y + bounds.h - y);
            child.draw(fb, Rect::new(bounds.x, y, bounds.w, h), theme);
            y = y.saturating_add(h);
            if i + 1 < self.children.len() {
                y = y.saturating_add(self.gap);
            }
        }
    }

    fn preferred_size(&self) -> (u32, u32) {
        let mut w = 0;
        let mut h = 0;
        for (i, child) in self.children.iter().enumerate() {
            let (cw, ch) = child.preferred_size();
            w = w.max(cw);
            h += ch;
            if i + 1 < self.children.len() {
                h += self.gap;
            }
        }
        (w, h)
    }
}

/// Horizontal stack. Children get their preferred width and the
/// container's full inner height.
pub struct HBox {
    pub children: Vec<Box<dyn Widget>>,
    pub gap: u32,
}

impl HBox {
    pub fn new() -> Self {
        Self { children: Vec::new(), gap: 8 }
    }

    pub fn with_gap(mut self, gap: u32) -> Self {
        self.gap = gap;
        self
    }

    pub fn push(mut self, child: Box<dyn Widget>) -> Self {
        self.children.push(child);
        self
    }
}

impl Default for HBox {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for HBox {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme) {
        let mut x = bounds.x;
        for (i, child) in self.children.iter().enumerate() {
            let (pw, _) = child.preferred_size();
            if x >= bounds.x + bounds.w {
                break;
            }
            let w = pw.min(bounds.x + bounds.w - x);
            child.draw(fb, Rect::new(x, bounds.y, w, bounds.h), theme);
            x = x.saturating_add(w);
            if i + 1 < self.children.len() {
                x = x.saturating_add(self.gap);
            }
        }
    }

    fn preferred_size(&self) -> (u32, u32) {
        let mut w = 0;
        let mut h = 0;
        for (i, child) in self.children.iter().enumerate() {
            let (cw, ch) = child.preferred_size();
            w += cw;
            h = h.max(ch);
            if i + 1 < self.children.len() {
                w += self.gap;
            }
        }
        (w, h)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Paint a 1-pixel rectangle outline along the inside edge of `r`.
fn draw_border(fb: &mut Framebuffer, r: Rect, color: Pixel) {
    if r.w == 0 || r.h == 0 {
        return;
    }
    fb.fill_rect(r.x, r.y, r.w, 1, color);
    fb.fill_rect(r.x, r.y + r.h - 1, r.w, 1, color);
    fb.fill_rect(r.x, r.y, 1, r.h, color);
    fb.fill_rect(r.x + r.w - 1, r.y, 1, r.h, color);
}
