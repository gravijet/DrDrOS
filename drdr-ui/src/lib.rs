//! drdr-ui — DrDrUI, the DrDrOS GUI framework (Tier 2).
//!
//! A small immediate-mode widget framework that paints directly onto a
//! [`drdr_fb::Framebuffer`] using [`drdr_font`]'s 8×16 bitmap glyphs.
//! No X11. No Wayland. No widget tree, no retained scene graph —
//! widgets are drawn in one pass per frame.
//!
//! Tier 2 (this file) ships:
//!
//!   - [`Rect`] — a region of pixels (x, y, w, h).
//!   - [`Theme`] — the palette every widget reads from.
//!   - [`Widget`] — a trait every drawable thing implements, with
//!     [`draw`](Widget::draw) and the new [`handle_event`](Widget::handle_event)
//!     + [`is_focusable`](Widget::is_focusable) hooks.
//!   - [`Label`], [`Button`] (with a `clicked` flag flipped by Enter /
//!     Space when focused), [`Frame`] (titled box).
//!   - [`VBox`], [`HBox`] — two layout containers.
//!   - [`input`] module with [`KeyReader`], [`Event`], [`KeyCode`],
//!     [`EventResponse`] — opens `/dev/input/eventN` and decodes raw
//!     evdev records into framework events.
//!   - [`vt`] module with [`VtGuard`] — takes the virtual terminal off
//!     the kernel (graphics mode + keyboard silenced) so fbcon stops
//!     fighting us for `/dev/fb0`, and restores it on exit.
//!
//! Coordinate system: (0, 0) is the top-left pixel; +x goes right, +y
//! goes down — same as the framebuffer underneath.

pub mod input;
pub mod vt;
pub mod window;

use drdr_fb::{Framebuffer, Pixel};
use drdr_font::{draw_text, GLYPH_HEIGHT, GLYPH_WIDTH};

pub use drdr_fb::{Framebuffer as Fb, Pixel as Px};
pub use input::{
    detect_keyboard, detect_mouse, detect_touch, Event, EventResponse, HubEvent, InputHub,
    KeyCode, KeyReader, MouseButton, MouseEvent, PointerReader,
};
pub use vt::VtGuard;
pub use window::{AppControl, Cell, Spawn, TextGrid, Window, WindowApp, WindowManager};

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

/// A flat palette of *semantic* color roles shared by every widget in a
/// draw pass. Widgets never hard-code colors — they ask the theme for
/// the role they need ("primary text", "a raised surface"), so a single
/// palette swap restyles the whole UI. This is the same idea as CSS
/// custom properties / design tokens, just resolved at draw time.
///
/// Two background depths matter on a dark UI: `bg` is the furthest-back
/// desktop/window fill, `surface` is one step *raised* (panels, controls
/// at rest) so elements read as layered rather than flat. Text comes in
/// two weights: `fg` for primary content and `muted` for secondary
/// chrome (frame titles, hints, disabled affordances).
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Furthest-back fill: desktop and window backgrounds.
    pub bg: Pixel,
    /// One step raised from `bg`: panels and controls at rest. Must be
    /// distinguishable from `bg` without a border.
    pub surface: Pixel,
    /// Primary text — high contrast against both `bg` and `surface`.
    pub fg: Pixel,
    /// Secondary text/chrome: titles, hints, disabled affordances.
    /// Still readable, but visibly recedes next to `fg`.
    pub muted: Pixel,
    /// Highlight for focused / pressed widgets and primary actions.
    pub accent: Pixel,
    /// Text painted over `accent` — must contrast with `accent`, not
    /// with `bg`. On a bright accent this is deliberately *dark*.
    pub accent_fg: Pixel,
    /// 1-pixel borders separating regions at rest.
    pub border: Pixel,
}

impl Theme {
    /// The default DrDrOS theme — "DrDr Midnight": a low-saturation navy
    /// dark scheme. Phase 5 introduces DrDrTheme for user customisation;
    /// this is the built-in every component falls back to.
    ///
    /// The palette is tuned for contrast, not just looks: see the
    /// `default_theme_meets_wcag_aa` test, which enforces a ≥ 4.5:1
    /// luminance contrast ratio (WCAG AA for body text) on the
    /// text-over-fill pairs a user actually reads.
    pub const DRDR: Self = Self {
        bg:        Pixel::rgb(0x0B, 0x0F, 0x1E),
        surface:   Pixel::rgb(0x18, 0x1F, 0x33),
        fg:        Pixel::rgb(0xE8, 0xEC, 0xF4),
        muted:     Pixel::rgb(0x97, 0xA0, 0xB8),
        accent:    Pixel::rgb(0x3D, 0xD0, 0xBC),
        accent_fg: Pixel::rgb(0x05, 0x10, 0x1A),
        border:    Pixel::rgb(0x2C, 0x36, 0x55),
    };

    /// "DrDrOS Fluent" — the modern, light scheme the desktop now boots
    /// into: a near-white workspace, true-white raised surfaces, and a
    /// Windows-11-style blue accent. The same semantic roles, so every
    /// existing widget restyles for free. Same contrast discipline as
    /// `DRDR` (see `theme_meets_wcag_aa`): primary text at AAA, chrome
    /// and accent labels at AA, `surface` provably lighter than `bg`.
    pub const FLUENT: Self = Self {
        bg:        Pixel::rgb(0xF3, 0xF3, 0xF3),
        surface:   Pixel::rgb(0xFF, 0xFF, 0xFF),
        fg:        Pixel::rgb(0x1B, 0x1B, 0x1B),
        muted:     Pixel::rgb(0x5C, 0x5C, 0x5C),
        accent:    Pixel::rgb(0x00, 0x67, 0xC0),
        accent_fg: Pixel::rgb(0xFF, 0xFF, 0xFF),
        border:    Pixel::rgb(0xD0, 0xD0, 0xD0),
    };

    /// The desktop default. Swapping this one constant reskins the whole
    /// OS — boot splash, every window, the taskbar and Start menu.
    pub const DEFAULT: Self = Self::FLUENT;

    /// Toggle between the light and dark schemes (the Settings app).
    pub fn toggled(&self) -> Self {
        if self.bg == Self::FLUENT.bg { Self::DRDR } else { Self::FLUENT }
    }

    /// A slightly raised step above `surface` for hover / pressed chrome
    /// (taskbar buttons, Start menu rows). Derived so a palette swap
    /// keeps it consistent: nudge toward the accent on light themes,
    /// toward white on dark ones.
    pub fn hover(&self) -> Pixel {
        if self.bg == Self::FLUENT.bg {
            self.bg.lerp(self.accent, 36)
        } else {
            self.surface.lerp(Pixel::WHITE, 28)
        }
    }
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
///
/// `handle_event` lets widgets react to input. The default just passes
/// the event through (`Passthrough`); only widgets that participate in
/// input override it. `is_focusable` declares whether the widget should
/// appear in the focus traversal order — Label says no, Button says yes.
pub trait Widget {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme);
    fn preferred_size(&self) -> (u32, u32);

    fn handle_event(&mut self, _event: &Event) -> EventResponse {
        EventResponse::Passthrough
    }

    fn is_focusable(&self) -> bool {
        false
    }
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
/// `clicked` flips to true on Enter / Space when focused — the app reads
/// and clears it on each frame.
pub struct Button {
    pub text: String,
    pub focused: bool,
    pub clicked: bool,
}

impl Button {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), focused: false, clicked: false }
    }

    pub fn focused(mut self, on: bool) -> Self {
        self.focused = on;
        self
    }

    /// One-shot read of the click flag — returns true at most once per
    /// activation, clearing the flag so subsequent frames don't repeat.
    pub fn take_click(&mut self) -> bool {
        let was = self.clicked;
        self.clicked = false;
        was
    }
}

impl Widget for Button {
    fn draw(&self, fb: &mut Framebuffer, bounds: Rect, theme: &Theme) {
        // At rest a button is a raised `surface` chip with a quiet
        // border; focused, it fills with `accent` and the border becomes
        // an accent focus-ring so the active control is unmistakable.
        let (bg, fg, border) = if self.focused {
            (theme.accent, theme.accent_fg, theme.accent)
        } else {
            (theme.surface, theme.fg, theme.border)
        };
        fb.fill_rect(bounds.x, bounds.y, bounds.w, bounds.h, bg);
        draw_border(fb, bounds, border);

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

    fn handle_event(&mut self, event: &Event) -> EventResponse {
        if !self.focused {
            return EventResponse::Passthrough;
        }
        match event {
            Event::Key(KeyCode::Enter | KeyCode::Space) => {
                self.clicked = true;
                EventResponse::Consumed
            }
            _ => EventResponse::Passthrough,
        }
    }

    fn is_focusable(&self) -> bool {
        true
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
            // A frame title is chrome, not content → muted, painted over
            // the frame's own `bg` fill so the glyph cells match.
            draw_text(fb, tx, ty.saturating_sub(GLYPH_HEIGHT / 4), &self.title, theme.muted, theme.bg);
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

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.x relative luminance of one channel (sRGB → linear light).
    fn channel(c: u8) -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }

    fn luminance(p: Pixel) -> f64 {
        0.2126 * channel(p.r) + 0.7152 * channel(p.g) + 0.0722 * channel(p.b)
    }

    /// WCAG contrast ratio of two colors: 1.0 (identical) … 21.0
    /// (black vs white). Body text needs ≥ 4.5 for AA.
    fn contrast(a: Pixel, b: Pixel) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The polish pass is only worth anything if the palette is actually
    /// legible. Lock that in: every text-over-fill pair a user reads
    /// must clear WCAG AA, and the dark-UI layering invariant
    /// (`surface` sits *above* `bg`) must hold.
    #[test]
    fn theme_meets_wcag_aa() {
        // Both shipped palettes — light and dark — obey the same rules,
        // so switching themes in Settings can never produce an illegible
        // desktop.
        for (name, t) in [("DRDR", Theme::DRDR), ("FLUENT", Theme::FLUENT)] {
            // Primary text is read at length → hold it to AAA (≥ 7:1).
            assert!(
                contrast(t.fg, t.bg) >= 7.0,
                "{name}: fg/bg = {}",
                contrast(t.fg, t.bg)
            );
            assert!(
                contrast(t.fg, t.surface) >= 7.0,
                "{name}: fg/surface = {}",
                contrast(t.fg, t.surface)
            );
            // Button label on the accent fill, and muted chrome on bg → AA.
            assert!(
                contrast(t.accent_fg, t.accent) >= 4.5,
                "{name}: accent_fg/accent = {}",
                contrast(t.accent_fg, t.accent)
            );
            assert!(
                contrast(t.muted, t.bg) >= 4.5,
                "{name}: muted/bg = {}",
                contrast(t.muted, t.bg)
            );

            // `surface` must be visibly raised above `bg` (lighter) and
            // not accidentally equal to it — that's what sells depth on a
            // flat framebuffer.
            assert!(
                luminance(t.surface) > luminance(t.bg),
                "{name}: surface not lighter than bg"
            );
            assert_ne!(t.surface, t.bg, "{name}");
            // Borders have to separate regions at rest.
            assert_ne!(t.border, t.surface, "{name}");
            assert_ne!(t.border, t.bg, "{name}");
        }
        // The Settings toggle round-trips between the two.
        assert_eq!(Theme::FLUENT.toggled().bg, Theme::DRDR.bg);
        assert_eq!(Theme::DRDR.toggled().bg, Theme::FLUENT.bg);
    }
}
