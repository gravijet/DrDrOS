//! DrDrUI Tier 2 — overlapping windows, the app surface, and a small
//! stacking window manager with a modern (Windows-style) shell.
//!
//! The app-in-a-window mechanism (and why it isn't a terminal emulator)
//! ───────────────────────────────────────────────────────────────────
//! Tier 1's DrDrDesk handed the whole console to one app at a time. A
//! real desktop runs several apps in overlapping windows instead. The
//! usual way to put a text program in a window is a *terminal
//! emulator* — a PTY plus a parser for decades of ANSI/VT escape
//! sequences. DrDrOS doesn't copy that (the project rule is "build our
//! own equivalent", and a VT parser is a swamp).
//!
//! Instead we define a deliberately tiny contract:
//!
//!   - A windowed app is anything implementing [`WindowApp`]. It never
//!     touches the framebuffer, a TTY, or escape codes. It draws
//!     characters into a [`TextGrid`] it's handed, and reacts to a
//!     [`KeyCode`] / a click.
//!   - The window manager owns the grid, the chrome (title bar with
//!     minimise / maximise / close, drop shadow), and a desktop **shell**
//!     — a bottom taskbar with a Start button, a live clock, and one
//!     button per open window, plus a Start menu. None of that is an app;
//!     it's painted by the WM directly onto the framebuffer.
//!
//! So a "window" is a rectangle, a title, an app, and a character
//! buffer. Apps compose into the desktop for free; there is no
//! sub-process, no pseudo-terminal, nothing borrowed from xterm.

use crate::input::{KeyCode, MouseButton, MouseEvent};
use crate::{Rect, Theme};
use drdr_fb::{Framebuffer, Pixel};
use drdr_font::{GLYPH_HEIGHT, GLYPH_WIDTH, draw_glyph};

// ─── Layout constants ────────────────────────────────────────────────

/// 1px window frame all the way round.
const BORDER: u32 = 1;
/// Title bar height — taller than Tier 1 for a modern, roomy feel.
const TITLE_H: u32 = GLYPH_HEIGHT + 14;
/// Each window-control button (minimise / maximise / close) is a square
/// the height of the title bar. The close button is the right-most one,
/// so [`Window::close_rect`] stays a `TITLE_H` square at the far edge.
const BTN_W: u32 = TITLE_H;
/// Bottom taskbar height.
const TASKBAR_H: u32 = GLYPH_HEIGHT + 22;
/// Soft drop-shadow reach (px) — how far the shadow extends past the
/// window edge. Larger = softer, Win11/macOS look.
const SHADOW_REACH: i32 = 18;
/// Corner radius for windows, taskbar and Start menu — modern UIs use
/// 6-10px; 8 reads clearly without eating too many pixels on small
/// framebuffers.
const RADIUS: u32 = 8;

// ─── TextGrid — the surface apps draw into ───────────────────────────

/// One character cell: the glyph plus its own colours, so an app can
/// paint, say, a selected row in the theme accent without the window
/// manager knowing anything about the app's semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Pixel,
    pub bg: Pixel,
}

/// A `cols × rows` character buffer. This is the *entire* drawing API a
/// windowed app gets — no pixels, no fonts, no escape sequences.
pub struct TextGrid {
    pub cols: u32,
    pub rows: u32,
    cells: Vec<Cell>,
    fg: Pixel,
    bg: Pixel,
}

impl TextGrid {
    pub fn new(cols: u32, rows: u32, fg: Pixel, bg: Pixel) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell { ch: ' ', fg, bg }; (cols * rows) as usize],
            fg,
            bg,
        }
    }

    /// Re-fit to a new size (window resized). Content is reset — apps
    /// repaint every frame anyway, so there's nothing to preserve.
    fn resize(&mut self, cols: u32, rows: u32, fg: Pixel, bg: Pixel) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows && fg == self.fg && bg == self.bg {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.fg = fg;
        self.bg = bg;
        self.cells = vec![Cell { ch: ' ', fg, bg }; (cols * rows) as usize];
    }

    /// Clear every cell back to a space in the default colours.
    pub fn clear(&mut self) {
        let blank = Cell { ch: ' ', fg: self.fg, bg: self.bg };
        for c in &mut self.cells {
            *c = blank;
        }
    }

    /// The grid's default foreground / background (theme-derived).
    pub fn fg(&self) -> Pixel {
        self.fg
    }
    pub fn bg(&self) -> Pixel {
        self.bg
    }

    fn idx(&self, col: u32, row: u32) -> Option<usize> {
        if col < self.cols && row < self.rows {
            Some((row * self.cols + col) as usize)
        } else {
            None
        }
    }

    /// Set one cell. Out-of-bounds writes are ignored (clipped).
    pub fn put(&mut self, col: u32, row: u32, ch: char, fg: Pixel, bg: Pixel) {
        if let Some(i) = self.idx(col, row) {
            self.cells[i] = Cell { ch, fg, bg };
        }
    }

    /// Write a string left-to-right from `(col, row)`, clipped at the
    /// row's right edge. Returns the column just past the text.
    pub fn write(&mut self, col: u32, row: u32, s: &str, fg: Pixel, bg: Pixel) -> u32 {
        let mut c = col;
        for ch in s.chars() {
            if c >= self.cols {
                break;
            }
            self.put(c, row, ch, fg, bg);
            c += 1;
        }
        c
    }

    /// `write` in the grid's default colours — the common case.
    pub fn text(&mut self, col: u32, row: u32, s: &str) -> u32 {
        self.write(col, row, s, self.fg, self.bg)
    }

    /// Paint a whole row in a flat colour (e.g. a selected list item).
    pub fn fill_row(&mut self, row: u32, fg: Pixel, bg: Pixel) {
        for col in 0..self.cols {
            self.put(col, row, ' ', fg, bg);
        }
    }

    fn cell(&self, col: u32, row: u32) -> Cell {
        self.cells[(row * self.cols + col) as usize]
    }
}

// ─── WindowApp — what a windowed program implements ──────────────────

/// Whether an app wants to keep running or asks its window to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppControl {
    Continue,
    Close,
}

/// A request from an app to open another window (DrDrFiles opening a
/// file in the editor, the launcher/Start menu opening an app). The
/// window manager drains these after every input call and `open`s each.
pub struct Spawn {
    pub rect: Rect,
    pub app: Box<dyn WindowApp>,
}

/// A program that lives in a window. The contract stays tiny — see the
/// module docs for why it is *not* a terminal. Every method past
/// `title`/`render` has a default, so an app implements only what it uses.
pub trait WindowApp {
    /// Shown in the title bar; may change frame to frame (e.g. a clock).
    fn title(&self) -> String;

    /// Paint the current state into `grid`. Called once per redraw. The
    /// grid is already sized to the window's content area and cleared.
    fn render(&mut self, grid: &mut TextGrid);

    /// A key arrived while this window was focused.
    fn on_key(&mut self, key: KeyCode) -> AppControl {
        let _ = key;
        AppControl::Continue
    }

    /// A mouse click landed in the content area. `(col, row)` is the
    /// character cell (already translated from pixels by the WM);
    /// `double` is true on the second click of a double-click.
    fn on_click(&mut self, col: u32, row: u32, double: bool) -> AppControl {
        let _ = (col, row, double);
        AppControl::Continue
    }

    /// The pointer moved over this window's content area **with the
    /// left button held**. Used by drawing apps to support drag — the
    /// default ignores motion, so existing apps don't see new events.
    fn on_drag(&mut self, col: u32, row: u32) -> AppControl {
        let _ = (col, row);
        AppControl::Continue
    }

    /// Periodic heartbeat (~ every `InputHub` tick), even when not
    /// focused — lets a clock or a network panel keep itself current.
    fn on_tick(&mut self) -> AppControl {
        AppControl::Continue
    }

    /// Windows this app wants opened. The WM calls this right after
    /// `on_key`/`on_click`/`on_tick` and opens whatever is returned.
    fn take_spawns(&mut self) -> Vec<Spawn> {
        Vec::new()
    }
}

// ─── Window ──────────────────────────────────────────────────────────

/// One on-screen window: an outer rectangle (title bar + 1px frame +
/// content), the app inside it, the app's grid, and modern-shell state
/// (minimised, and the rect to restore to after un-maximising).
pub struct Window {
    /// Outer rect in screen pixels — what move/drag operates on.
    pub rect: Rect,
    app: Box<dyn WindowApp>,
    grid: TextGrid,
    /// Hidden to the taskbar; not drawn, not hit-tested.
    minimized: bool,
    /// `Some(prev)` while maximised — the rect to snap back to.
    restore: Option<Rect>,
}

impl Window {
    pub fn new(rect: Rect, app: Box<dyn WindowApp>) -> Self {
        let (cols, rows) = Self::grid_dims(rect);
        Self {
            rect,
            app,
            grid: TextGrid::new(cols, rows, Pixel::WHITE, Pixel::BLACK),
            minimized: false,
            restore: None,
        }
    }

    /// Pixel rect of the content area (inside the frame, below the bar).
    fn content_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + BORDER,
            self.rect.y + TITLE_H,
            self.rect.w.saturating_sub(BORDER * 2),
            self.rect.h.saturating_sub(TITLE_H + BORDER),
        )
    }

    /// How many character cells fit in `rect`'s content area.
    fn grid_dims(rect: Rect) -> (u32, u32) {
        let cw = rect.w.saturating_sub(BORDER * 2);
        let ch = rect.h.saturating_sub(TITLE_H + BORDER);
        ((cw / GLYPH_WIDTH).max(1), (ch / GLYPH_HEIGHT).max(1))
    }

    /// The draggable strip — the title bar minus the control buttons.
    fn title_rect(&self) -> Rect {
        let buttons = BTN_W * 3;
        Rect::new(
            self.rect.x,
            self.rect.y,
            self.rect.w.saturating_sub(buttons),
            TITLE_H,
        )
    }

    /// Close box — the right-most `TITLE_H` square (kept here so the
    /// long-standing close-box behaviour and its test are unchanged).
    fn close_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + self.rect.w.saturating_sub(BTN_W),
            self.rect.y,
            BTN_W,
            TITLE_H,
        )
    }

    /// Maximise / restore box — immediately left of the close box.
    fn max_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + self.rect.w.saturating_sub(BTN_W * 2),
            self.rect.y,
            BTN_W,
            TITLE_H,
        )
    }

    /// Minimise box — left of the maximise box.
    fn min_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + self.rect.w.saturating_sub(BTN_W * 3),
            self.rect.y,
            BTN_W,
            TITLE_H,
        )
    }

    /// Map a screen pixel to the content character cell under it.
    fn cell_at(&self, x: i32, y: i32) -> Option<(u32, u32)> {
        let c = self.content_rect();
        if !hit(c, x, y) {
            return None;
        }
        let col = (x - c.x as i32) as u32 / GLYPH_WIDTH;
        let row = (y - c.y as i32) as u32 / GLYPH_HEIGHT;
        let (cols, rows) = Self::grid_dims(self.rect);
        (col < cols && row < rows).then_some((col, row))
    }
}

/// Is `(x, y)` inside `r`? Free function so it's trivially unit-tested.
fn hit(r: Rect, x: i32, y: i32) -> bool {
    x >= r.x as i32
        && y >= r.y as i32
        && (x as i64) < (r.x + r.w) as i64
        && (y as i64) < (r.y + r.h) as i64
}

// ─── Cursor ──────────────────────────────────────────────────────────

/// The mouse pointer, hand-drawn like the font: `#` = white fill,
/// `o` = black outline (so it stays visible over any colour).
const CURSOR: [&str; 16] = [
    "o",
    "oo",
    "o#o",
    "o##o",
    "o###o",
    "o####o",
    "o#####o",
    "o######o",
    "o#######o",
    "o########o",
    "o####ooooo",
    "o#o##o",
    "oo o##o",
    "o   o##o",
    "     o##o",
    "      oo",
];

fn draw_cursor(fb: &mut Framebuffer, px: i32, py: i32) {
    let white = Pixel::WHITE;
    let black = Pixel::rgb(0, 0, 0);
    for (dy, row) in CURSOR.iter().enumerate() {
        for (dx, ch) in row.bytes().enumerate() {
            let color = match ch {
                b'#' => white,
                b'o' => black,
                _ => continue,
            };
            let x = px + dx as i32;
            let y = py + dy as i32;
            if x >= 0 && y >= 0 {
                fb.put_pixel(x as u32, y as u32, color);
            }
        }
    }
}

// ─── Wall clock (no deps) ────────────────────────────────────────────

/// `HH:MM` and `YYYY-MM-DD` from the system clock (UTC). We avoid a
/// date-time crate: it's a few lines of civil-calendar arithmetic
/// (Howard Hinnant's algorithm) over the Unix timestamp.
fn clock_now() -> (String, String) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m) = (tod / 3600, (tod % 3600) / 60);

    // days since 1970-01-01 → civil (y, m, d).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mon = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mon <= 2 { y + 1 } else { y };

    (
        format!("{h:02}:{m:02}"),
        format!("{year:04}-{mon:02}-{d:02}"),
    )
}

// ─── WindowManager ───────────────────────────────────────────────────

/// While the left button is held on a title bar, how far the pointer is
/// from the window's top-left — kept constant so the window tracks the
/// cursor without snapping.
struct Drag {
    grab_dx: i32,
    grab_dy: i32,
}

/// How long (ms) between two clicks still counts as a double-click, and
/// how far apart (px) they may land.
const DOUBLE_CLICK_MS: u128 = 450;
const DOUBLE_CLICK_SLOP: i32 = 6;

/// How close (px) the pointer must be to a screen edge while dragging
/// to trigger a snap preview. Wide enough to be discoverable, narrow
/// enough that you can drop a window near an edge without snapping.
const SNAP_EDGE: i32 = 20;

// ─── Desktop-icon layout ─────────────────────────────────────────────
//
// The desktop greets the user with a grid of large clickable icons,
// one per app — single click selects, double-click (or Enter) opens
// the app in a window. No auto-opened windows; if you close everything
// you land back on the same icon grid.

/// Pixel side length of an icon tile (the rounded square).
const ICON_TILE: u32 = 92;
/// Gap between icons (horizontal AND vertical), in px.
const ICON_GAP: u32 = 24;
/// Rounded-corner radius of the icon tile.
const ICON_RADIUS: u32 = 14;
/// Vertical padding above the icon grid (under the screen top).
const ICON_GRID_TOP: u32 = 64;

/// One icon on the desktop: a label, the app factory it should launch,
/// and an optional "glyph" character (drawn 4x scaled inside the tile).
pub struct DesktopIcon {
    pub label: String,
    pub glyph: char,
    /// Background tint (paints behind the glyph). A soft, distinct
    /// colour per app helps the eye scan the grid.
    pub tint: Pixel,
    pub factory: Box<dyn Fn() -> Spawn>,
}

/// A stacking window manager plus the desktop shell (taskbar + Start
/// menu). Windows are a back-to-front list; the top one is focused.
pub struct WindowManager {
    /// Index 0 = bottom of the stack, last = top = focused.
    windows: Vec<Window>,
    screen_w: u32,
    screen_h: u32,
    pointer_x: i32,
    pointer_y: i32,
    drag: Option<Drag>,
    /// While dragging, the rect this window will snap to on release if
    /// the pointer is in an edge zone. Cleared on every move + release.
    snap_preview: Option<Rect>,
    /// Whether the left mouse button is currently held. Set by
    /// MouseButton(Left, pressed), cleared on release. Lets motion
    /// events synthesise "drag inside a content area" for drawing apps
    /// without also delivering motion when the user is just hovering.
    mouse_down: bool,
    last_click: Option<(std::time::Instant, i32, i32)>,
    dirty: bool,
    /// Builds a fresh launcher window when the desktop empties.
    launcher: Option<Box<dyn Fn() -> Spawn>>,
    /// Start-menu entries: a label and a factory that builds the window.
    start_items: Vec<(String, Box<dyn Fn() -> Spawn>)>,
    start_open: bool,
    /// While true, the keyboard-shortcut help overlay is drawn over the
    /// desktop (any key / click dismisses it).
    help_open: bool,
    /// Cached `HH:MM` / date, refreshed on tick (taskbar clock).
    clock: String,
    date: String,
    /// Icons that live on the wallpaper. A click on one launches its
    /// app; a double-click on a selected icon does the same. The order
    /// is the painting/tab-order.
    desktop_icons: Vec<DesktopIcon>,
    /// Currently keyboard-selected icon (Tab cycles, Enter launches).
    /// `None` when no icon has the keyboard focus (default).
    icon_sel: Option<usize>,
    /// Index of the icon the pointer is hovering, for the hover lift.
    icon_hover: Option<usize>,
}

impl WindowManager {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        let (clock, date) = clock_now();
        Self {
            windows: Vec::new(),
            screen_w,
            screen_h,
            pointer_x: (screen_w / 2) as i32,
            pointer_y: (screen_h / 2) as i32,
            drag: None,
            snap_preview: None,
            mouse_down: false,
            last_click: None,
            dirty: true,
            launcher: None,
            start_items: Vec::new(),
            start_open: false,
            help_open: false,
            clock,
            date,
            desktop_icons: Vec::new(),
            icon_sel: None,
            icon_hover: None,
        }
    }

    /// Replace the desktop icon set. Each icon launches its `factory`
    /// when activated.
    pub fn set_desktop_icons(&mut self, icons: Vec<DesktopIcon>) {
        self.desktop_icons = icons;
        self.icon_sel = None;
        self.icon_hover = None;
        self.dirty = true;
    }

    /// How many icons fit per row at the current screen size. Used by
    /// both the layout and the hit-test, so they can never disagree.
    fn icons_per_row(&self) -> u32 {
        let avail = self.screen_w.saturating_sub(ICON_GAP * 2);
        let stride = ICON_TILE + ICON_GAP;
        ((avail + ICON_GAP) / stride).max(1)
    }

    /// Pixel rect of icon `i` in the grid, including the label strip
    /// below the tile (so a click on the label still counts).
    fn icon_rect(&self, i: usize) -> Rect {
        let cols = self.icons_per_row();
        let row = (i as u32) / cols;
        let col = (i as u32) % cols;
        let x = ICON_GAP + col * (ICON_TILE + ICON_GAP);
        let y = ICON_GRID_TOP + row * (ICON_TILE + ICON_GAP + GLYPH_HEIGHT + 8);
        Rect::new(x, y, ICON_TILE, ICON_TILE + GLYPH_HEIGHT + 8)
    }

    /// Index of the icon under `(x, y)`, if any. Misses on windows /
    /// taskbar are silently ignored.
    fn icon_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.desktop_icons.len()).find(|&i| hit(self.icon_rect(i), x, y))
    }

    /// Launch the icon at `i`: invoke the factory and open the window.
    fn launch_icon(&mut self, i: usize) {
        if let Some(icon) = self.desktop_icons.get(i) {
            let s = (icon.factory)();
            self.open(s.rect, s.app);
        }
    }

    /// The usable area above the taskbar (windows maximise into this).
    fn workarea(&self) -> Rect {
        Rect::new(
            0,
            0,
            self.screen_w,
            self.screen_h.saturating_sub(TASKBAR_H),
        )
    }

    fn taskbar_rect(&self) -> Rect {
        Rect::new(
            0,
            self.screen_h.saturating_sub(TASKBAR_H),
            self.screen_w,
            TASKBAR_H,
        )
    }

    fn start_btn_rect(&self) -> Rect {
        let tb = self.taskbar_rect();
        Rect::new(tb.x, tb.y, GLYPH_WIDTH * 8 + 16, tb.h)
    }

    /// Register the factory used to re-open the launcher when the
    /// desktop becomes empty.
    pub fn set_launcher(&mut self, f: impl Fn() -> Spawn + 'static) {
        self.launcher = Some(Box::new(f));
    }

    /// Populate the Start menu. Each entry is a label and a factory that
    /// builds its window when chosen.
    pub fn set_start_menu(
        &mut self,
        items: Vec<(String, Box<dyn Fn() -> Spawn>)>,
    ) {
        self.start_items = items;
    }

    /// True if the scene changed since the last [`draw`](Self::draw).
    pub fn needs_redraw(&self) -> bool {
        self.dirty
    }

    /// Add a window on top (it becomes focused).
    pub fn open(&mut self, rect: Rect, app: Box<dyn WindowApp>) {
        // Clamp into the workarea so a window can never open hidden
        // behind the taskbar or off a small real-hardware framebuffer.
        let wa = self.workarea();
        let w = rect.w.min(wa.w.max(1));
        let h = rect.h.min(wa.h.max(1));
        let x = rect.x.min(wa.w.saturating_sub(w));
        let y = rect.y.min(wa.h.saturating_sub(h));
        self.windows.push(Window::new(Rect::new(x, y, w, h), app));
        self.start_open = false;
        self.dirty = true;
    }

    /// Open everything an app queued via [`WindowApp::take_spawns`].
    fn drain_spawns(&mut self, idx: usize) {
        if let Some(w) = self.windows.get_mut(idx) {
            let spawns = w.app.take_spawns();
            for s in spawns {
                self.open(s.rect, s.app);
            }
        }
    }

    /// If the desktop just emptied, bring the launcher back — but only
    /// when no desktop icons are configured. With icons, an empty
    /// desktop IS the launcher: clicking an icon opens a fresh window.
    fn refill_if_empty(&mut self) {
        if self.windows.is_empty() && self.desktop_icons.is_empty() {
            if let Some(f) = &self.launcher {
                let s = f();
                self.windows.push(Window::new(s.rect, s.app));
            }
        }
        self.dirty = true;
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn pointer(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    pub fn screen(&self) -> (u32, u32) {
        (self.screen_w, self.screen_h)
    }

    /// Top-most *visible* window under `(x, y)`, front-to-back.
    fn hit_window(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.windows.len())
            .rev()
            .find(|&i| !self.windows[i].minimized && hit(self.windows[i].rect, x, y))
    }

    /// Move window `i` to the top of the stack (focus + raise).
    fn raise(&mut self, i: usize) {
        if i + 1 < self.windows.len() {
            let w = self.windows.remove(i);
            self.windows.push(w);
        }
    }

    /// Alt+Tab: send the current top to the bottom so the window behind
    /// it gains focus.
    fn cycle_focus(&mut self) {
        if self.windows.len() >= 2 {
            self.windows.rotate_right(1);
        }
    }

    /// Toggle a window between maximised (filling the workarea) and its
    /// previous floating rect.
    fn toggle_max(&mut self, i: usize) {
        let wa = self.workarea();
        if let Some(w) = self.windows.get_mut(i) {
            match w.restore.take() {
                Some(prev) => w.rect = prev,
                None => {
                    w.restore = Some(w.rect);
                    w.rect = wa;
                }
            }
        }
    }

    /// Compute the snap target when the pointer is in an edge zone during
    /// a title-bar drag. Returns the rect the focused window will jump to
    /// on release; `None` means "drop where you let go". Edge zones are
    /// SNAP_EDGE pixels wide.
    fn compute_snap(&self, x: i32, y: i32) -> Option<Rect> {
        let wa = self.workarea();
        // Top edge → maximise. Tested first so a slow upward drag that
        // also brushes the left edge still maximises (matches Win11).
        if y < SNAP_EDGE {
            return Some(wa);
        }
        let half_w = wa.w / 2;
        if x < SNAP_EDGE {
            return Some(Rect::new(0, 0, half_w, wa.h));
        }
        if x >= self.screen_w as i32 - SNAP_EDGE {
            return Some(Rect::new(half_w, 0, wa.w - half_w, wa.h));
        }
        None
    }

    /// Apply a snap rect to the focused window, remembering the pre-snap
    /// rect so a later double-click on the title bar can restore it.
    fn snap_focused(&mut self, target: Rect) {
        let Some(idx) = self.windows.len().checked_sub(1) else {
            return;
        };
        let w = &mut self.windows[idx];
        if w.restore.is_none() {
            w.restore = Some(w.rect);
        }
        w.rect = target;
        self.dirty = true;
    }

    /// Feed a key to the focused window. AltTab, the Super/Start key,
    /// the snap chords and F1 are handled by the WM; everything else
    /// goes to the top app.
    pub fn handle_key(&mut self, key: KeyCode) {
        self.dirty = true;
        // The help overlay is modal — any keystroke dismisses it.
        if self.help_open {
            self.help_open = false;
            return;
        }
        match key {
            KeyCode::AltTab => {
                self.start_open = false;
                self.cycle_focus();
                return;
            }
            KeyCode::Super => {
                // Tapping Super on its own toggles the Start menu, the
                // Windows / GNOME convention.
                self.start_open = !self.start_open;
                return;
            }
            KeyCode::Help => {
                self.help_open = true;
                return;
            }
            KeyCode::SnapLeft => {
                let wa = self.workarea();
                self.snap_focused(Rect::new(0, 0, wa.w / 2, wa.h));
                return;
            }
            KeyCode::SnapRight => {
                let wa = self.workarea();
                let hw = wa.w / 2;
                self.snap_focused(Rect::new(hw, 0, wa.w - hw, wa.h));
                return;
            }
            KeyCode::SnapUp => {
                let wa = self.workarea();
                self.snap_focused(wa);
                return;
            }
            KeyCode::SnapDown => {
                // Restore the pre-snap rect, or minimise if not snapped.
                if let Some(idx) = self.windows.len().checked_sub(1) {
                    let w = &mut self.windows[idx];
                    if let Some(prev) = w.restore.take() {
                        w.rect = prev;
                    } else {
                        w.minimized = true;
                    }
                }
                return;
            }
            _ => {}
        }
        if self.start_open {
            // The Start menu is keyboard-navigable too (Esc closes it).
            if key == KeyCode::Escape {
                self.start_open = false;
            }
            return;
        }
        let Some(idx) = self.windows.len().checked_sub(1) else {
            // No window has focus → keystrokes drive icon selection.
            self.handle_icon_key(key);
            return;
        };
        let ctrl = self.windows[idx].app.on_key(key);
        self.drain_spawns(idx);
        if ctrl == AppControl::Close {
            self.windows.remove(idx);
            self.refill_if_empty();
        }
    }

    /// Keyboard navigation for the desktop-icon grid (active only when
    /// no window has focus). Arrows + Tab move the selection; Enter
    /// opens the selected app.
    fn handle_icon_key(&mut self, key: KeyCode) {
        let n = self.desktop_icons.len();
        if n == 0 {
            return;
        }
        let cols = self.icons_per_row() as i32;
        let mut sel = self.icon_sel.unwrap_or(0) as i32;
        match key {
            KeyCode::Tab => sel = (sel + 1) % n as i32,
            KeyCode::BackTab => sel = (sel - 1).rem_euclid(n as i32),
            KeyCode::Right => sel = (sel + 1).min(n as i32 - 1),
            KeyCode::Left => sel = (sel - 1).max(0),
            KeyCode::Down => sel = (sel + cols).min(n as i32 - 1),
            KeyCode::Up => sel = (sel - cols).max(0),
            KeyCode::Home => sel = 0,
            KeyCode::End => sel = n as i32 - 1,
            KeyCode::Enter | KeyCode::Space => {
                let i = self.icon_sel.unwrap_or(0);
                if i < n {
                    self.launch_icon(i);
                }
                return;
            }
            _ => {}
        }
        self.icon_sel = Some(sel.clamp(0, n as i32 - 1) as usize);
    }

    /// Move the cursor to an already-clamped absolute screen position
    /// and carry any in-progress title-bar drag with it. Shared by
    /// relative mice (`Moved`) and touchscreens (`MovedTo`). While
    /// dragging, also recompute the snap preview so a drop near a
    /// screen edge can snap the window to that half / fill.
    fn move_pointer(&mut self, nx: i32, ny: i32) {
        self.pointer_x = nx;
        self.pointer_y = ny;
        if let Some(d) = &self.drag {
            if let Some(w) = self.windows.last_mut() {
                let wx = (self.pointer_x - d.grab_dx)
                    .clamp(0, self.screen_w as i32 - 1);
                let wy = (self.pointer_y - d.grab_dy)
                    .clamp(0, self.screen_h as i32 - TITLE_H as i32);
                w.rect.x = wx.max(0) as u32;
                w.rect.y = wy.max(0) as u32;
            }
            self.snap_preview = self.compute_snap(nx, ny);
        } else {
            self.snap_preview = None;
        }
        // Update icon hover only when no window blocks the pointer —
        // otherwise the hover ring chases the cursor through windows.
        let on_window = self.hit_window(nx, ny).is_some();
        let on_taskbar = hit(self.taskbar_rect(), nx, ny);
        self.icon_hover = if on_window || on_taskbar {
            None
        } else {
            self.icon_at(nx, ny)
        };
    }

    /// A left-press on the taskbar / Start menu. Returns true if the
    /// shell consumed it (so window hit-testing is skipped).
    fn shell_click(&mut self, x: i32, y: i32) -> bool {
        let on_taskbar = hit(self.taskbar_rect(), x, y);

        // The Start button toggles the menu in *either* direction —
        // handle it before anything else so a click on it while the
        // menu is open closes it (and doesn't get double-toggled).
        if on_taskbar && hit(self.start_btn_rect(), x, y) {
            self.start_open = !self.start_open;
            return true;
        }

        // With the menu open: a click inside launches that row; a click
        // anywhere else just dismisses it.
        if self.start_open {
            let menu = self.start_menu_rect();
            if hit(menu, x, y) {
                let row = ((y - menu.y as i32) as u32) / (GLYPH_HEIGHT + 6);
                if let Some((_, factory)) = self.start_items.get(row as usize) {
                    let s = factory();
                    self.open(s.rect, s.app); // also clears start_open
                }
                return true;
            }
            self.start_open = false;
            // Outside the menu and off the taskbar: let the click also
            // fall through to whatever window is underneath.
            return on_taskbar;
        }

        if !on_taskbar {
            return false;
        }

        // Taskbar window buttons (one slot per window after Start).
        // Falls through to the icon-click handler in `handle_mouse` if
        // the click was outside the taskbar.
        let slot_x0 = self.start_btn_rect().w + 4;
        let slot_w = self.taskbar_slot_w();
        if x as u32 >= slot_x0 {
            let idx = ((x as u32 - slot_x0) / slot_w) as usize;
            if idx < self.windows.len() {
                let focused = idx + 1 == self.windows.len();
                if self.windows[idx].minimized {
                    self.windows[idx].minimized = false;
                    self.raise(idx);
                } else if focused {
                    self.windows[idx].minimized = true;
                } else {
                    self.raise(idx);
                }
                return true;
            }
        }
        true // empty taskbar space still swallows the click
    }

    fn taskbar_slot_w(&self) -> u32 {
        let avail = self
            .screen_w
            .saturating_sub(self.start_btn_rect().w + 4 + GLYPH_WIDTH * 12);
        let n = self.windows.len().max(1) as u32;
        (avail / n).clamp(GLYPH_WIDTH * 6, GLYPH_WIDTH * 22)
    }

    fn start_menu_rect(&self) -> Rect {
        let rows = self.start_items.len().max(1) as u32;
        let h = rows * (GLYPH_HEIGHT + 6) + 12;
        let w = GLYPH_WIDTH * 26;
        Rect::new(
            0,
            self.screen_h.saturating_sub(TASKBAR_H + h),
            w.min(self.screen_w),
            h,
        )
    }

    /// Feed a pointer event.
    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        self.dirty = true;
        // The help overlay is modal — any click dismisses it (motion
        // does not, so the user can scan the legend without flicker).
        if self.help_open {
            if matches!(
                ev,
                MouseEvent::Button { button: MouseButton::Left, pressed: true }
            ) {
                self.help_open = false;
                return;
            }
        }
        match ev {
            MouseEvent::Moved { dx, dy } => {
                let nx = (self.pointer_x + dx).clamp(0, self.screen_w as i32 - 1);
                let ny = (self.pointer_y + dy).clamp(0, self.screen_h as i32 - 1);
                self.move_pointer(nx, ny);
                self.deliver_drag(nx, ny);
            }
            MouseEvent::MovedTo { x, y } => {
                let nx = x.clamp(0, self.screen_w as i32 - 1);
                let ny = y.clamp(0, self.screen_h as i32 - 1);
                self.move_pointer(nx, ny);
                self.deliver_drag(nx, ny);
            }
            MouseEvent::Button { button: MouseButton::Left, pressed: true } => {
                self.mouse_down = true;
                let (x, y) = (self.pointer_x, self.pointer_y);
                let now = std::time::Instant::now();
                let double = matches!(self.last_click, Some((t, lx, ly))
                    if now.duration_since(t).as_millis() <= DOUBLE_CLICK_MS
                        && (lx - x).abs() <= DOUBLE_CLICK_SLOP
                        && (ly - y).abs() <= DOUBLE_CLICK_SLOP);
                self.last_click = Some((now, x, y));

                if self.shell_click(x, y) {
                    return;
                }

                if let Some(i) = self.hit_window(x, y) {
                    self.raise(i);
                    let idx = self.windows.len() - 1;
                    let top = &self.windows[idx];
                    if hit(top.close_rect(), x, y) {
                        self.windows.remove(idx);
                        self.refill_if_empty();
                    } else if hit(top.min_rect(), x, y) {
                        self.windows[idx].minimized = true;
                    } else if hit(top.max_rect(), x, y) {
                        self.toggle_max(idx);
                    } else if hit(top.title_rect(), x, y) {
                        if double {
                            self.toggle_max(idx);
                        } else {
                            let r = top.rect;
                            self.drag = Some(Drag {
                                grab_dx: x - r.x as i32,
                                grab_dy: y - r.y as i32,
                            });
                        }
                    } else if let Some((col, row)) = top.cell_at(x, y) {
                        let ctrl = self.windows[idx].app.on_click(col, row, double);
                        self.drain_spawns(idx);
                        if ctrl == AppControl::Close {
                            self.windows.remove(idx);
                            self.refill_if_empty();
                        }
                    }
                } else if let Some(i) = self.icon_at(x, y) {
                    // Single click selects; a second click within the
                    // double-click window opens the app — same idiom
                    // as every modern file manager.
                    self.icon_sel = Some(i);
                    if double {
                        self.launch_icon(i);
                    }
                } else {
                    // Clicking the empty desktop closes the Start menu
                    // and deselects any selected icon.
                    self.start_open = false;
                    self.icon_sel = None;
                }
            }
            MouseEvent::Button { button: MouseButton::Left, pressed: false } => {
                // If the drag ended over a snap zone, jump the window
                // to the snap target instead of leaving it where it lay.
                if self.drag.is_some() {
                    if let Some(target) = self.snap_preview.take() {
                        self.snap_focused(target);
                    }
                }
                self.snap_preview = None;
                self.drag = None;
                self.mouse_down = false;
            }
            _ => {}
        }
    }

    /// While the left button is held, hand pointer motion over the
    /// focused window's content area to its app as `on_drag`. The
    /// title-bar drag (window move) and the start-menu state both
    /// suppress this — you never start a paint stroke while moving a
    /// window.
    fn deliver_drag(&mut self, x: i32, y: i32) {
        if !self.mouse_down || self.drag.is_some() {
            return;
        }
        let Some(idx) = self.windows.len().checked_sub(1) else {
            return;
        };
        let Some((col, row)) = self.windows[idx].cell_at(x, y) else {
            return;
        };
        let ctrl = self.windows[idx].app.on_drag(col, row);
        self.drain_spawns(idx);
        if ctrl == AppControl::Close {
            self.windows.remove(idx);
            self.refill_if_empty();
        }
    }

    /// Heartbeat: refresh the clock and tick *every* window.
    pub fn tick(&mut self) {
        self.dirty = true;
        let (c, d) = clock_now();
        self.clock = c;
        self.date = d;
        let mut i = 0;
        while i < self.windows.len() {
            let ctrl = self.windows[i].app.on_tick();
            self.drain_spawns(i);
            if ctrl == AppControl::Close {
                self.windows.remove(i);
            } else {
                i += 1;
            }
        }
        if self.windows.is_empty() {
            self.refill_if_empty();
        }
    }

    /// Paint the whole desktop: wallpaper, window shadows + windows
    /// back-to-front, the taskbar, the Start menu, then the cursor.
    pub fn draw(&mut self, fb: &mut Framebuffer, theme: &Theme) {
        // Hover-aware chrome (control buttons) needs the pointer; stash
        // it for the free `draw_window` to read. WM is single-threaded.
        set_fb_pointer((self.pointer_x, self.pointer_y));
        let (w, h) = (fb.width, fb.height);

        // Wallpaper: a soft vertical gradient + a faint centred wordmark
        // so a bare desktop reads as DrDrOS, not a crash.
        let top = theme.bg;
        let bot = theme.bg.lerp(theme.accent, 22);
        fb.fill_rect_v(0, 0, w, h, top, bot);
        // Soft DrDrOS wordmark watermark only when nothing else fills
        // the wallpaper — once the user has icons the mark becomes
        // visual noise, so we skip it.
        if self.desktop_icons.is_empty() {
            let mark = "DrDrOS";
            let mw = GLYPH_WIDTH * 2 * mark.len() as u32;
            draw_text_2x(
                fb,
                w.saturating_sub(mw) / 2,
                h / 3,
                mark,
                theme.bg.lerp(theme.fg, 40),
            );
        }

        // Desktop icons sit BELOW window shadows / windows, so an
        // opened window covers them cleanly.
        self.draw_desktop_icons(fb, theme);

        let last = self.windows.len().saturating_sub(1);
        for i in 0..self.windows.len() {
            if self.windows[i].minimized {
                continue;
            }
            // Soft drop shadow first, then the window over it.
            draw_shadow(fb, self.windows[i].rect);
            let focused = i == last;
            draw_window(fb, &mut self.windows[i], theme, focused);
        }

        // Snap preview, drawn UNDER the cursor and chrome so the user
        // sees the target landing zone without losing sight of the
        // dragged window.
        if let Some(p) = self.snap_preview {
            let glow = Pixel::rgba(theme.accent.r, theme.accent.g, theme.accent.b, 70);
            fb.shade_rect(p.x, p.y, p.w, p.h, glow);
            // 2-px accent border so the preview reads as an active target.
            fb.fill_rect(p.x, p.y, p.w, 2, theme.accent);
            fb.fill_rect(p.x, p.y + p.h.saturating_sub(2), p.w, 2, theme.accent);
            fb.fill_rect(p.x, p.y, 2, p.h, theme.accent);
            fb.fill_rect(p.x + p.w.saturating_sub(2), p.y, 2, p.h, theme.accent);
        }

        self.draw_taskbar(fb, theme);
        if self.start_open {
            self.draw_start_menu(fb, theme);
        }

        if self.help_open {
            self.draw_help_overlay(fb, theme);
        }

        draw_cursor(fb, self.pointer_x, self.pointer_y);
        self.dirty = false;
    }

    /// Centred keyboard-shortcut legend over a dimmed wallpaper. Any
    /// key / click dismisses it (handle_key / handle_mouse).
    fn draw_help_overlay(&self, fb: &mut Framebuffer, theme: &Theme) {
        // Dim the desktop so the panel reads as a focused modal.
        fb.shade_rect(0, 0, self.screen_w, self.screen_h, Pixel::rgba(0, 0, 0, 120));

        let lines: &[(&str, &str)] = &[
            ("Alt + Tab",        "Cycle window focus"),
            ("Super",            "Open / close the Start menu"),
            ("Super + Left",     "Snap window to left half"),
            ("Super + Right",    "Snap window to right half"),
            ("Super + Up",       "Maximise window"),
            ("Super + Down",     "Restore / minimise window"),
            ("F1",               "Show / hide this help"),
            ("",                 ""),
            ("Mouse",            "Drag title bar to move"),
            ("",                 "Drag to edges to snap"),
            ("",                 "Double-click title to maximise"),
            ("",                 "Click [x] to close"),
        ];

        let pad = 14u32;
        let key_w = 18 * GLYPH_WIDTH;
        let desc_w = 32 * GLYPH_WIDTH;
        let row_h = GLYPH_HEIGHT + 4;
        let header_h = GLYPH_HEIGHT * 2 + 12;
        let w = (key_w + desc_w + pad * 3).min(self.screen_w.saturating_sub(40));
        let h = header_h + lines.len() as u32 * row_h + pad * 2;
        let x = self.screen_w.saturating_sub(w) / 2;
        let y = self.screen_h.saturating_sub(h) / 2;

        draw_shadow(fb, Rect::new(x, y, w, h));
        fb.fill_round_rect(x, y, w, h, RADIUS, theme.surface);
        // Header band
        fb.fill_round_rect_corners(x, y, w, header_h, RADIUS, RADIUS, 0, 0, theme.accent);
        let title = "DrDrOS shortcuts";
        let title_w = GLYPH_WIDTH * title.len() as u32 * 2;
        // Big centred title
        for (i, ch) in title.bytes().enumerate() {
            let glyph = drdr_font::glyph_for(ch);
            let gx = x + (w.saturating_sub(title_w)) / 2 + i as u32 * GLYPH_WIDTH * 2;
            let gy = y + 8;
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8u32 {
                    if *bits & (0x80u8 >> col) != 0 {
                        fb.fill_rect(gx + col * 2, gy + row as u32 * 2, 2, 2, theme.accent_fg);
                    }
                }
            }
        }

        let mut ry = y + header_h + pad;
        for (key, desc) in lines {
            drdr_font::draw_text(fb, x + pad, ry, key, theme.accent, theme.surface);
            drdr_font::draw_text(fb, x + pad + key_w + pad, ry, desc, theme.fg, theme.surface);
            ry += row_h;
        }

        // Footer hint
        let hint = "press any key to close";
        let hw = GLYPH_WIDTH * hint.len() as u32;
        drdr_font::draw_text(
            fb,
            x + (w.saturating_sub(hw)) / 2,
            y + h.saturating_sub(GLYPH_HEIGHT + 6),
            hint,
            theme.muted,
            theme.surface,
        );
    }

    /// Paint the desktop icon grid: a rounded tile per app with a 4×
    /// scaled glyph inside and a label below. Selected = accent ring;
    /// hovered = a slightly raised surface fill.
    fn draw_desktop_icons(&self, fb: &mut Framebuffer, theme: &Theme) {
        for (i, icon) in self.desktop_icons.iter().enumerate() {
            let r = self.icon_rect(i);
            // Don't paint icons that would land off-screen on a small
            // panel (rare; we still want a clean clip).
            if r.x + r.w > self.screen_w || r.y + ICON_TILE > self.screen_h {
                continue;
            }
            let tile_x = r.x;
            let tile_y = r.y;
            let selected = self.icon_sel == Some(i);
            let hovered = self.icon_hover == Some(i);

            // Soft drop shadow under every tile — gives the grid a
            // floating "card" feel matching the window shadows.
            fb.drop_shadow(
                tile_x as i32,
                tile_y as i32,
                ICON_TILE as i32,
                ICON_TILE as i32,
                10,
                70,
            );

            // Subtle hover/selection halo behind the tile.
            if selected || hovered {
                let halo = if selected { 90 } else { 50 };
                fb.shade_rect(
                    tile_x.saturating_sub(6),
                    tile_y.saturating_sub(6),
                    ICON_TILE + 12,
                    ICON_TILE + 12,
                    Pixel::rgba(theme.accent.r, theme.accent.g, theme.accent.b, halo),
                );
            }

            // Tile body — coloured background derived from the app's
            // tint with a translucent overlay so it reads as a button.
            let body = icon.tint;
            fb.fill_round_rect(tile_x, tile_y, ICON_TILE, ICON_TILE, ICON_RADIUS, body);
            // Soft vertical gradient ON TOP of the body for depth.
            // We can't do gradients via blend on mmap, but a single
            // dim band along the bottom edge sells it.
            let dim = body.lerp(theme.bg, 30);
            for row in 0..(ICON_TILE / 2) {
                let py = tile_y + ICON_TILE / 2 + row;
                let t = ((row * 255) / (ICON_TILE / 2).max(1)) as u8;
                let c = body.lerp(dim, t);
                fb.fill_rect(tile_x, py, ICON_TILE, 1, c);
            }
            // 1-px highlight along the top edge — fakes a glassy lift.
            let light = body.lerp(Pixel::WHITE, 60);
            fb.fill_rect(tile_x + ICON_RADIUS, tile_y, ICON_TILE - ICON_RADIUS * 2, 1, light);

            // Selection ring — 2-px accent stroke on the outside.
            if selected {
                let ring = theme.accent;
                fb.fill_rect(tile_x, tile_y, ICON_TILE, 2, ring);
                fb.fill_rect(tile_x, tile_y + ICON_TILE - 2, ICON_TILE, 2, ring);
                fb.fill_rect(tile_x, tile_y, 2, ICON_TILE, ring);
                fb.fill_rect(tile_x + ICON_TILE - 2, tile_y, 2, ICON_TILE, ring);
            }

            // 4× scaled glyph in the centre — readable from a metre
            // away on a 1080p screen; reuses the bitmap font.
            let scale: u32 = 4;
            let gw = GLYPH_WIDTH * scale;
            let gh = GLYPH_HEIGHT * scale;
            let gx = tile_x + (ICON_TILE.saturating_sub(gw)) / 2;
            let gy = tile_y + (ICON_TILE.saturating_sub(gh)) / 2;
            let glyph_fg = if luminance_for(body) > 140 {
                Pixel::rgb(0x10, 0x10, 0x14)
            } else {
                Pixel::WHITE
            };
            draw_glyph_scaled(fb, gx, gy, icon.glyph, glyph_fg, scale);

            // Label under the tile, centred.
            let label_y = tile_y + ICON_TILE + 4;
            let label = &icon.label;
            // Trim to what fits at 1x glyph width.
            let max_chars = (r.w / GLYPH_WIDTH).max(1) as usize;
            let shown: String = if label.chars().count() > max_chars {
                let mut s: String = label.chars().take(max_chars - 1).collect();
                s.push('…');
                s
            } else {
                label.clone()
            };
            let lw = GLYPH_WIDTH * shown.chars().count() as u32;
            let lx = tile_x + (ICON_TILE.saturating_sub(lw)) / 2;
            // Subtle dark shadow behind label so it stays legible over
            // both light and dark wallpapers without per-theme tuning.
            drdr_font::draw_text(fb, lx + 1, label_y + 1, &shown, Pixel::rgba(0, 0, 0, 90).over(theme.bg), theme.bg);
            drdr_font::draw_text(fb, lx, label_y, &shown, theme.fg, theme.bg);
        }
    }

    fn draw_taskbar(&self, fb: &mut Framebuffer, theme: &Theme) {
        let tb = self.taskbar_rect();
        // Frosted bar: solid surface with a 1px accent-tinted hairline
        // at the top so the bar reads as floating above the wallpaper.
        fb.fill_rect(tb.x, tb.y, tb.w, tb.h, theme.surface);
        fb.fill_rect(tb.x, tb.y, tb.w, 1, theme.accent.lerp(theme.bg, 80));

        // Start button — accent chip with rounded corners + wordmark.
        let sb = self.start_btn_rect();
        let sb_hot = hit(sb, self.pointer_x, self.pointer_y) || self.start_open;
        let (sbg, sfg) = if sb_hot {
            (theme.accent, theme.accent_fg)
        } else {
            (theme.surface, theme.accent)
        };
        let chip_pad = 5;
        let chip_r = (tb.h - chip_pad * 2) / 2;
        fb.fill_round_rect(
            sb.x + chip_pad,
            sb.y + chip_pad,
            sb.w - chip_pad * 2,
            tb.h - chip_pad * 2,
            chip_r,
            sbg,
        );
        let ty = sb.y + (tb.h.saturating_sub(GLYPH_HEIGHT)) / 2;
        // Bitmap font is ASCII-only — a clean word beats a tofu glyph.
        drdr_font::draw_text(fb, sb.x + 14, ty, "DrDrOS", sfg, sbg);

        // One rounded chip per open window.
        let slot_w = self.taskbar_slot_w();
        let mut x = sb.w + 4;
        let chip_h = tb.h - chip_pad * 2;
        let chip_radius = (chip_h / 2).min(6);
        for (i, win) in self.windows.iter().enumerate() {
            if x + slot_w > tb.x + tb.w - GLYPH_WIDTH * 11 {
                break;
            }
            let focused = i + 1 == self.windows.len() && !win.minimized;
            let (bg, fg) = if focused {
                (theme.bg.lerp(theme.accent, 30), theme.fg)
            } else if win.minimized {
                (theme.surface.lerp(theme.bg, 80), theme.muted)
            } else {
                (theme.surface.lerp(theme.bg, 30), theme.fg)
            };
            fb.fill_round_rect(
                x + 2,
                tb.y + chip_pad,
                slot_w - 4,
                chip_h,
                chip_radius,
                bg,
            );
            // A focused/active accent underline — Windows 11 style.
            if !win.minimized {
                let uw = if focused { slot_w - 4 } else { slot_w / 3 };
                fb.fill_rect(x + 2, tb.y + tb.h - 3, uw, 2, theme.accent);
            }
            let label = win.app.title();
            let maxc = ((slot_w - 12) / GLYPH_WIDTH) as usize;
            let label: String = label.chars().take(maxc).collect();
            drdr_font::draw_text(fb, x + 10, ty, &label, fg, bg);
            x += slot_w;
        }

        // Clock + date, right-aligned, with a subtle hover chip so the
        // tray area reads as interactive (matches modern Win11 + macOS).
        let cw = GLYPH_WIDTH * self.clock.len() as u32;
        let dw = GLYPH_WIDTH * self.date.len() as u32;
        let cx = tb.x + tb.w - cw.max(dw) - 14;
        let mid = tb.y + (tb.h.saturating_sub(GLYPH_HEIGHT * 2)) / 2;
        let tray_w = cw.max(dw) + 12;
        let tray_x = tb.x + tb.w - tray_w - 4;
        if hit(
            Rect::new(tray_x, tb.y + chip_pad, tray_w, chip_h),
            self.pointer_x,
            self.pointer_y,
        ) {
            fb.fill_round_rect(
                tray_x,
                tb.y + chip_pad,
                tray_w,
                chip_h,
                chip_radius,
                theme.bg.lerp(theme.accent, 30),
            );
        }
        drdr_font::draw_text(fb, cx, mid, &self.clock, theme.fg, theme.surface);
        drdr_font::draw_text(
            fb,
            cx,
            mid + GLYPH_HEIGHT,
            &self.date,
            theme.muted,
            theme.surface,
        );
    }

    fn draw_start_menu(&self, fb: &mut Framebuffer, theme: &Theme) {
        let m = self.start_menu_rect();
        // The Start menu sits ABOVE the taskbar — only the TOP corners
        // are rounded; its bottom edge meets the taskbar flush.
        draw_shadow(fb, m);
        fb.fill_round_rect_corners(
            m.x, m.y, m.w, m.h,
            RADIUS, RADIUS, 0, 0,
            theme.surface,
        );
        // 1-px accent hairline at the very top edge to lift the menu.
        fb.fill_rect(m.x + RADIUS / 2, m.y, m.w.saturating_sub(RADIUS), 1, theme.accent);

        let row_h = GLYPH_HEIGHT + 6;
        for (i, (label, _)) in self.start_items.iter().enumerate() {
            let ry = m.y + 6 + i as u32 * row_h;
            let hot = self.pointer_y >= ry as i32
                && self.pointer_y < (ry + row_h) as i32
                && hit(m, self.pointer_x, self.pointer_y);
            let (bg, fg) = if hot {
                (theme.accent, theme.accent_fg)
            } else {
                (theme.surface, theme.fg)
            };
            if hot {
                fb.fill_round_rect(
                    m.x + 4,
                    ry,
                    m.w - 8,
                    row_h - 2,
                    4,
                    bg,
                );
            }
            drdr_font::draw_text(fb, m.x + 16, ry + 3, label, fg, bg);
        }
    }
}

/// A soft, blurred drop shadow — proper per-pixel quadratic falloff,
/// not the chunky-rectangles approximation. The shadow fades to zero
/// alpha at `SHADOW_REACH` from the (slightly offset) window edge so a
/// floating window reads as "above the surface" without the harsh
/// staircase that stacked translucent rects produce.
fn draw_shadow(fb: &mut Framebuffer, r: Rect) {
    fb.drop_shadow(
        r.x as i32,
        r.y as i32,
        r.w as i32,
        r.h as i32,
        SHADOW_REACH,
        90, // peak alpha — visible but not heavy
    );
}

/// Quick relative-luminance proxy (0..255). Cheap; not WCAG — used to
/// pick a readable foreground over an arbitrary tile colour.
fn luminance_for(p: Pixel) -> u32 {
    // ITU-R BT.601-ish weighting on linear 0..255 — good enough to
    // pick black vs white text over a coloured tile.
    (p.r as u32 * 299 + p.g as u32 * 587 + p.b as u32 * 114) / 1000
}

/// Draw a single bitmap glyph scaled `scale`× by replicating pixels —
/// used by the desktop icons (large logos) and the boot wordmark.
fn draw_glyph_scaled(fb: &mut Framebuffer, x: u32, y: u32, ch: char, fg: Pixel, scale: u32) {
    let glyph = drdr_font::glyph_for(ch as u8);
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..8u32 {
            if *bits & (0x80u8 >> col) != 0 {
                fb.fill_rect(x + col * scale, y + row as u32 * scale, scale, scale, fg);
            }
        }
    }
}

/// 2×-scaled text for the wallpaper wordmark (no second font needed).
fn draw_text_2x(fb: &mut Framebuffer, x: u32, y: u32, text: &str, fg: Pixel) {
    let mut cx = x;
    for ch in text.bytes() {
        let glyph = drdr_font::glyph_for(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8u32 {
                if *bits & (0x80u8 >> col) != 0 {
                    fb.fill_rect(cx + col * 2, y + row as u32 * 2, 2, 2, fg);
                }
            }
        }
        cx += GLYPH_WIDTH * 2;
    }
}

/// Paint a single window: shadow is already down; here we do the rounded
/// frame, the modern title bar (title + minimise/maximise/close), and
/// the app's grid blitted cell by cell.
fn draw_window(fb: &mut Framebuffer, win: &mut Window, theme: &Theme, focused: bool) {
    let r = win.rect;
    let radius = RADIUS.min(r.w / 2).min(r.h / 2);

    // ── Title bar — flat color, only the TOP two corners rounded so it
    //    meets the content area below with a clean straight seam.
    let (bar_color, bar_fg) = if focused {
        (theme.accent, theme.accent_fg)
    } else {
        // A slightly tinted surface so an unfocused bar still reads as
        // chrome (distinct from the content area below).
        (theme.surface.lerp(theme.muted, 26), theme.muted)
    };
    fb.fill_round_rect_corners(
        r.x, r.y, r.w, TITLE_H,
        radius, radius, 0, 0,
        bar_color,
    );

    // ── Window body (content fill) — only the BOTTOM two corners rounded.
    //    The two rounded fills meet edge-to-edge at y = TITLE_H with
    //    square corners on both sides — no overlap, no half-painted
    //    rounded region.
    fb.fill_round_rect_corners(
        r.x, r.y + TITLE_H, r.w, r.h.saturating_sub(TITLE_H),
        0, 0, radius, radius,
        theme.surface,
    );

    // ── 1-pixel divider under the title bar — a quiet hairline so the
    //    bar reads as a separate strip even when both halves share the
    //    same surface color.
    let sep = if focused {
        theme.accent.lerp(theme.bg, 60)
    } else {
        theme.border
    };
    fb.fill_rect(
        r.x + radius / 2,
        r.y + TITLE_H,
        r.w.saturating_sub(radius),
        1,
        sep,
    );

    // ── Title text ──────────────────────────────────────────────────
    let title = win.app.title();
    let ty = r.y + (TITLE_H.saturating_sub(GLYPH_HEIGHT)) / 2;
    let maxc = ((r.w.saturating_sub(BTN_W * 3 + 16)) / GLYPH_WIDTH) as usize;
    let title: String = title.chars().take(maxc.max(1)).collect();
    drdr_font::draw_text(fb, r.x + 14, ty, &title, bar_fg, bar_color);

    // ── Window controls: minimise / maximise / close ────────────────
    // Close reddens on hover (Windows convention); maximise toggles
    // between `#` (maximise) and `+` (restore). The close-button hover
    // fill uses a rounded top-right corner so it stays inside the title
    // bar's rounded shape.
    let (px, py) = fb_pointer();
    let buttons = [
        (win.min_rect(), '_', false, false),
        (
            win.max_rect(),
            if win.restore.is_some() { '+' } else { '#' },
            false,
            false,
        ),
        (win.close_rect(), 'x', true, true), // last = round top-right
    ];
    for (b, g, danger, last) in buttons {
        let hot = px >= b.x as i32
            && px < (b.x + b.w) as i32
            && py >= b.y as i32
            && py < (b.y + b.h) as i32;
        let (cell_bg, glyph_fg) = if hot {
            let c = if danger {
                Pixel::rgb(0xE8, 0x11, 0x23)
            } else {
                theme.bg.lerp(theme.accent, 40)
            };
            if last {
                fb.fill_round_rect_corners(
                    b.x, b.y, b.w, b.h,
                    0, radius, 0, 0,
                    c,
                );
            } else {
                fb.fill_rect(b.x, b.y, b.w, b.h, c);
            }
            (c, if danger { Pixel::WHITE } else { theme.fg })
        } else {
            (bar_color, bar_fg)
        };
        let gx = b.x + (b.w.saturating_sub(GLYPH_WIDTH)) / 2;
        let gy = b.y + (b.h.saturating_sub(GLYPH_HEIGHT)) / 2;
        draw_glyph(fb, gx, gy, g, glyph_fg, cell_bg);
    }

    // ── App content ─────────────────────────────────────────────────
    // Size the grid, let the app paint, blit the cells. We inset the
    // grid-bg fill by `radius` at the bottom so the rounded body's
    // alpha-blended corner pixels stay visible underneath.
    let content = win.content_rect();
    let (cols, rows) = Window::grid_dims(r);
    win.grid.resize(cols, rows, theme.fg, theme.surface);
    win.grid.clear();
    win.app.render(&mut win.grid);

    // Paint the grid-bg as a rounded rectangle at the bottom so a
    // custom grid bg (e.g. the Notes app) doesn't blast over the body
    // corners. Top is square (it meets the title-bar hairline).
    if win.grid.bg() != theme.surface {
        fb.fill_round_rect_corners(
            content.x, content.y, content.w, content.h,
            0, 0, radius.saturating_sub(1), radius.saturating_sub(1),
            win.grid.bg(),
        );
    }
    // Cell glyphs — same as before.
    for gy in 0..win.grid.rows {
        for gx in 0..win.grid.cols {
            let cell = win.grid.cell(gx, gy);
            let pxg = content.x + gx * GLYPH_WIDTH;
            let pyg = content.y + gy * GLYPH_HEIGHT;
            draw_glyph(fb, pxg, pyg, cell.ch, cell.fg, cell.bg);
        }
    }
}

// The control-button hover test needs the pointer position, but
// `draw_window` is a free function. We thread it through a tiny
// thread-local set by `WindowManager::draw` just before painting — far
// simpler than reworking every signature, and the WM is single-threaded.
thread_local! {
    static FB_POINTER: std::cell::Cell<(i32, i32)> = const { std::cell::Cell::new((0, 0)) };
}
fn fb_pointer() -> (i32, i32) {
    FB_POINTER.with(|c| c.get())
}
fn set_fb_pointer(p: (i32, i32)) {
    FB_POINTER.with(|c| c.set(p));
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy(&'static str);
    impl WindowApp for Dummy {
        fn title(&self) -> String {
            self.0.into()
        }
        fn render(&mut self, _g: &mut TextGrid) {}
    }

    fn wm() -> WindowManager {
        let mut m = WindowManager::new(1000, 800);
        m.open(Rect::new(0, 0, 200, 150), Box::new(Dummy("a")));
        m.open(Rect::new(50, 50, 200, 150), Box::new(Dummy("b")));
        m.open(Rect::new(100, 100, 200, 150), Box::new(Dummy("c")));
        m
    }

    fn top_title(m: &WindowManager) -> String {
        m.windows.last().unwrap().app.title()
    }

    #[test]
    fn textgrid_clips_out_of_bounds_writes() {
        let mut g = TextGrid::new(4, 2, Pixel::WHITE, Pixel::BLACK);
        g.write(2, 0, "abcd", Pixel::WHITE, Pixel::BLACK);
        assert_eq!(g.cell(2, 0).ch, 'a');
        assert_eq!(g.cell(3, 0).ch, 'b');
        g.put(0, 99, 'z', Pixel::WHITE, Pixel::BLACK);
        assert_eq!(g.cell(0, 0).ch, ' ');
    }

    #[test]
    fn hit_test_picks_topmost_window() {
        let m = wm();
        assert_eq!(m.hit_window(120, 120), Some(2));
        assert_eq!(m.hit_window(10, 10), Some(0));
        assert_eq!(m.hit_window(900, 700), None);
    }

    #[test]
    fn clicking_a_buried_window_raises_and_focuses_it() {
        let mut m = wm();
        assert_eq!(top_title(&m), "c");
        m.pointer_x = 10;
        m.pointer_y = 10;
        m.handle_mouse(MouseEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        assert_eq!(top_title(&m), "a");
        assert_eq!(m.window_count(), 3);
    }

    #[test]
    fn close_box_closes_the_window() {
        let mut m = wm();
        let cb = m.windows.last().unwrap().close_rect();
        m.pointer_x = (cb.x + 2) as i32;
        m.pointer_y = (cb.y + 2) as i32;
        m.handle_mouse(MouseEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        assert_eq!(m.window_count(), 2);
        assert_eq!(top_title(&m), "b");
    }

    #[test]
    fn dragging_a_titlebar_moves_the_window() {
        let mut m = wm();
        m.pointer_x = 120;
        m.pointer_y = 105;
        m.handle_mouse(MouseEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        m.handle_mouse(MouseEvent::Moved { dx: 40, dy: 30 });
        let r = m.windows.last().unwrap().rect;
        assert_eq!((r.x, r.y), (140, 130));
        m.handle_mouse(MouseEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        });
        m.handle_mouse(MouseEvent::Moved { dx: 100, dy: 100 });
        let r2 = m.windows.last().unwrap().rect;
        assert_eq!((r2.x, r2.y), (140, 130));
    }

    #[test]
    fn alt_tab_cycles_focus() {
        let mut m = wm();
        assert_eq!(top_title(&m), "c");
        m.handle_key(KeyCode::AltTab);
        assert_eq!(top_title(&m), "b");
        m.handle_key(KeyCode::AltTab);
        assert_eq!(top_title(&m), "a");
        m.handle_key(KeyCode::AltTab);
        assert_eq!(top_title(&m), "c");
    }

    #[test]
    fn pointer_is_clamped_to_screen() {
        let mut m = WindowManager::new(640, 480);
        m.handle_mouse(MouseEvent::Moved { dx: -9999, dy: -9999 });
        assert_eq!(m.pointer(), (0, 0));
        m.handle_mouse(MouseEvent::Moved { dx: 9999, dy: 9999 });
        assert_eq!(m.pointer(), (639, 479));
    }

    struct SelfCloser;
    impl WindowApp for SelfCloser {
        fn title(&self) -> String {
            "bye".into()
        }
        fn render(&mut self, _g: &mut TextGrid) {}
        fn on_tick(&mut self) -> AppControl {
            AppControl::Close
        }
    }

    #[test]
    fn an_app_can_close_its_own_window_on_tick() {
        let mut m = WindowManager::new(800, 600);
        m.open(Rect::new(0, 0, 100, 100), Box::new(SelfCloser));
        m.open(Rect::new(0, 0, 100, 100), Box::new(Dummy("keep")));
        m.tick();
        assert_eq!(m.window_count(), 1);
        assert_eq!(top_title(&m), "keep");
    }

    #[test]
    fn minimise_then_taskbar_restores() {
        let mut m = wm();
        // Minimise the focused window 'c' via its min button.
        let mb = m.windows.last().unwrap().min_rect();
        m.pointer_x = (mb.x + 2) as i32;
        m.pointer_y = (mb.y + 2) as i32;
        m.handle_mouse(MouseEvent::Button { button: MouseButton::Left, pressed: true });
        assert!(m.windows.last().unwrap().minimized);
        // It is no longer hit-tested on the desktop…
        assert_ne!(m.hit_window(120, 120), Some(2));
        // …but a taskbar click on its slot restores + raises it.
        let sb_w = m.start_btn_rect().w + 4;
        let slot = m.taskbar_slot_w();
        let tb = m.taskbar_rect();
        m.pointer_x = (sb_w + slot * 2 + 2) as i32; // window index 2 ('c')
        m.pointer_y = (tb.y + tb.h / 2) as i32;
        m.handle_mouse(MouseEvent::Button { button: MouseButton::Left, pressed: true });
        assert!(!m.windows.last().unwrap().minimized);
        assert_eq!(top_title(&m), "c");
    }

    #[test]
    fn super_arrow_snaps_focused_window_to_half() {
        let mut m = wm();
        let (sw, _) = m.screen();
        m.handle_key(KeyCode::SnapLeft);
        let r = m.windows.last().unwrap().rect;
        assert_eq!(r.x, 0);
        assert_eq!(r.w, sw / 2);
        m.handle_key(KeyCode::SnapRight);
        let r = m.windows.last().unwrap().rect;
        assert_eq!(r.x, sw / 2);
        // Snap-up = maximise to the workarea (full width).
        m.handle_key(KeyCode::SnapUp);
        let r = m.windows.last().unwrap().rect;
        assert_eq!(r.x, 0);
        assert_eq!(r.w, sw);
    }

    #[test]
    fn super_tap_toggles_start_menu() {
        let mut m = WindowManager::new(1000, 800);
        m.set_start_menu(vec![]);
        assert!(!m.start_open);
        m.handle_key(KeyCode::Super);
        assert!(m.start_open);
        m.handle_key(KeyCode::Super);
        assert!(!m.start_open);
    }

    #[test]
    fn f1_opens_help_overlay_any_key_closes() {
        let mut m = wm();
        m.handle_key(KeyCode::Help);
        assert!(m.help_open);
        // Any keystroke dismisses the overlay (it's modal).
        m.handle_key(KeyCode::Escape);
        assert!(!m.help_open);
    }

    #[test]
    fn drag_to_top_edge_snaps_to_maximise_on_release() {
        let mut m = WindowManager::new(1000, 800);
        m.open(Rect::new(200, 200, 300, 200), Box::new(Dummy("a")));
        // Grab the title bar.
        let r = m.windows.last().unwrap().rect;
        m.pointer_x = (r.x + 50) as i32;
        m.pointer_y = (r.y + 5) as i32;
        m.handle_mouse(MouseEvent::Button { button: MouseButton::Left, pressed: true });
        // Drag the cursor up to the screen top → triggers snap preview.
        m.handle_mouse(MouseEvent::Moved { dx: 0, dy: -500 });
        assert!(m.snap_preview.is_some());
        // Release → window snaps to the work area.
        m.handle_mouse(MouseEvent::Button { button: MouseButton::Left, pressed: false });
        let r = m.windows.last().unwrap().rect;
        let wa = m.workarea();
        assert_eq!((r.w, r.h), (wa.w, wa.h));
    }

    #[test]
    fn start_button_toggles_menu() {
        let mut m = WindowManager::new(1000, 800);
        m.set_start_menu(vec![(
            "X".into(),
            Box::new(|| Spawn {
                rect: Rect::new(0, 0, 100, 100),
                app: Box::new(Dummy("x")),
            }),
        )]);
        let sb = m.start_btn_rect();
        m.pointer_x = (sb.x + 4) as i32;
        m.pointer_y = (sb.y + 4) as i32;
        m.handle_mouse(MouseEvent::Button { button: MouseButton::Left, pressed: true });
        assert!(m.start_open);
        m.handle_mouse(MouseEvent::Button { button: MouseButton::Left, pressed: true });
        assert!(!m.start_open);
    }
}
