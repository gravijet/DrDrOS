//! DrDrUI Tier 2 — overlapping windows, the app surface, and a small
//! stacking window manager.
//!
//! The app-in-a-window mechanism (and why it isn't a terminal emulator)
//! ───────────────────────────────────────────────────────────────────
//! Tier 1's DrDrDesk handed the whole console to one app at a time. A
//! real desktop runs several apps in overlapping windows instead. The
//! usual way to put a text program in a window is a *terminal
//! emulator* — a PTY plus a parser for the decades of ANSI/VT escape
//! sequences. DrDrOS doesn't copy that (the project rule is "build our
//! own equivalent", and a VT parser is a swamp).
//!
//! Instead we define a deliberately tiny contract:
//!
//!   - A windowed app is anything implementing [`WindowApp`]. It never
//!     touches the framebuffer, a TTY, or escape codes. It does two
//!     things: draw characters into a [`TextGrid`] it's handed, and
//!     react to a [`KeyCode`].
//!   - The window manager owns the grid. It sizes the grid to the
//!     window's content area, asks the focused app to paint into it,
//!     and blits the cells to the framebuffer through DrDrFont. Input
//!     from the [`InputHub`](crate::input::InputHub) is routed to the
//!     focused window.
//!
//! So a "window" is just a rectangle, a title, an app, and a character
//! buffer. Apps compose into the desktop for free, there is no
//! sub-process, no pseudo-terminal, and nothing borrowed from xterm.
//! The trade-off is honest: these are grid apps, not Unix TTY apps —
//! the existing standalone DrDrShell/DrDrEdit binaries still run full
//! screen; the *windowed* apps are written to this surface.

use crate::input::{KeyCode, MouseButton, MouseEvent};
use crate::{Rect, Theme};
use drdr_fb::{Framebuffer, Pixel};
use drdr_font::{GLYPH_HEIGHT, GLYPH_WIDTH, draw_glyph};

// ─── Layout constants ────────────────────────────────────────────────

/// 1px window frame all the way round.
const BORDER: u32 = 1;
/// Title bar height: one glyph row plus breathing room.
const TITLE_H: u32 = GLYPH_HEIGHT + 8;

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
/// windowed app gets — no pixels, no fonts, no escape sequences. The
/// window manager allocates it at the window's content size and blits
/// it; the app just fills cells.
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

    /// Set one cell. Out-of-bounds writes are ignored (clipped), so an
    /// app can't corrupt neighbours by miscounting.
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

/// A program that lives in a window. The contract is intentionally
/// four small methods — see the module docs for why it is *not* a
/// terminal.
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

    /// Periodic heartbeat (~ every `InputHub` tick), even when not
    /// focused — lets a clock or a network panel keep itself current.
    fn on_tick(&mut self) -> AppControl {
        AppControl::Continue
    }
}

// ─── Window ──────────────────────────────────────────────────────────

/// One on-screen window: an outer rectangle (title bar + 1px frame +
/// content), the app inside it, and the app's grid sized to the content.
pub struct Window {
    /// Outer rect in screen pixels — what move/drag operates on.
    pub rect: Rect,
    app: Box<dyn WindowApp>,
    grid: TextGrid,
}

impl Window {
    pub fn new(rect: Rect, app: Box<dyn WindowApp>) -> Self {
        let (cols, rows) = Self::grid_dims(rect);
        Self {
            rect,
            app,
            // Real colours are set from the theme on the first draw.
            grid: TextGrid::new(cols, rows, Pixel::WHITE, Pixel::BLACK),
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

    /// The title bar rect (the draggable strip).
    fn title_rect(&self) -> Rect {
        Rect::new(self.rect.x, self.rect.y, self.rect.w, TITLE_H)
    }

    /// The close box: a TITLE_H square at the title bar's right end.
    fn close_rect(&self) -> Rect {
        let s = TITLE_H;
        Rect::new(self.rect.x + self.rect.w.saturating_sub(s), self.rect.y, s, s)
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
/// `o` = black outline (so it stays visible over any window colour),
/// space = transparent. A north-west arrow with a short tail.
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

// ─── WindowManager ───────────────────────────────────────────────────

/// While the left button is held on a title bar, how far the pointer is
/// from the window's top-left — kept constant so the window tracks the
/// cursor without snapping.
struct Drag {
    grab_dx: i32,
    grab_dy: i32,
}

/// A stacking window manager: a back-to-front list of windows (no
/// compositor, no clipping regions — just paint bottom-up and the top
/// one wins the overlap), one focused window, a cursor, and drag state.
pub struct WindowManager {
    /// Index 0 = bottom of the stack, last = top = focused.
    windows: Vec<Window>,
    screen_w: u32,
    screen_h: u32,
    pointer_x: i32,
    pointer_y: i32,
    drag: Option<Drag>,
}

impl WindowManager {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            windows: Vec::new(),
            screen_w,
            screen_h,
            // Start the cursor mid-screen so it's visible immediately
            // (the headless boot test only ever sees this resting pose).
            pointer_x: (screen_w / 2) as i32,
            pointer_y: (screen_h / 2) as i32,
            drag: None,
        }
    }

    /// Add a window on top (it becomes focused).
    pub fn open(&mut self, rect: Rect, app: Box<dyn WindowApp>) {
        self.windows.push(Window::new(rect, app));
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn pointer(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    /// Screen size the manager was built with — handy for laying out
    /// the initial window set.
    pub fn screen(&self) -> (u32, u32) {
        (self.screen_w, self.screen_h)
    }

    /// Top-most window under `(x, y)`, searching front-to-back.
    fn hit_window(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.windows.len())
            .rev()
            .find(|&i| hit(self.windows[i].rect, x, y))
    }

    /// Move window `i` to the top of the stack (focus + raise).
    fn raise(&mut self, i: usize) {
        if i + 1 < self.windows.len() {
            let w = self.windows.remove(i);
            self.windows.push(w);
        }
    }

    /// Alt+Tab: send the current top to the bottom so the window behind
    /// it gains focus. With < 2 windows it's a no-op.
    fn cycle_focus(&mut self) {
        if self.windows.len() >= 2 {
            // [a, b, c] (c focused) → [c, a, b] (b focused).
            self.windows.rotate_right(1);
        }
    }

    /// Feed a key to the focused window. AltTab is handled by the WM
    /// itself; everything else goes to the top app.
    pub fn handle_key(&mut self, key: KeyCode) {
        if key == KeyCode::AltTab {
            self.cycle_focus();
            return;
        }
        if let Some(top) = self.windows.last_mut() {
            if top.app.on_key(key) == AppControl::Close {
                self.windows.pop();
            }
        }
    }

    /// Feed a pointer event. Returns nothing — the caller redraws after
    /// any event (Tier 2 keeps the loop dead simple; dirty-rect
    /// repaint is a Tier 3 optimisation).
    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        match ev {
            MouseEvent::Moved { dx, dy } => {
                self.pointer_x =
                    (self.pointer_x + dx).clamp(0, self.screen_w as i32 - 1);
                self.pointer_y =
                    (self.pointer_y + dy).clamp(0, self.screen_h as i32 - 1);
                if let Some(d) = &self.drag {
                    // Keep the grabbed point under the cursor; clamp the
                    // title bar on-screen so a window can't be lost.
                    if let Some(w) = self.windows.last_mut() {
                        let nx = (self.pointer_x - d.grab_dx)
                            .clamp(0, self.screen_w as i32 - 1);
                        let ny = (self.pointer_y - d.grab_dy)
                            .clamp(0, self.screen_h as i32 - TITLE_H as i32);
                        w.rect.x = nx.max(0) as u32;
                        w.rect.y = ny.max(0) as u32;
                    }
                }
            }
            MouseEvent::Button { button: MouseButton::Left, pressed: true } => {
                let (x, y) = (self.pointer_x, self.pointer_y);
                if let Some(i) = self.hit_window(x, y) {
                    self.raise(i);
                    let top = self.windows.last().unwrap();
                    if hit(top.close_rect(), x, y) {
                        self.windows.pop(); // close box
                    } else if hit(top.title_rect(), x, y) {
                        let r = top.rect;
                        self.drag = Some(Drag {
                            grab_dx: x - r.x as i32,
                            grab_dy: y - r.y as i32,
                        });
                    }
                    // Click in the content area: focus only for now.
                }
            }
            MouseEvent::Button { button: MouseButton::Left, pressed: false } => {
                self.drag = None;
            }
            _ => {}
        }
    }

    /// Heartbeat: tick *every* window (a background DrDrNet panel must
    /// keep refreshing even when it isn't focused). Windows whose app
    /// asks to close are removed.
    pub fn tick(&mut self) {
        let mut i = 0;
        while i < self.windows.len() {
            if self.windows[i].app.on_tick() == AppControl::Close {
                self.windows.remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Paint the whole desktop: background, then windows back-to-front,
    /// then the cursor on top.
    pub fn draw(&mut self, fb: &mut Framebuffer, theme: &Theme) {
        fb.clear(theme.bg);

        // A faint wordmark + hint behind everything, so an empty desktop
        // (all windows closed) still reads as DrDrOS, not a crash.
        let w = fb.width;
        let h = fb.height;
        let mark = "DrDrOS";
        let mw = GLYPH_WIDTH * mark.len() as u32;
        drdr_font::draw_text(
            fb,
            w.saturating_sub(mw) / 2,
            h / 2 - GLYPH_HEIGHT,
            mark,
            theme.muted,
            theme.bg,
        );
        let hint = "drag titlebars  *  Alt-Tab cycles  *  [x] closes";
        let hw = GLYPH_WIDTH * hint.len() as u32;
        drdr_font::draw_text(
            fb,
            w.saturating_sub(hw) / 2,
            h.saturating_sub(GLYPH_HEIGHT + 8),
            hint,
            theme.muted,
            theme.bg,
        );

        // Top of the stack (last index) is the focused window. If the
        // list is empty this loop simply doesn't run.
        let last = self.windows.len().saturating_sub(1);
        for (i, win) in self.windows.iter_mut().enumerate() {
            draw_window(fb, win, theme, i == last);
        }

        draw_cursor(fb, self.pointer_x, self.pointer_y);
    }
}

/// Paint a single window: frame, title bar (+ close box), and the app's
/// grid blitted cell by cell.
fn draw_window(fb: &mut Framebuffer, win: &mut Window, theme: &Theme, focused: bool) {
    let r = win.rect;

    // Frame.
    fb.fill_rect(r.x, r.y, r.w, r.h, theme.bg);
    let edge = if focused { theme.accent } else { theme.border };
    fb.fill_rect(r.x, r.y, r.w, BORDER, edge);
    fb.fill_rect(r.x, r.y + r.h.saturating_sub(BORDER), r.w, BORDER, edge);
    fb.fill_rect(r.x, r.y, BORDER, r.h, edge);
    fb.fill_rect(r.x + r.w.saturating_sub(BORDER), r.y, BORDER, r.h, edge);

    // Title bar — accent when focused so the active window is obvious.
    let (bar_bg, bar_fg) = if focused {
        (theme.accent, theme.accent_fg)
    } else {
        (theme.surface, theme.muted)
    };
    fb.fill_rect(r.x, r.y, r.w, TITLE_H, bar_bg);
    let title = win.app.title();
    let ty = r.y + (TITLE_H.saturating_sub(GLYPH_HEIGHT)) / 2;
    drdr_font::draw_text(fb, r.x + 6, ty, &title, bar_fg, bar_bg);

    // Close box: a contrasting square with an 'x'.
    let cb = win.close_rect();
    fb.fill_rect(cb.x, cb.y, cb.w, cb.h, bar_bg);
    let cx = cb.x + (cb.w.saturating_sub(GLYPH_WIDTH)) / 2;
    let cy = cb.y + (cb.h.saturating_sub(GLYPH_HEIGHT)) / 2;
    drdr_font::draw_glyph(fb, cx, cy, 'x', bar_fg, bar_bg);

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
        // Runs off the right edge — only 'ab' should land.
        g.write(2, 0, "abcd", Pixel::WHITE, Pixel::BLACK);
        assert_eq!(g.cell(2, 0).ch, 'a');
        assert_eq!(g.cell(3, 0).ch, 'b');
        // Writing to a non-existent row must not panic or corrupt.
        g.put(0, 99, 'z', Pixel::WHITE, Pixel::BLACK);
        assert_eq!(g.cell(0, 0).ch, ' ');
    }

    #[test]
    fn hit_test_picks_topmost_window() {
        let m = wm();
        // (120,120) is inside all three; 'c' is on top.
        assert_eq!(m.hit_window(120, 120), Some(2));
        // (10,10) is only inside 'a'.
        assert_eq!(m.hit_window(10, 10), Some(0));
        // Empty desktop region.
        assert_eq!(m.hit_window(900, 700), None);
    }

    #[test]
    fn clicking_a_buried_window_raises_and_focuses_it() {
        let mut m = wm();
        assert_eq!(top_title(&m), "c");
        // Point inside 'a' only, move cursor there, left-press.
        m.pointer_x = 10;
        m.pointer_y = 10;
        m.handle_mouse(MouseEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        assert_eq!(top_title(&m), "a"); // raised to top == focused
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
        // Press on 'c's title bar (y just below its top at 100).
        m.pointer_x = 120;
        m.pointer_y = 105;
        m.handle_mouse(MouseEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        m.handle_mouse(MouseEvent::Moved { dx: 40, dy: 30 });
        let r = m.windows.last().unwrap().rect;
        assert_eq!((r.x, r.y), (140, 130));
        // Releasing ends the drag — further motion must not move it.
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
        assert_eq!(top_title(&m), "c"); // wrapped around
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
}
