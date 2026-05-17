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
const TITLE_H: u32 = GLYPH_HEIGHT + 12;
/// Each window-control button (minimise / maximise / close) is a square
/// the height of the title bar. The close button is the right-most one,
/// so [`Window::close_rect`] stays a `TITLE_H` square at the far edge.
const BTN_W: u32 = TITLE_H;
/// Bottom taskbar height.
const TASKBAR_H: u32 = GLYPH_HEIGHT + 18;
/// Soft drop-shadow reach, in pixels, down and to the right of a window.
const SHADOW: u32 = 9;

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
    last_click: Option<(std::time::Instant, i32, i32)>,
    dirty: bool,
    /// Builds a fresh launcher window when the desktop empties.
    launcher: Option<Box<dyn Fn() -> Spawn>>,
    /// Start-menu entries: a label and a factory that builds the window.
    start_items: Vec<(String, Box<dyn Fn() -> Spawn>)>,
    start_open: bool,
    /// Cached `HH:MM` / date, refreshed on tick (taskbar clock).
    clock: String,
    date: String,
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
            last_click: None,
            dirty: true,
            launcher: None,
            start_items: Vec::new(),
            start_open: false,
            clock,
            date,
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

    /// If the desktop just emptied, bring the launcher back.
    fn refill_if_empty(&mut self) {
        if self.windows.is_empty() {
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

    /// Feed a key to the focused window. AltTab and the Super/Start key
    /// are handled by the WM; everything else goes to the top app.
    pub fn handle_key(&mut self, key: KeyCode) {
        self.dirty = true;
        if key == KeyCode::AltTab {
            self.start_open = false;
            self.cycle_focus();
            return;
        }
        if self.start_open {
            // The Start menu is keyboard-navigable too (Esc closes it).
            if key == KeyCode::Escape {
                self.start_open = false;
            }
            return;
        }
        let Some(idx) = self.windows.len().checked_sub(1) else {
            return;
        };
        let ctrl = self.windows[idx].app.on_key(key);
        self.drain_spawns(idx);
        if ctrl == AppControl::Close {
            self.windows.remove(idx);
            self.refill_if_empty();
        }
    }

    /// Move the cursor to an already-clamped absolute screen position
    /// and carry any in-progress title-bar drag with it. Shared by
    /// relative mice (`Moved`) and touchscreens (`MovedTo`).
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
        }
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

        // Taskbar window buttons: one slot per window after Start.
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
        match ev {
            MouseEvent::Moved { dx, dy } => {
                let nx = (self.pointer_x + dx).clamp(0, self.screen_w as i32 - 1);
                let ny = (self.pointer_y + dy).clamp(0, self.screen_h as i32 - 1);
                self.move_pointer(nx, ny);
            }
            MouseEvent::MovedTo { x, y } => {
                let nx = x.clamp(0, self.screen_w as i32 - 1);
                let ny = y.clamp(0, self.screen_h as i32 - 1);
                self.move_pointer(nx, ny);
            }
            MouseEvent::Button { button: MouseButton::Left, pressed: true } => {
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
                } else {
                    // Clicking the empty desktop closes the Start menu.
                    self.start_open = false;
                }
            }
            MouseEvent::Button { button: MouseButton::Left, pressed: false } => {
                self.drag = None;
            }
            _ => {}
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
        let mark = "DrDrOS";
        let mw = GLYPH_WIDTH * 2 * mark.len() as u32;
        draw_text_2x(
            fb,
            w.saturating_sub(mw) / 2,
            h / 3,
            mark,
            theme.bg.lerp(theme.fg, 40),
        );

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

        self.draw_taskbar(fb, theme);
        if self.start_open {
            self.draw_start_menu(fb, theme);
        }

        draw_cursor(fb, self.pointer_x, self.pointer_y);
        self.dirty = false;
    }

    fn draw_taskbar(&self, fb: &mut Framebuffer, theme: &Theme) {
        let tb = self.taskbar_rect();
        // Frosted bar: solid surface with a 1px accent top hairline.
        fb.fill_rect(tb.x, tb.y, tb.w, tb.h, theme.surface);
        fb.fill_rect(tb.x, tb.y, tb.w, 1, theme.border);

        // Start button — accent chip with the wordmark.
        let sb = self.start_btn_rect();
        let sb_hot = hit(sb, self.pointer_x, self.pointer_y) || self.start_open;
        let (sbg, sfg) = if sb_hot {
            (theme.accent, theme.accent_fg)
        } else {
            (theme.surface, theme.accent)
        };
        fb.fill_rect(sb.x + 4, sb.y + 4, sb.w - 8, sb.h - 8, sbg);
        let ty = sb.y + (tb.h.saturating_sub(GLYPH_HEIGHT)) / 2;
        // Bitmap font is ASCII-only — a clean word beats a tofu glyph.
        drdr_font::draw_text(fb, sb.x + 12, ty, "DrDrOS", sfg, sbg);

        // One button per window.
        let slot_w = self.taskbar_slot_w();
        let mut x = sb.w + 4;
        for (i, win) in self.windows.iter().enumerate() {
            if x + slot_w > tb.x + tb.w - GLYPH_WIDTH * 11 {
                break;
            }
            let focused = i + 1 == self.windows.len() && !win.minimized;
            let (bg, fg) = if focused {
                (theme.bg.lerp(theme.accent, 30), theme.fg)
            } else if win.minimized {
                (theme.surface, theme.muted)
            } else {
                (theme.surface, theme.fg)
            };
            fb.fill_rect(x + 2, tb.y + 4, slot_w - 4, tb.h - 8, bg);
            // A focused/active accent underline, Windows-style.
            if !win.minimized {
                let uw = if focused { slot_w - 4 } else { slot_w / 3 };
                fb.fill_rect(x + 2, tb.y + tb.h - 5, uw, 2, theme.accent);
            }
            let label = win.app.title();
            let maxc = ((slot_w - 12) / GLYPH_WIDTH) as usize;
            let label: String = label.chars().take(maxc).collect();
            drdr_font::draw_text(fb, x + 8, ty, &label, fg, bg);
            x += slot_w;
        }

        // Clock + date, right-aligned.
        let cw = GLYPH_WIDTH * self.clock.len() as u32;
        let dw = GLYPH_WIDTH * self.date.len() as u32;
        let cx = tb.x + tb.w - cw.max(dw) - 12;
        let mid = tb.y + (tb.h.saturating_sub(GLYPH_HEIGHT * 2)) / 2;
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
        draw_shadow(fb, m);
        fb.fill_rect(m.x, m.y, m.w, m.h, theme.surface);
        fb.fill_rect(m.x, m.y, m.w, 1, theme.accent);
        fb.fill_rect(m.x, m.y + m.h - 1, m.w, 1, theme.border);
        fb.fill_rect(m.x + m.w - 1, m.y, 1, m.h, theme.border);

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
                fb.fill_rect(m.x + 3, ry - 1, m.w - 6, row_h, bg);
            }
            drdr_font::draw_text(fb, m.x + 14, ry + 3, label, fg, bg);
        }
    }
}

/// A blurry-ish drop shadow: a few translucent rectangles fanning down
/// and to the right of `r`, so floating windows lift off the wallpaper
/// like a modern compositor (we just alpha-blend onto the back buffer).
fn draw_shadow(fb: &mut Framebuffer, r: Rect) {
    for i in 1..=SHADOW {
        let a = (70 / SHADOW * (SHADOW - i + 1)) as u8;
        fb.shade_rect(
            r.x + i,
            r.y + i,
            r.w,
            r.h,
            Pixel::rgba(0, 0, 0, a),
        );
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

/// Paint a single window: shadow is already down; here we do the frame,
/// the modern title bar (title + minimise/maximise/close), and the
/// app's grid blitted cell by cell.
fn draw_window(fb: &mut Framebuffer, win: &mut Window, theme: &Theme, focused: bool) {
    let r = win.rect;

    // Body + 1px frame (accent edge when focused).
    fb.fill_rect(r.x, r.y, r.w, r.h, theme.surface);
    let edge = if focused { theme.accent } else { theme.border };
    fb.fill_rect(r.x, r.y, r.w, BORDER, edge);
    fb.fill_rect(r.x, r.y + r.h.saturating_sub(BORDER), r.w, BORDER, edge);
    fb.fill_rect(r.x, r.y, BORDER, r.h, edge);
    fb.fill_rect(r.x + r.w.saturating_sub(BORDER), r.y, BORDER, r.h, edge);

    // Title bar — a subtle gradient; accent when focused.
    let (bar_a, bar_b, bar_fg) = if focused {
        (theme.accent, theme.accent.lerp(Pixel::BLACK, 26), theme.accent_fg)
    } else {
        (theme.surface, theme.bg, theme.muted)
    };
    fb.fill_rect_v(r.x + BORDER, r.y + BORDER, r.w - BORDER * 2, TITLE_H - BORDER, bar_a, bar_b);
    let title = win.app.title();
    let ty = r.y + (TITLE_H.saturating_sub(GLYPH_HEIGHT)) / 2;
    let maxc = ((r.w.saturating_sub(BTN_W * 3 + 12)) / GLYPH_WIDTH) as usize;
    let title: String = title.chars().take(maxc.max(1)).collect();
    drdr_font::draw_text(fb, r.x + 10, ty, &title, bar_fg, bar_a);

    // Control buttons: minimise, maximise/restore, close. The close
    // button reddens on hover (Windows convention); the maximise glyph
    // becomes a "restore" mark while the window is maximised.
    let (px, py) = fb_pointer();
    for (b, g, danger) in [
        (win.min_rect(), '_', false),
        (
            win.max_rect(),
            if win.restore.is_some() { '+' } else { '#' },
            false,
        ),
        (win.close_rect(), 'x', true),
    ] {
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
            fb.fill_rect(b.x, b.y + BORDER, b.w, b.h - BORDER, c);
            (c, if danger { Pixel::WHITE } else { theme.fg })
        } else {
            // Over the title-bar gradient — bar_a is a close-enough bg
            // at button size.
            (bar_a, bar_fg)
        };
        let gx = b.x + (b.w.saturating_sub(GLYPH_WIDTH)) / 2;
        let gy = b.y + (b.h.saturating_sub(GLYPH_HEIGHT)) / 2;
        draw_glyph(fb, gx, gy, g, glyph_fg, cell_bg);
    }

    // Content: size the grid, let the app paint, blit the cells.
    let content = win.content_rect();
    let (cols, rows) = Window::grid_dims(r);
    win.grid.resize(cols, rows, theme.fg, theme.surface);
    win.grid.clear();
    win.app.render(&mut win.grid);

    fb.fill_rect(content.x, content.y, content.w, content.h, win.grid.bg());
    for gy in 0..win.grid.rows {
        for gx in 0..win.grid.cols {
            let cell = win.grid.cell(gx, gy);
            let px = content.x + gx * GLYPH_WIDTH;
            let py = content.y + gy * GLYPH_HEIGHT;
            draw_glyph(fb, px, py, cell.ch, cell.fg, cell.bg);
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
