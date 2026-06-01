//! The windowed apps DrDrDesk hosts.
//!
//! Each one implements [`WindowApp`]: it never opens `/dev/fb0`, never
//! puts a TTY in raw mode, never emits an escape sequence. It paints
//! characters into the [`TextGrid`] the window manager hands it and
//! reacts to [`KeyCode`]s / clicks. That's the whole "app inside a
//! window" mechanism — see `drdr-ui/src/window.rs` for why it's
//! deliberately not a terminal emulator.
//!
//! Apps open *other* windows by queueing a [`Spawn`] (returned from
//! [`WindowApp::take_spawns`]): DrDrFiles opens the editor on a file,
//! the launcher opens anything. Selection highlight uses *reverse
//! video* (swap fg/bg) so apps stay theme-agnostic.

use std::fs;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use drdr_net::status::{KIND_STAT_REQ, Stat, StatReq};
use drdr_net::Conn;
use drdr_ui::{
    AppControl, DesktopIcon, IconKind, KeyCode, Px, Rect, Spawn, TextGrid, Theme, WindowApp,
    WindowManager,
};

use nix::sys::reboot::{RebootMode, reboot};

use crate::net::NetState;
use crate::SharedNet;

/// Snapshot the shared DrDrNet state for read-only use inside an app.
/// Returns `None` while the background thread is still bringing it up
/// (or if networking never came up).
fn net_snapshot(net: &SharedNet) -> Option<NetState> {
    net.lock().ok().and_then(|g| g.clone())
}

// ─── Process-global desktop palette ──────────────────────────────────
//
// The window manager owns the `Theme` it draws with, but the Settings
// window (just another app) needs to flip light/dark at runtime. The
// cleanest seam without threading a handle through every app is one
// atomic the WM re-reads before each repaint — DrDrDesk is single
// process, single UI thread, so this is race-free in practice.

static DARK_THEME: AtomicBool = AtomicBool::new(false);

/// The palette the desktop should paint with right now.
pub fn current_theme() -> Theme {
    if DARK_THEME.load(Ordering::Relaxed) {
        Theme::DRDR
    } else {
        Theme::FLUENT
    }
}

/// Flip between the light ("Fluent") and dark ("Midnight") schemes.
fn toggle_theme() {
    DARK_THEME.fetch_xor(true, Ordering::Relaxed);
}

fn theme_is_dark() -> bool {
    DARK_THEME.load(Ordering::Relaxed)
}

/// Draw `s` at `(col, row)` in reverse video (selected-row look).
fn selected(grid: &mut TextGrid, row: u32, s: &str) {
    grid.fill_row(row, grid.bg(), grid.fg());
    grid.write(0, row, s, grid.bg(), grid.fg());
}

/// Default rect for a window an app spawns. Fits the QEMU 1024x768
/// default with room to see what's underneath.
fn spawn_rect() -> Rect {
    Rect::new(150, 90, 720, 540)
}

// ─── Shared chat + layout helpers (used by the messaging apps) ────────

/// Broadcast a chat line to every known LAN peer and echo it into our own
/// log so the sender sees it immediately. Shared by DrDrChat, DrDrMessages
/// and DrDrDiscord so the three interoperate on one bus — best-effort and
/// fire-and-forget (a silent peer simply never receives it).
fn fanout_chat(net: &NetState, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let msg = drdr_net::chat::ChatMsg {
        from: net.me.host.clone(),
        text,
        ts_unix_secs: crate::net::now_unix_secs(),
    };
    crate::net::push_chat(&net.chat_log, msg.clone());
    let peers = net.directory.lock().map(|d| d.snapshot()).unwrap_or_default();
    for p in peers {
        let addr = std::net::SocketAddr::new(p.addr, p.peer.tcp_port);
        let msg = msg.clone();
        std::thread::spawn(move || {
            let _ = deliver_chat(addr, &msg);
        });
    }
}

/// Dial one peer and push a single chat frame, fire-and-forget.
fn deliver_chat(
    addr: std::net::SocketAddr,
    msg: &drdr_net::chat::ChatMsg,
) -> std::io::Result<()> {
    let to = Duration::from_millis(500);
    let stream = TcpStream::connect_timeout(&addr, to)?;
    stream.set_write_timeout(Some(to)).ok();
    let _ = stream.set_nodelay(true);
    let mut conn = Conn::new(stream);
    conn.send_typed(drdr_net::chat::KIND_CHAT_SAY, msg)
}

/// Word-wrap `text` to at most `width` columns, breaking words that are
/// longer than a whole line. Blank input lines are preserved. Char-safe
/// (never slices a multi-byte boundary). Used by the bubble chat views.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut line_len = 0usize;
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let wlen = word.chars().count();
            if wlen > width {
                if line_len > 0 {
                    out.push(std::mem::take(&mut line));
                    line_len = 0;
                }
                let mut chunk = String::new();
                for ch in word.chars() {
                    chunk.push(ch);
                    if chunk.chars().count() == width {
                        out.push(std::mem::take(&mut chunk));
                    }
                }
                if !chunk.is_empty() {
                    line_len = chunk.chars().count();
                    line = chunk;
                }
                continue;
            }
            let need = if line_len == 0 { wlen } else { wlen + 1 };
            if line_len + need > width {
                out.push(std::mem::take(&mut line));
                line_len = 0;
            }
            if line_len > 0 {
                line.push(' ');
                line_len += 1;
            }
            line.push_str(word);
            line_len += wlen;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Fill a horizontal run of `w` cells from `(col, row)` in one colour —
/// the building block for chat bubbles and coloured panes.
fn fill_cells(g: &mut TextGrid, col: u32, row: u32, w: u32, fg: Px, bg: Px) {
    for c in col..col.saturating_add(w).min(g.cols) {
        g.put(c, row, ' ', fg, bg);
    }
}

/// One painted chat-bubble row: `(x0, width, bubble_bg, runs)` where each
/// run is `(offset_from_x0, text, fg)`. Kept as a named type so the
/// bubble builder reads cleanly (and clippy stays quiet about the nesting).
type BubbleRow = (u32, u32, Px, Vec<(u32, String, Px)>);

/// A stable, legible colour for a username, so a glance distinguishes
/// authors in the Discord-style stream and the peer lists.
fn user_color(name: &str) -> Px {
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    const PALETTE: [Px; 8] = [
        Px::rgb(0x4F, 0xA3, 0xFF),
        Px::rgb(0x57, 0xD9, 0x8E),
        Px::rgb(0xF2, 0x7E, 0x63),
        Px::rgb(0xC9, 0x8B, 0xFF),
        Px::rgb(0xF5, 0xC0, 0x42),
        Px::rgb(0x4D, 0xD0, 0xC4),
        Px::rgb(0xFF, 0x8A, 0xC0),
        Px::rgb(0x9B, 0xD1, 0x5B),
    ];
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

// ─── About ───────────────────────────────────────────────────────────

/// A static welcome card.
pub struct AboutApp;

impl WindowApp for AboutApp {
    fn icon(&self) -> IconKind {
        IconKind::Info
    }
    fn title(&self) -> String {
        "About DrDrOS".into()
    }

    fn render(&mut self, g: &mut TextGrid) {
        let lines = [
            "DrDrOS - a complete custom userland desktop",
            "",
            "Written from scratch in Rust on the Linux kernel.",
            "Framebuffer only (no X11/Wayland). Runs from RAM;",
            "open Disks to save your files to a real disk.",
            "",
            "Using the desktop:",
            "  * Start menu / taskbar  - bottom of the screen",
            "  * move a window   - drag its title bar",
            "  * maximise        - double-click the title bar",
            "  * minimise/close  - the [_] [#] [x] buttons",
            "  * switch windows  - Alt-Tab, or the taskbar",
            "  * theme           - Settings toggles light/dark",
            "",
            concat!("drdr-desk v", env!("CARGO_PKG_VERSION")),
        ];
        for (i, l) in lines.iter().enumerate() {
            g.text(1, i as u32 + 1, l);
        }
    }
}

// ─── Files ───────────────────────────────────────────────────────────

struct Item {
    name: String,
    is_dir: bool,
}

/// What the browser is doing: just listing, typing a new filename, or
/// confirming a delete. A tiny modal state machine so DrDrFiles can
/// create/delete without a separate dialog window.
enum FMode {
    Browse,
    NewName(String),
    ConfirmDel,
}

/// A directory browser: reads the filesystem itself (no `ls`), opens
/// dirs/files on double-click or Enter, and can create + delete.
pub struct FilesApp {
    cwd: PathBuf,
    items: Vec<Item>,
    sel: usize,
    scroll: usize,
    err: Option<String>,
    mode: FMode,
    spawns: Vec<Spawn>,
    menu: MenuBar,
}

impl FilesApp {
    pub fn new(start: impl Into<PathBuf>) -> Self {
        let mut a = Self {
            cwd: start.into(),
            items: Vec::new(),
            sel: 0,
            scroll: 0,
            err: None,
            mode: FMode::Browse,
            spawns: Vec::new(),
            menu: files_menu(),
        };
        a.reload();
        a
    }

    /// Apply a clickable-menu action.
    fn do_action(&mut self, action: &str) -> AppControl {
        match action {
            "new" => self.mode = FMode::NewName(String::new()),
            "reload" => self.reload(),
            "up" => self.go_up(),
            "delete" => {
                if self.items.get(self.sel).is_some_and(|i| i.name != "..") {
                    self.mode = FMode::ConfirmDel;
                }
            }
            "documents" => self.nav_to(drdr_store::documents_dir()),
            "filesystem" => self.nav_to(PathBuf::from("/")),
            "scratch" => self.nav_to(PathBuf::from("/tmp")),
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }

    fn reload(&mut self) {
        self.items.clear();
        self.sel = 0;
        self.scroll = 0;
        self.err = None;

        if self.cwd.parent().is_some() {
            self.items.push(Item { name: "..".into(), is_dir: true });
        }
        match fs::read_dir(&self.cwd) {
            Ok(rd) => {
                let mut dirs = Vec::new();
                let mut files = Vec::new();
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().into_owned();
                    let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if is_dir { &mut dirs } else { &mut files }
                        .push(Item { name, is_dir });
                }
                dirs.sort_by(|a, b| a.name.cmp(&b.name));
                files.sort_by(|a, b| a.name.cmp(&b.name));
                self.items.extend(dirs);
                self.items.extend(files);
            }
            Err(e) => self.err = Some(format!("cannot read dir: {e}")),
        }
    }

    /// Open the selected entry: a directory navigates into it, a file
    /// opens in a new editor window.
    fn activate(&mut self) {
        let Some(it) = self.items.get(self.sel) else { return };
        if it.name == ".." {
            if let Some(p) = self.cwd.parent() {
                self.cwd = p.to_path_buf();
                self.reload();
            }
        } else if it.is_dir {
            self.cwd.push(&it.name);
            self.reload();
        } else {
            let mut path = self.cwd.clone();
            path.push(&it.name);
            // Route by file type: images to the viewer, web/markdown to
            // the browser, binaries to a safe info pane, everything else
            // (text + code) to the editor.
            let app: Box<dyn WindowApp> = match classify(&it.name) {
                FileClass::Image => Box::new(ImageApp::open(path)),
                FileClass::Web => Box::new(BrowserApp::open(path)),
                FileClass::Pdf => Box::new(PdfApp::open(path)),
                FileClass::Archive => Box::new(ArchiveApp::open(path)),
                FileClass::Media => Box::new(MediaApp::open(path)),
                FileClass::Binary => Box::new(BinaryInfoApp::open(path)),
                _ => Box::new(EditApp::new(path)),
            };
            self.spawns.push(Spawn { rect: spawn_rect(), app });
        }
    }

    fn go_up(&mut self) {
        if let Some(p) = self.cwd.parent() {
            self.cwd = p.to_path_buf();
            self.reload();
        }
    }

    fn create_file(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() || name.contains('/') {
            self.err = Some("invalid name (no '/', not empty)".into());
            return;
        }
        let path = self.cwd.join(name);
        match fs::File::create(&path) {
            Ok(_) => self.reload(),
            Err(e) => self.err = Some(format!("create failed: {e}")),
        }
    }

    fn delete_selected(&mut self) {
        let Some(it) = self.items.get(self.sel) else { return };
        if it.name == ".." {
            return;
        }
        let path = self.cwd.join(&it.name);
        let r = if it.is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match r {
            Ok(()) => self.reload(),
            Err(e) => self.err = Some(format!("delete failed: {e}")),
        }
    }

    fn move_sel(&mut self, delta: i32) {
        let n = self.items.len() as i32;
        if n == 0 {
            return;
        }
        self.sel = (self.sel as i32 + delta).clamp(0, n - 1) as usize;
    }

    /// The "Places" shortcuts shown in the left sidebar: the writable
    /// Documents / Data folders, the filesystem root, and the scratch
    /// area. A click jumps straight there — the navigation rail every
    /// modern file manager has.
    fn places(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("Documents", drdr_store::documents_dir()),
            ("My Data", drdr_store::data_dir()),
            ("Filesystem", PathBuf::from("/")),
            ("Scratch", PathBuf::from("/tmp")),
        ]
    }

    /// Jump to a Places shortcut (only if it's a readable directory).
    fn nav_to(&mut self, p: PathBuf) {
        if p.is_dir() {
            self.cwd = p;
            self.reload();
        }
    }
}

/// Width of the file-manager navigation sidebar, in character cells.
const FILES_SIDEBAR_W: u32 = 14;
/// First content column to the right of the sidebar + its divider.
const FILES_LIST_X: u32 = FILES_SIDEBAR_W + 2;

/// Write one list line in the content region `[x0, cols)`, painting a
/// reverse-video background across just that region when selected — so a
/// left sidebar drawn earlier is never overwritten.
fn region_line(g: &mut TextGrid, row: u32, x0: u32, text: &str, sel: bool) {
    if sel {
        let (fg, bg) = (g.bg(), g.fg());
        for c in x0..g.cols {
            g.put(c, row, ' ', fg, bg);
        }
        g.write(x0, row, text, fg, bg);
    } else {
        g.text(x0, row, text);
    }
}

impl WindowApp for FilesApp {
    fn icon(&self) -> IconKind {
        IconKind::Folder
    }
    fn title(&self) -> String {
        format!("DrDrFiles - {}", self.cwd.display())
    }

    fn take_spawns(&mut self) -> Vec<Spawn> {
        std::mem::take(&mut self.spawns)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        match &mut self.mode {
            FMode::NewName(buf) => match key {
                KeyCode::Char(c) => buf.push(c),
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Enter => {
                    let name = std::mem::take(buf);
                    self.mode = FMode::Browse;
                    self.create_file(&name);
                }
                KeyCode::Escape => self.mode = FMode::Browse,
                _ => {}
            },
            FMode::ConfirmDel => match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.mode = FMode::Browse;
                    self.delete_selected();
                }
                _ => self.mode = FMode::Browse,
            },
            FMode::Browse => match key {
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Down => self.move_sel(1),
                KeyCode::PageUp => self.move_sel(-10),
                KeyCode::PageDown => self.move_sel(10),
                KeyCode::Home => self.sel = 0,
                KeyCode::End => self.sel = self.items.len().saturating_sub(1),
                KeyCode::Enter | KeyCode::Right => self.activate(),
                KeyCode::Left | KeyCode::Backspace => self.go_up(),
                KeyCode::Char('r') => self.reload(),
                KeyCode::Char('n') => self.mode = FMode::NewName(String::new()),
                KeyCode::Char('d')
                    if self.items.get(self.sel).is_some_and(|i| i.name != "..") =>
                {
                    self.mode = FMode::ConfirmDel;
                }
                _ => {}
            },
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, double: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        if !matches!(self.mode, FMode::Browse) {
            return AppControl::Continue;
        }
        // Row 0 = menu bar; the sidebar/list both start a row lower now.
        // A click in the left sidebar jumps to that Place (row 1 = header).
        if col < FILES_SIDEBAR_W {
            if row >= 2 {
                let places = self.places();
                if let Some((_, p)) = places.get(row as usize - 2) {
                    self.nav_to(p.clone());
                }
            }
            return AppControl::Continue;
        }
        // The file list: row 1 is the header; entries start at row 2.
        if row < 2 {
            return AppControl::Continue;
        }
        let idx = self.scroll + (row as usize - 2);
        if idx < self.items.len() {
            self.sel = idx;
            if double {
                self.activate();
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let fg = g.fg();
        let bg = g.bg();
        // ── Left navigation sidebar ("Places") — row 0 is the menu bar ──
        let muted = fg;
        g.write(1, 1, "PLACES", muted, bg);
        let active_place = self.places().iter().position(|(_, p)| *p == self.cwd);
        for (i, (label, _)) in self.places().iter().enumerate() {
            let row = i as u32 + 2;
            let sel = active_place == Some(i);
            if sel {
                for c in 0..FILES_SIDEBAR_W {
                    g.put(c, row, ' ', bg, fg);
                }
                g.write(1, row, label, bg, fg);
            } else {
                g.write(1, row, label, fg, bg);
            }
        }
        // Vertical divider between the sidebar and the file list.
        for r in 1..g.rows {
            g.put(FILES_SIDEBAR_W, r, '|', muted, bg);
        }

        let x0 = FILES_LIST_X;
        if let Some(e) = &self.err {
            let e = e.clone();
            g.text(x0, 2, &e);
            g.text(x0, 4, "(any key / r to reload)");
        }
        let rows = g.rows as usize;
        let visible = rows.saturating_sub(3);
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if visible > 0 && self.sel >= self.scroll + visible {
            self.scroll = self.sel + 1 - visible;
        }

        let header = match &self.mode {
            FMode::Browse => format!(
                "{} item(s)  dblclick/Enter open  n new  d del  r reload",
                self.items.len()
            ),
            FMode::NewName(buf) => format!("new file name: {buf}_  (Enter=ok Esc=cancel)"),
            FMode::ConfirmDel => {
                let n = self.items.get(self.sel).map(|i| i.name.as_str()).unwrap_or("?");
                format!("delete '{n}' ?  y = yes, any other key = no")
            }
        };
        g.text(x0, 1, &header);

        if self.err.is_none() {
            for vis in 0..visible {
                let idx = self.scroll + vis;
                if idx >= self.items.len() {
                    break;
                }
                let it = &self.items[idx];
                let line = format!("{:<4} {}", type_tag(&it.name, it.is_dir), it.name);
                let row = vis as u32 + 2;
                region_line(g, row, x0, &line, idx == self.sel);
            }
        }
        // Menu bar last so its drop-down overlays the list.
        self.menu.render(g, fg, bg);
    }
}

/// The file manager's menu bar — file operations, quick navigation and
/// view toggles, all clickable so nothing needs a keyboard shortcut.
fn files_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "File".into(),
                items: vec![
                    MenuBar::item("New file", "new"),
                    MenuBar::item("Delete", "delete"),
                    MenuBar::item("Reload", "reload"),
                ],
            },
            Menu {
                title: "Go".into(),
                items: vec![
                    MenuBar::item("Up one level", "up"),
                    MenuBar::item("Documents", "documents"),
                    MenuBar::item("Filesystem /", "filesystem"),
                    MenuBar::item("Scratch /tmp", "scratch"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

// ─── Text editor ─────────────────────────────────────────────────────

/// A real editable text buffer in a window. Loads a file into lines,
/// supports insert / Backspace / Enter / arrows / Home / End, click to
/// position the caret, and saves. Esc saves and closes; F2 saves and
/// stays. This is the windowed counterpart of the standalone DrDrEdit
/// TTY binary — same project, different surface (a TextGrid, not a TTY).
pub struct EditApp {
    path: PathBuf,
    lines: Vec<String>,
    /// Per-character manual colour, parallel to `lines` (one entry per
    /// char). `None` = use the syntax/default colour. Set by the Colour
    /// menu, applied to typed text.
    colors: Vec<Vec<Option<Px>>>,
    cx: usize,
    cy: usize,
    top: usize,
    modified: bool,
    status: String,
    /// `Some(buf)` while the user is typing a new file name in the
    /// "Save As" prompt (F2). Enter commits, Esc cancels.
    save_as: Option<String>,
    /// Ticks since the last edit while dirty — drives the autosave.
    autosave: u16,
    /// The clickable File / Format / Colour / View menu bar.
    menu: MenuBar,
    /// Current ink for newly typed characters (None = default colour).
    ink: Option<Px>,
    /// Text magnification (1..=3), driven by Format → Bigger/Smaller.
    text_zoom: u32,
    /// Language for syntax highlighting (from the file extension).
    lang: Lang,
    /// Whether syntax highlighting is on (View → Toggle syntax).
    syntax: bool,
}

impl EditApp {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (lines, status) = match fs::read_to_string(&path) {
            Ok(s) => {
                let mut v: Vec<String> = s.split('\n').map(|l| l.to_string()).collect();
                if v.is_empty() {
                    v.push(String::new());
                }
                (v, "loaded".into())
            }
            Err(_) => (vec![String::new()], "new file".into()),
        };
        let colors = lines.iter().map(|l| vec![None; l.chars().count()]).collect();
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let lang = lang_for(&name);
        Self {
            path,
            lines,
            colors,
            cx: 0,
            cy: 0,
            top: 0,
            modified: false,
            status,
            save_as: None,
            autosave: 0,
            menu: editor_menu(),
            ink: None,
            text_zoom: 1,
            lang,
            syntax: lang != Lang::Plain,
        }
    }

    /// Resolve the colour of every character on line `li`: a manual ink
    /// wins, then syntax highlighting (if on), then the default `fg`.
    fn line_colors(&self, li: usize, fg: Px) -> Vec<Px> {
        let line = &self.lines[li];
        let mut base = if self.syntax {
            highlight(line, self.lang, fg)
        } else {
            vec![fg; line.chars().count()]
        };
        if let Some(man) = self.colors.get(li) {
            for (i, c) in base.iter_mut().enumerate() {
                if let Some(Some(px)) = man.get(i) {
                    *c = *px;
                }
            }
        }
        base
    }

    /// Apply an action id fired by the menu bar.
    fn do_action(&mut self, action: &str) -> AppControl {
        match action {
            "new" => {
                self.lines = vec![String::new()];
                self.colors = vec![vec![]];
                self.cx = 0;
                self.cy = 0;
                self.top = 0;
                self.modified = true;
                self.lang = Lang::Plain;
                self.syntax = false;
                self.status = "new document (Save As to name it)".into();
            }
            "save" => {
                self.save();
            }
            "saveas" => {
                self.save_as = Some(String::new());
                self.status = "Save As: type a name then Enter (Esc to cancel)".into();
            }
            "close" => {
                self.save();
                return AppControl::Close;
            }
            "size_up" => self.text_zoom = (self.text_zoom + 1).min(3),
            "size_down" => self.text_zoom = self.text_zoom.saturating_sub(1).max(1),
            "colorline" => {
                if let Some(row) = self.colors.get_mut(self.cy) {
                    for c in row.iter_mut() {
                        *c = self.ink;
                    }
                    self.modified = true;
                }
            }
            "color_reset" => {
                for row in &mut self.colors {
                    for c in row.iter_mut() {
                        *c = None;
                    }
                }
            }
            "ink_default" => self.ink = None,
            "syntax" => self.syntax = !self.syntax,
            "theme" => toggle_theme(),
            other => {
                if other.starts_with("ink_") {
                    self.ink = ink_for_action(other);
                }
            }
        }
        AppControl::Continue
    }

    fn cur_len(&self) -> usize {
        self.lines[self.cy].chars().count()
    }

    fn clamp_cx(&mut self) {
        self.cx = self.cx.min(self.cur_len());
    }

    fn save(&mut self) -> bool {
        let body = self.lines.join("\n");
        match fs::write(&self.path, body) {
            Ok(()) => {
                self.modified = false;
                let where_ = if drdr_store::data_is_persistent() {
                    "saved (persistent)"
                } else {
                    "saved to RAM - mount a disk in Disks to keep it!"
                };
                self.status = format!("{where_}: {}", self.path.display());
                true
            }
            Err(e) => {
                self.status = format!("save failed: {e}");
                false
            }
        }
    }

    /// Commit the "Save As" prompt: route through drdr-store::save so
    /// the file lands in the persistent (or RAM) Documents folder no
    /// matter where the editor was opened from.
    fn save_as_commit(&mut self, name: &str) {
        let body = self.lines.join("\n");
        match drdr_store::save(name, body.as_bytes()) {
            Ok(path) => {
                self.path = path.clone();
                self.modified = false;
                let where_ = if drdr_store::data_is_persistent() {
                    "saved (persistent)"
                } else {
                    "saved to RAM - mount a disk in Disks to keep it!"
                };
                self.status = format!("{where_}: {}", path.display());
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    fn insert(&mut self, c: char) {
        let line = &mut self.lines[self.cy];
        let byte = line
            .char_indices()
            .nth(self.cx)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        line.insert(byte, c);
        // Keep the parallel colour buffer in lock-step with the text.
        let row = &mut self.colors[self.cy];
        let at = self.cx.min(row.len());
        row.insert(at, self.ink);
        self.cx += 1;
        self.modified = true;
    }

    fn backspace(&mut self) {
        if self.cx > 0 {
            let line = &mut self.lines[self.cy];
            let (byte, _) = line.char_indices().nth(self.cx - 1).unwrap();
            line.remove(byte);
            let row = &mut self.colors[self.cy];
            if self.cx - 1 < row.len() {
                row.remove(self.cx - 1);
            }
            self.cx -= 1;
            self.modified = true;
        } else if self.cy > 0 {
            let cur = self.lines.remove(self.cy);
            let cur_c = self.colors.remove(self.cy);
            self.cy -= 1;
            self.cx = self.cur_len();
            self.lines[self.cy].push_str(&cur);
            self.colors[self.cy].extend(cur_c);
            self.modified = true;
        }
    }

    fn newline(&mut self) {
        let at = self.cx.min(self.cur_len());
        let byte = self.lines[self.cy]
            .char_indices()
            .nth(at)
            .map(|(i, _)| i)
            .unwrap_or(self.lines[self.cy].len());
        let rest = self.lines[self.cy].split_off(byte);
        self.lines.insert(self.cy + 1, rest);
        let row = &mut self.colors[self.cy];
        let rest_c = if at <= row.len() { row.split_off(at) } else { Vec::new() };
        self.colors.insert(self.cy + 1, rest_c);
        self.cy += 1;
        self.cx = 0;
        self.modified = true;
    }
}

impl WindowApp for EditApp {
    fn icon(&self) -> IconKind {
        IconKind::Document
    }
    fn title(&self) -> String {
        let star = if self.modified { "*" } else { "" };
        format!("DrDrEdit{star} - {}", self.path.display())
    }

    fn zoom(&self) -> u32 {
        self.text_zoom
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        // "Save As" modal — captures all input until Enter / Esc.
        // Entered via the Backtab (Shift+Tab) shortcut so an unmapped
        // function key isn't a blocker — Tier 3 should add a real
        // ctrl/F-key path through the input layer.
        if let Some(buf) = self.save_as.as_mut() {
            match key {
                KeyCode::Char(c) => buf.push(c),
                KeyCode::Space => buf.push(' '),
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Enter => {
                    let name = std::mem::take(buf);
                    self.save_as = None;
                    let trimmed = name.trim();
                    if !trimmed.is_empty() {
                        self.save_as_commit(trimmed);
                    }
                }
                KeyCode::Escape => {
                    self.save_as = None;
                    self.status = "Save As cancelled".into();
                }
                _ => {}
            }
            return AppControl::Continue;
        }

        // A keystroke dismisses an open menu (so you don't type into it).
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }

        match key {
            // Shift+Tab → "Save As" prompt. Tab inserts indentation;
            // Shift+Tab is otherwise free in the editor and easy to
            // reach without an F-key mapping.
            KeyCode::BackTab => {
                self.save_as = Some(String::new());
                self.status = "Save As: type a name then Enter (Esc to cancel)".into();
            }
            // Save then close. If the save fails we stay open (the
            // status line shows why) so work isn't lost to a bad path.
            KeyCode::Escape if self.save() => return AppControl::Close,
            KeyCode::Escape => {}
            KeyCode::Char(c) => self.insert(c),
            KeyCode::Space => self.insert(' '),
            KeyCode::Tab => {
                self.insert(' ');
                self.insert(' ');
            }
            KeyCode::Enter => self.newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Left => {
                if self.cx > 0 {
                    self.cx -= 1;
                } else if self.cy > 0 {
                    self.cy -= 1;
                    self.cx = self.cur_len();
                }
            }
            KeyCode::Right => {
                if self.cx < self.cur_len() {
                    self.cx += 1;
                } else if self.cy + 1 < self.lines.len() {
                    self.cy += 1;
                    self.cx = 0;
                }
            }
            KeyCode::Up => {
                self.cy = self.cy.saturating_sub(1);
                self.clamp_cx();
            }
            KeyCode::Down => {
                if self.cy + 1 < self.lines.len() {
                    self.cy += 1;
                }
                self.clamp_cx();
            }
            KeyCode::Home => self.cx = 0,
            KeyCode::End => self.cx = self.cur_len(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _double: bool) -> AppControl {
        if self.save_as.is_some() {
            return AppControl::Continue;
        }
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        // Row 0 = menu bar, row 1 = status; document text from row 2.
        if row >= 2 {
            let target = self.top + (row as usize - 2);
            if target < self.lines.len() {
                self.cy = target;
                self.cx = (col as usize).min(self.cur_len());
            }
        }
        AppControl::Continue
    }

    fn on_tick(&mut self) -> AppControl {
        // Autosave to the current file a few seconds after the last edit
        // (not while the Save-As prompt is open — the name isn't final).
        if self.modified && self.save_as.is_none() {
            self.autosave = self.autosave.saturating_add(1);
            if self.autosave >= 40 {
                self.save();
                self.autosave = 0;
            }
        } else if !self.modified {
            self.autosave = 0;
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let rows = g.rows as usize;
        // Row 0 = menu bar, row 1 = status; document text uses the rest.
        let text_rows = rows.saturating_sub(2);
        if self.cy < self.top {
            self.top = self.cy;
        } else if text_rows > 0 && self.cy >= self.top + text_rows {
            self.top = self.cy + 1 - text_rows;
        }

        let star = if self.modified { " *" } else { "" };
        let inkname = if self.ink.is_some() { "custom" } else { "default" };
        let header = if let Some(buf) = &self.save_as {
            format!("Save As: {buf}_  (Enter=ok Esc=cancel)")
        } else {
            format!(
                "[{}]{star} ink:{} size:{}x  {}",
                self.lines.len(),
                inkname,
                self.text_zoom,
                self.status
            )
        };
        g.text(0, 1, &header);

        let fg = g.fg();
        let bg = g.bg();
        for vis in 0..text_rows {
            let li = self.top + vis;
            if li >= self.lines.len() {
                break;
            }
            let row = vis as u32 + 2;
            let colors = self.line_colors(li, fg);
            for (i, ch) in self.lines[li].chars().enumerate() {
                if i as u32 >= g.cols {
                    break;
                }
                let c = colors.get(i).copied().unwrap_or(fg);
                g.put(i as u32, row, ch, c, bg);
            }
            // Caret as a reverse-video cell on its line.
            if li == self.cy {
                let ch = self.lines[li].chars().nth(self.cx).unwrap_or(' ');
                if (self.cx as u32) < g.cols {
                    g.put(self.cx as u32, row, ch, bg, fg);
                }
            }
        }

        // Menu bar painted LAST so its drop-down overlays the document.
        self.menu.render(g, fg, bg);
    }
}

// ─── Launcher ────────────────────────────────────────────────────────

/// The way back: lists every app and opens a fresh window for the
/// chosen one (double-click or Enter). The window manager re-creates
/// this automatically whenever the desktop becomes empty, so closed
/// windows can always be reopened.
pub struct LauncherApp {
    items: Vec<(String, IconKind, Box<dyn Fn() -> Spawn>)>,
    sel: usize,
    spawns: Vec<Spawn>,
}

/// The single source of truth for "every app you can open" — used by
/// the desktop icons, the Launcher window, and the taskbar Start menu,
/// so they can never drift apart. Each entry is a label and a factory
/// that builds the window on demand.
pub fn app_catalog(
    net: SharedNet,
) -> Vec<(String, IconKind, Box<dyn Fn() -> Spawn>)> {
    fn entry(
        label: &str,
        icon: IconKind,
        f: impl Fn() -> Box<dyn WindowApp> + 'static,
    ) -> (String, IconKind, Box<dyn Fn() -> Spawn>) {
        (
            label.to_string(),
            icon,
            Box::new(move || Spawn { rect: spawn_rect(), app: f() }),
        )
    }
    let net_for_chat = net.clone();
    let net_for_settings = net.clone();
    let net_for_panel = net.clone();
    let net_for_network = net.clone();
    let net_for_messages = net.clone();
    let net_for_discord = net.clone();
    vec![
        // Default to the user's writable Documents folder — that's where
        // Notes / drdr-store::save live, and it shows the user where
        // their files actually go (RAM or a mounted disk).
        entry("Files", IconKind::Folder, || Box::new(FilesApp::new(drdr_store::documents_dir()))),
        entry("Text Editor", IconKind::Document, || Box::new(EditApp::new(drdr_store::documents_dir().join("untitled.txt")))),
        entry("Notes (saved)", IconKind::Note, || Box::new(NotesApp::new())),
        entry("Tasks (saved)", IconKind::Tasks, || Box::new(TasksApp::new())),
        entry("Browser", IconKind::Browser, || Box::new(BrowserApp::new())),
        entry("Mail", IconKind::Mail, || Box::new(MailApp::new())),
        entry("Messages", IconKind::Messages, move || Box::new(MessagesApp::new(net_for_messages.clone()))),
        entry("DrDrCord", IconKind::Discord, move || Box::new(DiscordApp::new(net_for_discord.clone()))),
        entry("Contacts", IconKind::Contacts, || Box::new(ContactsApp::new())),
        entry("Calendar", IconKind::Calendar, || Box::new(CalendarApp::new())),
        entry("Terminal (Shell)", IconKind::Terminal, || Box::new(ConsoleApp::new())),
        entry("Calculator", IconKind::Calculator, || Box::new(CalcApp::new())),
        entry("Clock", IconKind::Clock, || Box::new(ClockApp::new())),
        entry("System Monitor", IconKind::Monitor, || Box::new(SysMonApp::new())),
        entry("System Info", IconKind::Info, || Box::new(SysInfoApp::new())),
        entry("Network & Wi-Fi", IconKind::Network, move || Box::new(NetworkApp::new(net_for_network.clone()))),
        entry("DrDrChat (LAN)", IconKind::Chat, move || Box::new(ChatApp::new(net_for_chat.clone()))),
        entry("DrDrPaint", IconKind::Paint, || Box::new(PaintApp::new())),
        entry("DrDrSnake", IconKind::Snake, || Box::new(SnakeApp::new())),
        entry("DrDr2048", IconKind::Dice2048, || Box::new(Game2048::new())),
        entry("DrDrMines", IconKind::Mine, || Box::new(MinesApp::new())),
        entry("Disks", IconKind::Disk, || Box::new(DisksApp::new())),
        entry("Settings", IconKind::Settings, move || Box::new(SettingsApp::new(net_for_settings.clone()))),
        entry("DrDrNet panel", IconKind::Network, move || Box::new(NetApp::new(net_for_panel.clone()))),
        entry("About DrDrOS", IconKind::Info, || Box::new(AboutApp)),
        entry("Power", IconKind::Power, || Box::new(SystemApp::new())),
    ]
}

/// The icons that appear on the empty desktop. Sub-set of `app_catalog`
/// (we don't put every app on the desktop — Power and the DrDrNet
/// diagnostic panel stay in the Start menu only), each with a glyph
/// and a soft tint that helps the eye scan the grid.
pub fn desktop_icons(net: SharedNet) -> Vec<DesktopIcon> {
    fn icon(
        label: &str,
        kind: IconKind,
        tint: Px,
        f: impl Fn() -> Box<dyn WindowApp> + 'static,
    ) -> DesktopIcon {
        DesktopIcon {
            label: label.to_string(),
            icon: kind,
            tint,
            factory: Box::new(move || Spawn { rect: spawn_rect(), app: f() }),
        }
    }
    let net_for_chat = net.clone();
    let net_for_settings = net.clone();
    let net_for_network = net.clone();
    let net_for_messages = net.clone();
    let net_for_discord = net.clone();
    vec![
        icon("Files",      IconKind::Folder,     Px::rgb(0x2D, 0x82, 0xF0), || {
            Box::new(FilesApp::new(drdr_store::documents_dir()))
        }),
        icon("Editor",     IconKind::Document,   Px::rgb(0x5E, 0xB2, 0x4F), || {
            Box::new(EditApp::new(drdr_store::documents_dir().join("untitled.txt")))
        }),
        icon("Notes",      IconKind::Note,       Px::rgb(0xF2, 0xC0, 0x32), || Box::new(NotesApp::new())),
        icon("Tasks",      IconKind::Tasks,      Px::rgb(0x2E, 0xA0, 0x6A), || Box::new(TasksApp::new())),
        icon("Browser",    IconKind::Browser,    Px::rgb(0x1E, 0x9E, 0xD6), || Box::new(BrowserApp::new())),
        icon("Mail",       IconKind::Mail,       Px::rgb(0x2D, 0x6C, 0xD8), || Box::new(MailApp::new())),
        icon("Messages",   IconKind::Messages,   Px::rgb(0x25, 0xB3, 0x5E), move || Box::new(MessagesApp::new(net_for_messages.clone()))),
        icon("DrDrCord",   IconKind::Discord,    Px::rgb(0x58, 0x65, 0xF2), move || Box::new(DiscordApp::new(net_for_discord.clone()))),
        icon("Contacts",   IconKind::Contacts,   Px::rgb(0x2B, 0x9B, 0x8A), || Box::new(ContactsApp::new())),
        icon("Calendar",   IconKind::Calendar,   Px::rgb(0xD0, 0x52, 0x4A), || Box::new(CalendarApp::new())),
        icon("Terminal",   IconKind::Terminal,   Px::rgb(0x33, 0x33, 0x3A), || Box::new(ConsoleApp::new())),
        icon("Calculator", IconKind::Calculator, Px::rgb(0x6B, 0x4F, 0xC9), || Box::new(CalcApp::new())),
        icon("Clock",      IconKind::Clock,      Px::rgb(0xE8, 0x6B, 0x3D), || Box::new(ClockApp::new())),
        icon("Monitor",    IconKind::Monitor,    Px::rgb(0x36, 0xB9, 0xB0), || Box::new(SysMonApp::new())),
        icon("Sys Info",   IconKind::Info,       Px::rgb(0x2D, 0x82, 0xF0), || Box::new(SysInfoApp::new())),
        icon("Network",    IconKind::Network,    Px::rgb(0x2B, 0x9B, 0x8A), move || Box::new(NetworkApp::new(net_for_network.clone()))),
        icon("Chat",       IconKind::Chat,       Px::rgb(0xE8, 0x4E, 0x95), move || Box::new(ChatApp::new(net_for_chat.clone()))),
        icon("Paint",      IconKind::Paint,      Px::rgb(0xC8, 0x36, 0x52), || Box::new(PaintApp::new())),
        icon("Snake",      IconKind::Snake,      Px::rgb(0x4F, 0xB8, 0x35), || Box::new(SnakeApp::new())),
        icon("2048",       IconKind::Dice2048,   Px::rgb(0xED, 0xC2, 0x2E), || Box::new(Game2048::new())),
        icon("Mines",      IconKind::Mine,       Px::rgb(0x4A, 0x6C, 0xD4), || Box::new(MinesApp::new())),
        icon("Disks",      IconKind::Disk,       Px::rgb(0x6E, 0x6E, 0x82), || Box::new(DisksApp::new())),
        icon("Settings",   IconKind::Settings,   Px::rgb(0x6B, 0x73, 0x80), move || Box::new(SettingsApp::new(net_for_settings.clone()))),
    ]
}

/// Open a representative set of windows — used only by the `--ppm
/// DRDR_DEMO` snapshot path so generated screenshots show a working
/// desktop (window chrome + real app text), never in a normal boot.
pub fn open_demo_windows(wm: &mut WindowManager) {
    // Editor on a real source file → menu bar + syntax highlighting.
    wm.open(Rect::new(12, 40, 410, 250), Box::new(EditApp::new("drdr-desk/src/main.rs")));
    // Notes → its new menu bar.
    wm.open(Rect::new(20, 300, 400, 250), Box::new(NotesApp::new()));
    // Paint → its new left tool palette.
    wm.open(Rect::new(300, 360, 380, 250), Box::new(PaintApp::new()));
    // A real web browser, navigated to a live http:// page when the snapshot
    // host has network (it degrades to an honest error page otherwise).
    let mut browser = BrowserApp::new();
    if std::env::var_os("DRDR_DEMO_WEB").is_some() {
        browser.navigate("http://example.com");
    }
    wm.open(Rect::new(450, 40, 555, 360), Box::new(browser));
    let img = std::path::Path::new("/tmp/drdr-demo.jpg");
    if img.exists() {
        wm.open(Rect::new(700, 410, 300, 200), Box::new(ImageApp::open(img.into())));
    }
}

/// Open the productivity apps that gained menu bars — Files, Calculator
/// and the Image viewer — so a snapshot shows their new Windows-style
/// menus. Used by the `DRDR_DEMO3` snapshot path only.
pub fn open_demo_productivity(wm: &mut WindowManager) {
    wm.open(Rect::new(20, 30, 460, 420), Box::new(FilesApp::new(drdr_store::documents_dir())));
    wm.open(Rect::new(500, 30, 360, 300), Box::new(CalcApp::new()));
    let pic = std::path::Path::new("/tmp/pic.ppm");
    if pic.exists() {
        wm.open(Rect::new(420, 280, 500, 420), Box::new(ImageApp::open(pic.into())));
    }
}

/// Open the new communication apps for a snapshot — Messages, DrDrCord,
/// Mail, Calendar and Contacts — so a screenshot shows the messaging
/// suite. Seeds a short conversation into the shared chat log first so the
/// bubble views have content. Used by the `DRDR_DEMO2` snapshot path only.
pub fn open_demo_messaging(wm: &mut WindowManager, net: SharedNet) {
    if let Some(s) = net_snapshot(&net) {
        for (from, text) in [
            ("alice", "Hey everyone, welcome to DrDrOS!"),
            ("drdros", "Thanks! Loving the new look."),
            ("bob", "#help how do I open the calculator?"),
            ("alice", "Click the Calculator tile on the desktop."),
            ("drdros", "Got it, works great."),
        ] {
            crate::net::push_chat(
                &s.chat_log,
                drdr_net::chat::ChatMsg {
                    from: from.into(),
                    text: text.into(),
                    ts_unix_secs: crate::net::now_unix_secs(),
                },
            );
        }
    }
    wm.open(Rect::new(20, 30, 520, 360), Box::new(MailApp::new()));
    wm.open(Rect::new(560, 30, 450, 330), Box::new(ContactsApp::new()));
    wm.open(Rect::new(40, 360, 470, 360), Box::new(CalendarApp::new()));
    wm.open(Rect::new(150, 120, 540, 380), Box::new(MessagesApp::new(net.clone())));
    wm.open(Rect::new(360, 200, 620, 420), Box::new(DiscordApp::new(net)));
}

impl LauncherApp {
    pub fn new(net: SharedNet) -> Self {
        Self { items: app_catalog(net), sel: 0, spawns: Vec::new() }
    }

    fn launch(&mut self) {
        if let Some((_, _, factory)) = self.items.get(self.sel) {
            self.spawns.push(factory());
        }
    }
}

impl WindowApp for LauncherApp {
    fn icon(&self) -> IconKind {
        IconKind::Settings
    }
    fn title(&self) -> String {
        "Launcher".into()
    }

    fn take_spawns(&mut self) -> Vec<Spawn> {
        std::mem::take(&mut self.spawns)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                self.sel = (self.sel + 1).min(self.items.len() - 1)
            }
            KeyCode::Enter | KeyCode::Space | KeyCode::Right => self.launch(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, _col: u32, row: u32, double: bool) -> AppControl {
        if row >= 2 {
            let idx = row as usize - 2;
            if idx < self.items.len() {
                self.sel = idx;
                if double {
                    self.launch();
                }
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        g.text(1, 0, "Open a window (double-click or Enter):");
        for (i, (label, _, _)) in self.items.iter().enumerate() {
            let row = i as u32 + 2;
            let line = format!("  {label}");
            if i == self.sel {
                selected(g, row, &line);
            } else {
                g.text(0, row, &line);
            }
        }
        g.text(
            1,
            self.items.len() as u32 + 3,
            "This window reappears if you close everything.",
        );
    }
}

// ─── System ──────────────────────────────────────────────────────────

/// The power menu.
pub struct SystemApp {
    sel: usize,
}

impl SystemApp {
    pub fn new() -> Self {
        Self { sel: 0 }
    }

    fn activate(&self) {
        // On success reboot() never returns; under QEMU it exits the VM.
        let mode = if self.sel == 0 {
            RebootMode::RB_AUTOBOOT
        } else {
            RebootMode::RB_POWER_OFF
        };
        let _ = reboot(mode);
    }
}

const SYS_ITEMS: [&str; 2] = ["Reboot", "Power off"];

impl WindowApp for SystemApp {
    fn icon(&self) -> IconKind {
        IconKind::Power
    }
    fn title(&self) -> String {
        "System".into()
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(SYS_ITEMS.len() - 1),
            KeyCode::Enter | KeyCode::Space => self.activate(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, _col: u32, row: u32, double: bool) -> AppControl {
        if row >= 2 {
            let idx = row as usize - 2;
            if idx < SYS_ITEMS.len() {
                self.sel = idx;
                if double {
                    self.activate();
                }
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        g.text(1, 0, "Select, then Enter (or double-click):");
        for (i, label) in SYS_ITEMS.iter().enumerate() {
            let row = i as u32 + 2;
            let line = format!("  {label}");
            if i == self.sel {
                selected(g, row, &line);
            } else {
                g.text(0, row, &line);
            }
        }
        g.text(1, SYS_ITEMS.len() as u32 + 3, "(the desktop auto-respawns)");
    }
}

// ─── DrDrNet status panel ────────────────────────────────────────────

/// A live client of DrDrNet's Tier 3 async reactor. Phase 8: the
/// reactor is also on the LAN, so the panel doubles as a peer-list view
/// drawn from the discovery directory.
pub struct NetApp {
    net: SharedNet,
    last: Result<Stat, String>,
    polls: u64,
}

impl NetApp {
    pub fn new(net: SharedNet) -> Self {
        Self { net, last: Err("connecting...".into()), polls: 0 }
    }

    fn fetch(addr: std::net::SocketAddr) -> Result<Stat, String> {
        let to = Duration::from_millis(300);
        let stream =
            TcpStream::connect_timeout(&addr, to).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(to)).ok();
        stream.set_write_timeout(Some(to)).ok();
        let _ = stream.set_nodelay(true);
        let mut conn = Conn::new(stream);
        let (_kind, stat): (u8, Stat) = conn
            .request(KIND_STAT_REQ, &StatReq)
            .map_err(|e| e.to_string())?;
        Ok(stat)
    }
}

impl WindowApp for NetApp {
    fn icon(&self) -> IconKind {
        IconKind::Network
    }
    fn title(&self) -> String {
        match (net_snapshot(&self.net), &self.last) {
            (Some(_), Ok(_)) => "DrDrNet  * online".into(),
            (None, _) => "DrDrNet  - starting...".into(),
            _ => "DrDrNet  - offline".into(),
        }
    }

    fn on_tick(&mut self) -> AppControl {
        self.polls += 1;
        if let Some(net) = net_snapshot(&self.net) {
            // Dial ourselves over loopback — same path real peers take,
            // just a shorter hop. Proves the same code path the LAN uses.
            let loopback = std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                net.reactor_addr.port(),
            );
            self.last = Self::fetch(loopback);
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let teal = Px::rgb(0x3D, 0xD0, 0xBC);
        let red = Px::rgb(0xFF, 0x6B, 0x6B);
        let amber = Px::rgb(0xE0, 0xB0, 0x40);
        let bg = g.bg();

        let snap = net_snapshot(&self.net);
        match (&snap, &self.last) {
            (None, _) => {
                g.write(1, 1, ". starting up...", amber, bg);
                g.text(1, 3, "DrDrNet is coming online in the background.");
                g.text(1, 4, "The reactor binds to all interfaces and");
                g.text(1, 5, "broadcasts a HELLO so peers can find us.");
                g.text(1, 7, "This window updates as soon as it's ready.");
            }
            (Some(net), Ok(s)) => {
                g.write(1, 1, "* connected", teal, bg);
                g.text(1, 3, &format!("server   {}", net.reactor_addr));
                g.text(1, 4, &format!("host     {}", s.host));
                g.text(1, 5, &format!("peer id  {:016x}", net.me.id));
                g.text(1, 6, &format!("uptime   {} s", s.uptime_secs));
                g.text(1, 7, &format!("served   {} requests", s.requests));
                g.text(1, 8, &format!("polls    {} (this window)", self.polls));

                let peers = net.directory.lock()
                    .map(|d| d.snapshot())
                    .unwrap_or_default();
                g.text(1, 10, &format!("LAN peers: {}", peers.len()));
                for (i, p) in peers.iter().take(6).enumerate() {
                    g.text(
                        3,
                        11 + i as u32,
                        &format!("{} @ {}:{}", p.peer.host, p.addr, p.peer.tcp_port),
                    );
                }
                if peers.is_empty() {
                    g.text(3, 11, "(none yet - waiting for HELLOs)");
                }
            }
            (Some(_), Err(e)) => {
                g.write(1, 1, "x disconnected", red, bg);
                let msg = if e.len() > (g.cols as usize).saturating_sub(2) {
                    &e[..(g.cols as usize).saturating_sub(2)]
                } else {
                    e
                };
                g.text(1, 3, msg);
                g.text(1, 5, "retrying every heartbeat...");
            }
        }
    }
}

// ─── Settings ────────────────────────────────────────────────────────

/// Appearance + storage control panel. Toggles the light/dark palette
/// for the whole desktop and shows where files are being saved (and
/// whether that survives a reboot), with a one-key jump to the Disks
/// manager to change it.
pub struct SettingsApp {
    net: SharedNet,
    sel: usize,
    spawns: Vec<Spawn>,
}

const SETTINGS_ROWS: usize = 3;

impl SettingsApp {
    pub fn new(net: SharedNet) -> Self {
        Self { net, sel: 0, spawns: Vec::new() }
    }

    fn activate(&mut self) {
        match self.sel {
            0 => toggle_theme(),
            1 => self.spawns.push(Spawn {
                rect: spawn_rect(),
                app: Box::new(DisksApp::new()),
            }),
            _ => self.spawns.push(Spawn {
                rect: spawn_rect(),
                app: Box::new(NetApp::new(self.net.clone())),
            }),
        }
    }
}

impl WindowApp for SettingsApp {
    fn icon(&self) -> IconKind {
        IconKind::Settings
    }
    fn title(&self) -> String {
        "Settings".into()
    }

    fn take_spawns(&mut self) -> Vec<Spawn> {
        std::mem::take(&mut self.spawns)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(SETTINGS_ROWS - 1),
            KeyCode::Enter | KeyCode::Space | KeyCode::Right => self.activate(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, _c: u32, row: u32, double: bool) -> AppControl {
        if (2..2 + SETTINGS_ROWS as u32).contains(&row) {
            self.sel = (row - 2) as usize;
            if double {
                self.activate();
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        g.text(1, 0, "DrDrOS Settings  -  Up/Down, Enter to change");
        let appearance = if theme_is_dark() {
            "Appearance ......... Dark (DrDr Midnight)"
        } else {
            "Appearance ......... Light (DrDr Fluent)"
        };
        let dir = drdr_store::data_dir();
        let persistent = drdr_store::data_is_persistent();
        let storage = format!(
            "Storage ............ {} [{}]",
            dir.display(),
            if persistent { "persistent" } else { "RAM - not saved!" }
        );
        let rows = [
            appearance.to_string(),
            storage,
            "Network ............ open the DrDrNet panel".to_string(),
        ];
        for (i, line) in rows.iter().enumerate() {
            let r = i as u32 + 2;
            if i == self.sel {
                selected(g, r, &format!("  {line}"));
            } else {
                g.text(0, r, &format!("  {line}"));
            }
        }
        g.text(1, 7, "Tip: pick a disk in 'Disks' to save your files");
        g.text(1, 8, "for good. Until then documents live in RAM and");
        g.text(1, 9, "are lost on shutdown.");
        let red = Px::rgb(0xC8, 0x2B, 0x2B);
        if !persistent {
            g.write(1, 11, "! No persistent storage selected", red, g.bg());
        }
    }
}

// ─── Disks (mount real storage, choose where files live) ─────────────

/// The "save to a real disk, and say where" feature. Lists every block
/// device the kernel sees, what is mounted, and lets the user mount a
/// partition and adopt it as the data directory — after which DrDrEdit
/// and Notes write to it and survive a reboot.
pub struct DisksApp {
    devices: Vec<drdr_store::BlockDev>,
    sel: usize,
    status: String,
}

impl DisksApp {
    pub fn new() -> Self {
        let mut a = Self { devices: Vec::new(), sel: 0, status: String::new() };
        a.reload();
        a
    }

    fn reload(&mut self) {
        self.devices = drdr_store::list_block_devices();
        self.sel = self.sel.min(self.devices.len().saturating_sub(1));
    }

    fn mount_selected(&mut self) {
        let Some(d) = self.devices.get(self.sel).cloned() else { return };
        if !d.partition {
            self.status = format!("{} is a whole disk - pick a partition", d.name);
            return;
        }
        let target = format!("/mnt/{}", d.name);
        match drdr_store::mount_device(&d.dev_path(), &target) {
            Ok(fs) => {
                match drdr_store::set_data_dir(std::path::Path::new(&target)) {
                    Ok(()) => {
                        self.status =
                            format!("mounted {} ({fs}) -> data dir is now {target}", d.name)
                    }
                    Err(e) => self.status = format!("mounted, but set-data-dir failed: {e}"),
                }
                self.reload();
            }
            Err(e) => self.status = format!("mount {} failed: {e}", d.name),
        }
    }

    fn use_mounted(&mut self) {
        let Some(d) = self.devices.get(self.sel).cloned() else { return };
        let Some(mp) = d.mountpoint.clone() else {
            self.status = "not mounted - press Enter to mount it".into();
            return;
        };
        match drdr_store::set_data_dir(std::path::Path::new(&mp)) {
            Ok(()) => self.status = format!("data dir is now {mp}"),
            Err(e) => self.status = format!("could not use {mp}: {e}"),
        }
    }

    fn unmount_selected(&mut self) {
        let Some(d) = self.devices.get(self.sel).cloned() else { return };
        if let Some(mp) = d.mountpoint.clone() {
            match drdr_store::unmount(&mp) {
                Ok(()) => self.status = format!("unmounted {mp}"),
                Err(e) => self.status = format!("unmount failed: {e}"),
            }
            self.reload();
        }
    }
}

impl WindowApp for DisksApp {
    fn icon(&self) -> IconKind {
        IconKind::Disk
    }
    fn title(&self) -> String {
        "Disks".into()
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                self.sel = (self.sel + 1).min(self.devices.len().saturating_sub(1))
            }
            KeyCode::Enter => self.mount_selected(),
            KeyCode::Space => self.use_mounted(),
            KeyCode::Char('u') => self.unmount_selected(),
            KeyCode::Char('r') => self.reload(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, _c: u32, row: u32, double: bool) -> AppControl {
        if row >= 3 {
            let idx = (row - 3) as usize;
            if idx < self.devices.len() {
                self.sel = idx;
                if double {
                    self.mount_selected();
                }
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        g.text(0, 0, "Disks - Enter=mount&use  Space=use mounted  u=unmount  r=rescan");
        let dir = drdr_store::data_dir();
        g.text(
            0,
            1,
            &format!(
                "data dir: {}  [{}]",
                dir.display(),
                if drdr_store::data_is_persistent() { "persistent" } else { "RAM" }
            ),
        );
        g.text(0, 2, "NAME         SIZE      TYPE       MOUNTED AT");
        let visible = (g.rows as usize).saturating_sub(4);
        for (i, d) in self.devices.iter().take(visible).enumerate() {
            let kind = if d.partition {
                if d.removable { "part/USB" } else { "partition" }
            } else {
                "disk"
            };
            let mb = d.size_mb();
            let size = if mb >= 1024 {
                format!("{:.1}G", mb as f64 / 1024.0)
            } else {
                format!("{mb}M")
            };
            let line = format!(
                "{:<12} {:>7} {:<10} {}",
                d.name,
                size,
                kind,
                d.mountpoint.as_deref().unwrap_or("-")
            );
            let r = i as u32 + 3;
            if i == self.sel {
                selected(g, r, &line);
            } else {
                g.text(0, r, &line);
            }
        }
        if self.devices.is_empty() {
            g.text(0, 4, "(no block devices - running purely from RAM)");
        }
        if !self.status.is_empty() {
            g.text(0, g.rows.saturating_sub(1), &self.status);
        }
    }
}

// ─── Notes (persistent quick notes) ──────────────────────────────────

/// A fast scratch-pad that actually persists: it loads and saves through
/// [`drdr_store`], so notes land wherever the user pointed storage (a
/// mounted disk = forever; RAM = this boot). Esc saves and keeps the
/// window open; the title shows the save target and a `*` when dirty.
pub struct NotesApp {
    name: String,
    lines: Vec<String>,
    cx: usize,
    cy: usize,
    top: usize,
    modified: bool,
    status: String,
    /// Ticks elapsed since the last edit while still dirty — drives the
    /// autosave so work is never lost just because Esc wasn't pressed.
    autosave: u16,
    /// The clickable File / View menu bar.
    menu: MenuBar,
}

impl NotesApp {
    pub fn new() -> Self {
        Self::open("notes.txt")
    }

    fn open(name: &str) -> Self {
        let (lines, status) = match drdr_store::load(name) {
            Ok(bytes) => {
                let s = String::from_utf8_lossy(&bytes);
                let mut v: Vec<String> = s.split('\n').map(|l| l.to_string()).collect();
                if v.is_empty() {
                    v.push(String::new());
                }
                (v, "loaded".to_string())
            }
            Err(_) => (vec![String::new()], "new note".to_string()),
        };
        Self {
            name: name.to_string(),
            lines,
            cx: 0,
            cy: 0,
            top: 0,
            modified: false,
            status,
            autosave: 0,
            menu: notes_menu(),
        }
    }

    fn cur_len(&self) -> usize {
        self.lines[self.cy].chars().count()
    }

    /// Apply a menu action.
    fn do_action(&mut self, action: &str) -> AppControl {
        match action {
            "save" => self.save(),
            "new" => {
                self.lines = vec![String::new()];
                self.cx = 0;
                self.cy = 0;
                self.top = 0;
                self.modified = true;
                self.status = "new note".into();
            }
            "close" => {
                self.save();
                return AppControl::Close;
            }
            "theme" => toggle_theme(),
            _ => {}
        }
        AppControl::Continue
    }

    fn save(&mut self) {
        let body = self.lines.join("\n");
        match drdr_store::save(&self.name, body.as_bytes()) {
            Ok(path) => {
                self.modified = false;
                let tag = if drdr_store::data_is_persistent() {
                    "saved (persistent)"
                } else {
                    "saved to RAM - pick a disk in Disks to keep it!"
                };
                self.status = format!("{tag}: {}", path.display());
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    fn insert(&mut self, c: char) {
        let line = &mut self.lines[self.cy];
        let byte = line
            .char_indices()
            .nth(self.cx)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        line.insert(byte, c);
        self.cx += 1;
        self.modified = true;
    }

    fn backspace(&mut self) {
        if self.cx > 0 {
            let line = &mut self.lines[self.cy];
            if let Some((byte, _)) = line.char_indices().nth(self.cx - 1) {
                line.remove(byte);
                self.cx -= 1;
                self.modified = true;
            }
        } else if self.cy > 0 {
            let cur = self.lines.remove(self.cy);
            self.cy -= 1;
            self.cx = self.cur_len();
            self.lines[self.cy].push_str(&cur);
            self.modified = true;
        }
    }

    fn newline(&mut self) {
        let byte = self.lines[self.cy]
            .char_indices()
            .nth(self.cx)
            .map(|(i, _)| i)
            .unwrap_or(self.lines[self.cy].len());
        let rest = self.lines[self.cy].split_off(byte);
        self.lines.insert(self.cy + 1, rest);
        self.cy += 1;
        self.cx = 0;
        self.modified = true;
    }
}

impl WindowApp for NotesApp {
    fn icon(&self) -> IconKind {
        IconKind::Note
    }
    fn title(&self) -> String {
        format!(
            "Notes - {}{}",
            self.name,
            if self.modified { " *" } else { "" }
        )
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        match key {
            KeyCode::Escape => self.save(),
            KeyCode::Char(c) => self.insert(c),
            KeyCode::Space => self.insert(' '),
            KeyCode::Tab => {
                self.insert(' ');
                self.insert(' ');
            }
            KeyCode::Enter => self.newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Left if self.cx > 0 => self.cx -= 1,
            KeyCode::Left if self.cy > 0 => {
                self.cy -= 1;
                self.cx = self.cur_len();
            }
            KeyCode::Right if self.cx < self.cur_len() => self.cx += 1,
            KeyCode::Right if self.cy + 1 < self.lines.len() => {
                self.cy += 1;
                self.cx = 0;
            }
            KeyCode::Up => {
                self.cy = self.cy.saturating_sub(1);
                self.cx = self.cx.min(self.cur_len());
            }
            KeyCode::Down if self.cy + 1 < self.lines.len() => {
                self.cy += 1;
                self.cx = self.cx.min(self.cur_len());
            }
            KeyCode::Home => self.cx = 0,
            KeyCode::End => self.cx = self.cur_len(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        // Row 0 = menu bar, row 1 = status; note text from row 2.
        if row >= 2 {
            let target = self.top + (row as usize - 2);
            if target < self.lines.len() {
                self.cy = target;
                self.cx = (col as usize).min(self.cur_len());
            }
        }
        AppControl::Continue
    }

    fn on_tick(&mut self) -> AppControl {
        // Autosave: a few seconds after the last edit, flush to storage so
        // a note is never lost just because the user didn't press Esc.
        if self.modified {
            self.autosave = self.autosave.saturating_add(1);
            if self.autosave >= 40 {
                self.save();
                self.autosave = 0;
            }
        } else {
            self.autosave = 0;
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let rows = g.rows as usize;
        // Row 0 = menu bar, row 1 = status; note text uses the rest.
        let text_rows = rows.saturating_sub(2);
        if self.cy < self.top {
            self.top = self.cy;
        } else if text_rows > 0 && self.cy >= self.top + text_rows {
            self.top = self.cy + 1 - text_rows;
        }
        g.text(
            0,
            1,
            &format!("Esc=save (autosaves)  [{} lines]  {}", self.lines.len(), self.status),
        );
        let fg = g.fg();
        let bg = g.bg();
        for vis in 0..text_rows {
            let li = self.top + vis;
            if li >= self.lines.len() {
                break;
            }
            let row = vis as u32 + 2;
            g.text(0, row, &self.lines[li]);
            if li == self.cy {
                let ch = self.lines[li].chars().nth(self.cx).unwrap_or(' ');
                if (self.cx as u32) < g.cols {
                    g.put(self.cx as u32, row, ch, bg, fg);
                }
            }
        }
        // Menu bar last so its drop-down overlays the note.
        self.menu.render(g, fg, bg);
    }
}

/// The Notes menu bar — simpler than the editor's (one fixed file).
fn notes_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "File".into(),
                items: vec![
                    MenuBar::item("Save", "save"),
                    MenuBar::item("New", "new"),
                    MenuBar::item("Close", "close"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![MenuBar::item("Toggle theme", "theme")],
            },
        ],
    }
}

// ─── Calculator ──────────────────────────────────────────────────────

/// A real calculator: a recursive-descent expression evaluator over
/// `+ - * / %`, parentheses and decimals, driven by the keyboard or an
/// on-screen keypad. No `bc`, no libm — the parser is ours.
pub struct CalcApp {
    expr: String,
    result: Option<f64>,
    error: Option<String>,
    menu: MenuBar,
}

impl CalcApp {
    pub fn new() -> Self {
        Self { expr: String::new(), result: None, error: None, menu: calc_menu() }
    }

    fn clear(&mut self) {
        self.expr.clear();
        self.result = None;
        self.error = None;
    }

    /// Apply a menu action.
    fn do_action(&mut self, a: &str) -> AppControl {
        match a {
            "clear" => self.clear(),
            "back" => {
                self.expr.pop();
                self.result = None;
                self.error = None;
            }
            "equals" => self.equals(),
            "lparen" => self.push('('),
            "rparen" => self.push(')'),
            "percent" => self.push('%'),
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }

    fn equals(&mut self) {
        match eval_expr(&self.expr) {
            Ok(v) => {
                self.result = Some(v);
                self.error = None;
            }
            Err(e) => {
                self.result = None;
                self.error = Some(e);
            }
        }
    }

    fn push(&mut self, c: char) {
        self.expr.push(c);
        self.result = None;
        self.error = None;
    }
}

/// Keypad layout (also the click target grid).
const KEYPAD: [[char; 4]; 4] = [
    ['7', '8', '9', '/'],
    ['4', '5', '6', '*'],
    ['1', '2', '3', '-'],
    ['0', '.', '=', '+'],
];

impl WindowApp for CalcApp {
    fn icon(&self) -> IconKind {
        IconKind::Calculator
    }
    fn title(&self) -> String {
        "Calculator".into()
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        match key {
            KeyCode::Char(c)
                if c.is_ascii_digit()
                    || "+-*/%().".contains(c) =>
            {
                self.push(c)
            }
            KeyCode::Enter | KeyCode::Char('=') => self.equals(),
            KeyCode::Backspace => {
                self.expr.pop();
                self.result = None;
                self.error = None;
            }
            KeyCode::Escape | KeyCode::Char('c') | KeyCode::Char('C') => self.clear(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        // Row 0 = menu bar; the keypad starts at grid row 5 (5 cols/key).
        if row >= 5 {
            let kr = (row as usize - 5) / 2;
            let kc = (col as usize) / 5;
            if kr < 4 && kc < 4 {
                let ch = KEYPAD[kr][kc];
                if ch == '=' {
                    self.equals();
                } else {
                    self.push(ch);
                }
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let fg = g.fg();
        let bg = g.bg();
        g.text(0, 1, "Type, or click the keys.  Enter = result.");
        let shown = if self.expr.is_empty() { "0" } else { &self.expr };
        g.text(0, 2, &format!("> {shown}_"));
        if let Some(v) = self.result {
            let teal = Px::rgb(0x1F, 0x9E, 0x55);
            g.write(0, 3, &format!("= {}", trim_float(v)), teal, bg);
        } else if let Some(e) = &self.error {
            let red = Px::rgb(0xC8, 0x2B, 0x2B);
            g.write(0, 3, &format!("! {e}"), red, bg);
        }
        for (r, krow) in KEYPAD.iter().enumerate() {
            let row = 5 + r as u32 * 2;
            let mut line = String::new();
            for k in krow {
                line.push_str(&format!("[ {k} ]"));
            }
            g.text(0, row, &line);
        }
        // Menu bar last so its drop-down overlays the keypad.
        self.menu.render(g, fg, bg);
    }
}

/// The Calculator menu bar — edit actions, quick inserts and view toggles
/// so everything works with the mouse as well as the keyboard.
fn calc_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Edit".into(),
                items: vec![
                    MenuBar::item("Evaluate (=)", "equals"),
                    MenuBar::item("Backspace", "back"),
                    MenuBar::item("Clear", "clear"),
                ],
            },
            Menu {
                title: "Insert".into(),
                items: vec![
                    MenuBar::item("( open paren", "lparen"),
                    MenuBar::item(") close paren", "rparen"),
                    MenuBar::item("% percent", "percent"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

/// Strip trailing zeros from a float result: `4.0 -> 4`, `2.5 -> 2.5`.
fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Evaluate `expr` (`+ - * / %`, parentheses, decimals, unary minus).
/// Hand-written recursive descent — small, total, and unit-tested.
pub fn eval_expr(expr: &str) -> Result<f64, String> {
    let tokens: Vec<char> = expr.chars().filter(|c| !c.is_whitespace()).collect();
    let mut p = Parser { t: &tokens, i: 0 };
    let v = p.expr()?;
    if p.i != p.t.len() {
        return Err(format!("unexpected '{}'", p.t[p.i]));
    }
    if !v.is_finite() {
        return Err("not finite".into());
    }
    Ok(v)
}

struct Parser<'a> {
    t: &'a [char],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.t.get(self.i).copied()
    }

    // expr := term (('+' | '-') term)*
    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        while let Some(op) = self.peek() {
            if op == '+' || op == '-' {
                self.i += 1;
                let r = self.term()?;
                v = if op == '+' { v + r } else { v - r };
            } else {
                break;
            }
        }
        Ok(v)
    }

    // term := factor (('*' | '/' | '%') factor)*
    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.factor()?;
        while let Some(op) = self.peek() {
            if op == '*' || op == '/' || op == '%' {
                self.i += 1;
                let r = self.factor()?;
                v = match op {
                    '*' => v * r,
                    '/' => {
                        if r == 0.0 {
                            return Err("divide by zero".into());
                        }
                        v / r
                    }
                    _ => {
                        if r == 0.0 {
                            return Err("mod by zero".into());
                        }
                        v % r
                    }
                };
            } else {
                break;
            }
        }
        Ok(v)
    }

    // factor := '-' factor | '(' expr ')' | number
    fn factor(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some('-') => {
                self.i += 1;
                Ok(-self.factor()?)
            }
            Some('+') => {
                self.i += 1;
                self.factor()
            }
            Some('(') => {
                self.i += 1;
                let v = self.expr()?;
                if self.peek() != Some(')') {
                    return Err("missing ')'".into());
                }
                self.i += 1;
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.number(),
            Some(c) => Err(format!("unexpected '{c}'")),
            None => Err("unexpected end".into()),
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '.' {
                self.i += 1;
            } else {
                break;
            }
        }
        let s: String = self.t[start..self.i].iter().collect();
        s.parse::<f64>().map_err(|_| format!("bad number '{s}'"))
    }
}

// ─── Clock & calendar ────────────────────────────────────────────────

/// A big clock, the date, system uptime, and a month calendar with
/// today highlighted. Uptime comes from `/proc/uptime`; the calendar is
/// computed (no chrono).
pub struct ClockApp;

impl ClockApp {
    pub fn new() -> Self {
        ClockApp
    }
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix days → (year, month, day) — Howard Hinnant's civil algorithm.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days from civil date back to a Unix day number (for the calendar's
/// weekday alignment).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn uptime_secs() -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

impl WindowApp for ClockApp {
    fn icon(&self) -> IconKind {
        IconKind::Clock
    }
    fn title(&self) -> String {
        "Clock".into()
    }

    fn on_tick(&mut self) -> AppControl {
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let secs = now_unix();
        let days = secs.div_euclid(86_400);
        let tod = secs.rem_euclid(86_400);
        let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
        let (y, m, d) = civil(days);

        // Large time, drawn from box characters in the text grid.
        let big = format!("{h:02}:{mi:02}:{s:02}");
        g.text(2, 1, "Coordinated Universal Time");
        let accent = Px::rgb(0x00, 0x67, 0xC0);
        g.write(2, 3, &big, accent, g.bg());
        let months = [
            "January", "February", "March", "April", "May", "June", "July",
            "August", "September", "October", "November", "December",
        ];
        let mon = months.get((m - 1) as usize).copied().unwrap_or("?");
        g.text(2, 5, &format!("{mon} {d}, {y}"));
        let up = uptime_secs();
        g.text(
            2,
            6,
            &format!("Uptime  {}h {}m {}s", up / 3600, (up % 3600) / 60, up % 60),
        );

        // Month calendar. Weekday of the 1st: 1970-01-01 was a Thursday
        // (=4 with Sunday=0).
        g.text(2, 8, "Su Mo Tu We Th Fr Sa");
        let first = days_from_civil(y, m, 1);
        let wd = (((first % 7) + 7 + 4) % 7) as u32; // 0=Sun
        let dim = {
            let nm = days_from_civil(if m == 12 { y + 1 } else { y }, if m == 12 { 1 } else { m + 1 }, 1);
            (nm - first) as i64
        };
        let mut col = wd;
        let mut row = 9u32;
        for day in 1..=dim {
            let cell = format!("{day:>2}");
            let x = 2 + col * 3;
            if day == d {
                g.write(x, row, &cell, g.bg(), accent);
            } else {
                g.text(x, row, &cell);
            }
            col += 1;
            if col == 7 {
                col = 0;
                row += 1;
            }
        }
    }
}

// ─── System monitor ──────────────────────────────────────────────────

/// Live CPU / memory / load / process count, straight from `/proc`.
/// CPU% is the busy-jiffies delta between two ticks (the same maths
/// `top` does), so it needs no kernel API beyond reading a file.
pub struct SysMonApp {
    last_total: u64,
    last_idle: u64,
    cpu_pct: u32,
}

impl SysMonApp {
    pub fn new() -> Self {
        Self { last_total: 0, last_idle: 0, cpu_pct: 0 }
    }

    fn sample_cpu(&mut self) {
        let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
        let Some(line) = stat.lines().next() else { return };
        // "cpu  user nice system idle iowait irq softirq steal ..."
        let v: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|x| x.parse().ok())
            .collect();
        if v.len() < 4 {
            return;
        }
        let idle = v[3] + v.get(4).copied().unwrap_or(0);
        let total: u64 = v.iter().sum();
        let dt = total.saturating_sub(self.last_total);
        let di = idle.saturating_sub(self.last_idle);
        if dt > 0 {
            self.cpu_pct = (((dt - di) * 100) / dt) as u32;
        }
        self.last_total = total;
        self.last_idle = idle;
    }
}

fn meminfo_kb(key: &str) -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

fn proc_count() -> usize {
    fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .chars()
                        .all(|c| c.is_ascii_digit())
                })
                .count()
        })
        .unwrap_or(0)
}

/// A 0..=100 value as a `[#####-----] 50%` bar `width` cells wide.
fn bar(pct: u32, width: u32) -> String {
    let pct = pct.min(100);
    let fill = (pct * width / 100) as usize;
    let mut s = String::from("[");
    for i in 0..width as usize {
        s.push(if i < fill { '#' } else { '-' });
    }
    s.push_str(&format!("] {pct:>3}%"));
    s
}

impl WindowApp for SysMonApp {
    fn icon(&self) -> IconKind {
        IconKind::Monitor
    }
    fn title(&self) -> String {
        "System Monitor".into()
    }

    fn on_tick(&mut self) -> AppControl {
        self.sample_cpu();
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        self.sample_cpu();
        let total = meminfo_kb("MemTotal");
        let avail = meminfo_kb("MemAvailable");
        let used = total.saturating_sub(avail);
        let mem_pct = if total > 0 {
            (used * 100 / total) as u32
        } else {
            0
        };
        let load = fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let load: String = load.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
        let up = uptime_secs();

        g.text(1, 1, "DrDrOS System Monitor");
        g.text(1, 3, &format!("CPU   {}", bar(self.cpu_pct, 24)));
        g.text(1, 4, &format!("RAM   {}", bar(mem_pct, 24)));
        g.text(
            1,
            6,
            &format!("memory   {} / {} MiB", used / 1024, total / 1024),
        );
        g.text(1, 7, &format!("load avg {load}"));
        g.text(1, 8, &format!("processes {}", proc_count()));
        g.text(
            1,
            9,
            &format!("uptime   {}h {}m", up / 3600, (up % 3600) / 60),
        );
        g.text(1, 11, "(updates every heartbeat)");
    }
}

// ─── DrDrConsole (in-window command interpreter, no PTY) ──────────────

/// A usable console without a pseudo-terminal: it interprets a built-in
/// command set itself (the project rule — build our own, don't wrap a
/// TTY). Commands operate via `std::fs` and [`drdr_store`], so they work
/// the same windowed or not. Up/Down recalls history.
pub struct ConsoleApp {
    cwd: PathBuf,
    input: String,
    out: Vec<String>,
    history: Vec<String>,
    hist_idx: Option<usize>,
}

impl ConsoleApp {
    pub fn new() -> Self {
        let mut a = Self {
            cwd: PathBuf::from("/"),
            input: String::new(),
            out: Vec::new(),
            history: Vec::new(),
            hist_idx: None,
        };
        a.out.push("DrDrConsole - type 'help'. No PTY, all built-ins.".into());
        a
    }

    fn echo(&mut self, s: impl Into<String>) {
        for line in s.into().split('\n') {
            self.out.push(line.to_string());
        }
        let max = 400;
        if self.out.len() > max {
            let drop = self.out.len() - max;
            self.out.drain(0..drop);
        }
    }

    fn run(&mut self, line: &str) {
        let line = line.trim();
        self.echo(format!("$ {line}"));
        if line.is_empty() {
            return;
        }
        self.history.push(line.to_string());
        self.hist_idx = None;
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");
        let args: Vec<&str> = it.collect();
        match cmd {
            "help" => self.echo(
                "commands: help ls cd pwd cat echo free ps mounts df \
                 save load ls-docs clear date",
            ),
            "clear" => self.out.clear(),
            "pwd" => {
                let s = self.cwd.display().to_string();
                self.echo(s)
            }
            "echo" => {
                let s = args.join(" ");
                self.echo(s)
            }
            "date" => {
                let secs = now_unix();
                let (y, m, d) = civil(secs.div_euclid(86_400));
                let t = secs.rem_euclid(86_400);
                self.echo(format!(
                    "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
                    t / 3600,
                    (t % 3600) / 60,
                    t % 60
                ))
            }
            "cd" => {
                let target = args.first().copied().unwrap_or("/");
                let new = if target.starts_with('/') {
                    PathBuf::from(target)
                } else {
                    self.cwd.join(target)
                };
                if new.is_dir() {
                    self.cwd = new;
                } else {
                    self.echo(format!("cd: not a directory: {target}"));
                }
            }
            "ls" => {
                let dir = args
                    .first()
                    .map(|a| {
                        if a.starts_with('/') {
                            PathBuf::from(a)
                        } else {
                            self.cwd.join(a)
                        }
                    })
                    .unwrap_or_else(|| self.cwd.clone());
                match fs::read_dir(&dir) {
                    Ok(rd) => {
                        let mut names: Vec<String> = rd
                            .flatten()
                            .map(|e| {
                                let n = e.file_name().to_string_lossy().into_owned();
                                if e.path().is_dir() { format!("{n}/") } else { n }
                            })
                            .collect();
                        names.sort();
                        let joined = names.join("  ");
                        self.echo(joined)
                    }
                    Err(e) => self.echo(format!("ls: {e}")),
                }
            }
            "cat" => {
                if let Some(f) = args.first() {
                    let p = if f.starts_with('/') {
                        PathBuf::from(f)
                    } else {
                        self.cwd.join(f)
                    };
                    match fs::read_to_string(&p) {
                        Ok(s) => self.echo(s),
                        Err(e) => self.echo(format!("cat: {e}")),
                    }
                } else {
                    self.echo("cat: need a file")
                }
            }
            "free" => {
                let t = meminfo_kb("MemTotal");
                let a = meminfo_kb("MemAvailable");
                self.echo(format!(
                    "Mem: total {} MiB  used {} MiB  avail {} MiB",
                    t / 1024,
                    (t - a) / 1024,
                    a / 1024
                ))
            }
            "ps" => self.echo(format!("{} processes (see System Monitor)", proc_count())),
            "mounts" => {
                let lines: Vec<String> = drdr_store::current_mounts()
                    .iter()
                    .map(|m| format!("{:<14} {:<20} {}", m.source, m.target, m.fstype))
                    .collect();
                let joined = lines.join("\n");
                self.echo(joined)
            }
            "df" => {
                let dir = drdr_store::data_dir();
                self.echo(format!(
                    "data dir {} [{}]",
                    dir.display(),
                    if drdr_store::data_is_persistent() { "persistent" } else { "RAM" }
                ))
            }
            "ls-docs" => {
                let joined = drdr_store::list_documents().join("  ");
                self.echo(joined)
            }
            "save" => {
                if args.len() >= 2 {
                    let body = args[1..].join(" ");
                    match drdr_store::save(args[0], body.as_bytes()) {
                        Ok(p) => self.echo(format!("saved {}", p.display())),
                        Err(e) => self.echo(format!("save: {e}")),
                    }
                } else {
                    self.echo("usage: save <name> <text...>")
                }
            }
            "load" => {
                if let Some(n) = args.first() {
                    match drdr_store::load(n) {
                        Ok(b) => self.echo(String::from_utf8_lossy(&b).into_owned()),
                        Err(e) => self.echo(format!("load: {e}")),
                    }
                } else {
                    self.echo("usage: load <name>")
                }
            }
            other => self.echo(format!("unknown command: {other} (try 'help')")),
        }
    }
}

impl WindowApp for ConsoleApp {
    fn icon(&self) -> IconKind {
        IconKind::Terminal
    }
    fn title(&self) -> String {
        format!("DrDrConsole - {}", self.cwd.display())
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Space => self.input.push(' '),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input);
                self.run(&line);
            }
            KeyCode::Up => {
                if !self.history.is_empty() {
                    let i = match self.hist_idx {
                        Some(i) if i > 0 => i - 1,
                        Some(i) => i,
                        None => self.history.len() - 1,
                    };
                    self.hist_idx = Some(i);
                    self.input = self.history[i].clone();
                }
            }
            KeyCode::Down => {
                if let Some(i) = self.hist_idx {
                    if i + 1 < self.history.len() {
                        self.hist_idx = Some(i + 1);
                        self.input = self.history[i + 1].clone();
                    } else {
                        self.hist_idx = None;
                        self.input.clear();
                    }
                }
            }
            KeyCode::Escape => self.input.clear(),
            _ => {}
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let rows = g.rows as usize;
        let prompt_row = rows.saturating_sub(1) as u32;
        let body = rows.saturating_sub(1);
        let start = self.out.len().saturating_sub(body);
        for (i, line) in self.out[start..].iter().enumerate() {
            g.text(0, i as u32, line);
        }
        let accent = Px::rgb(0x00, 0x67, 0xC0);
        g.write(
            0,
            prompt_row,
            &format!("$ {}_", self.input),
            accent,
            g.bg(),
        );
    }
}

// ─── DrDrChat — LAN chat over DrDrNet ────────────────────────────────

/// Chat client + view. The reactor receives chat frames and pushes them
/// into `net.chat_log`; this app reads that log every tick and renders
/// it, plus a composer at the bottom. Enter on the composer fans out
/// the line to every live peer in the discovery directory.
pub struct ChatApp {
    net: SharedNet,
    input: String,
}

impl ChatApp {
    pub fn new(net: SharedNet) -> Self {
        Self { net, input: String::new() }
    }

    /// Self-log the line, then dial every peer in parallel and deliver
    /// the chat frame fire-and-forget (see [`fanout_chat`]).
    fn send(&mut self) {
        let Some(net) = net_snapshot(&self.net) else {
            self.input.clear();
            return;
        };
        let text = std::mem::take(&mut self.input);
        fanout_chat(&net, text);
    }
}

/// Format a Unix-epoch second as `HH:MM` in UTC. Display-only — the
/// receiver doesn't trust the sender's clock, so a wrong timezone here
/// is at worst a wrong label, never wrong ordering.
fn fmt_hhmm(ts: u64) -> String {
    let mins_of_day = ((ts / 60) % (24 * 60)) as u32;
    let h = mins_of_day / 60;
    let m = mins_of_day % 60;
    format!("{:02}:{:02}", h, m)
}

impl WindowApp for ChatApp {
    fn icon(&self) -> IconKind {
        IconKind::Chat
    }
    fn title(&self) -> String {
        match net_snapshot(&self.net) {
            None => "DrDrChat - starting...".into(),
            Some(net) => {
                let peers = net.directory.lock().map(|d| d.len()).unwrap_or(0);
                format!("DrDrChat - {} ({} peers)", net.me.host, peers)
            }
        }
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Space => self.input.push(' '),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.send(),
            KeyCode::Escape => self.input.clear(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_tick(&mut self) -> AppControl {
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let teal = Px::rgb(0x3D, 0xD0, 0xBC);
        let red = Px::rgb(0xFF, 0x6B, 0x6B);
        let muted = Px::rgb(0x80, 0x80, 0x80);
        let bg = g.bg();
        let rows = g.rows;
        let cols = g.cols as usize;

        let Some(net) = net_snapshot(&self.net) else {
            g.write(1, 1, "DrDrChat is connecting...", muted, bg);
            g.text(1, 3, "DrDrNet is still standing up on this machine.");
            g.text(1, 4, "Once the reactor is bound (a moment after");
            g.text(1, 5, "boot) chat opens up on its own.");
            let _ = red; // kept for symmetry with the offline branch
            return;
        };
        let net = &net;

        // ── Peer strip (top 4 rows) ──
        let peers = net.directory.lock().map(|d| d.snapshot()).unwrap_or_default();
        g.text(0, 0, &format!("Peers ({}):", peers.len()));
        for (i, p) in peers.iter().take(3).enumerate() {
            g.write(2, 1 + i as u32, "*", teal, bg);
            g.text(
                4,
                1 + i as u32,
                &format!("{} @ {}:{}", p.peer.host, p.addr, p.peer.tcp_port),
            );
        }
        if peers.is_empty() {
            g.write(2, 1, "(no peers yet - waiting for HELLOs)", muted, bg);
        }

        // ── Message log: rows 5 .. rows-2 ──
        let log_top = 5u32;
        let log_bot = rows.saturating_sub(2); // composer + separator below
        let log_h = log_bot.saturating_sub(log_top);
        if log_h == 0 {
            return;
        }
        let log = net.chat_log.lock().map(|l| l.clone()).unwrap_or_default();
        let start = log.len().saturating_sub(log_h as usize);
        for (i, m) in log[start..].iter().enumerate() {
            let line = format!("{} {:>8}  {}", fmt_hhmm(m.ts_unix_secs), m.from, m.text);
            let line = if line.len() > cols {
                line[..cols].to_string()
            } else {
                line
            };
            // Tint our own lines so a thread is easy to read at a glance.
            let fg = if m.from == net.me.host { teal } else { g.fg() };
            g.write(0, log_top + i as u32, &line, fg, bg);
        }

        // ── Composer (last row) ──
        let prompt_row = rows.saturating_sub(1);
        g.write(0, prompt_row, &format!("> {}_", self.input), teal, bg);
    }
}

// ─── DrDrPaint — mouse-driven block drawing on the TextGrid ──────────

/// Paint by clicking and dragging blocks onto the TextGrid. The top row
/// is a palette of swatches; clicking a swatch selects that colour. The
/// rest of the grid is the canvas. Each painted cell stores its own
/// colour — `c` clears, `e` toggles the eraser.
/// Width of the Paint tool sidebar, in character cells.
const PAINT_SIDEBAR_W: u32 = 9;
/// First canvas column (right of the sidebar + its 1-col divider).
const PAINT_CANVAS_X: u32 = PAINT_SIDEBAR_W + 1;

pub struct PaintApp {
    /// Canvas cells. None = transparent (theme bg), Some(px) = painted.
    cells: Vec<Vec<Option<Px>>>,
    palette: Vec<Px>,
    sel: usize,
    erasing: bool,
    /// Brush size in cells (1..=5) — a fat brush paints a square.
    brush: u32,
    /// Last canvas size we rendered with. The grid is sized to the
    /// window: a resize would invalidate the buffer, so we rebuild on
    /// the first render at a new size. Cheap because we never read what
    /// we threw away — it's "what was painted that scrolled away".
    cur_w: u32,
    cur_h: u32,
}

impl PaintApp {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            palette: vec![
                Px::rgb(0xE6, 0x1E, 0x1E), // red
                Px::rgb(0xE6, 0x8E, 0x1E), // orange
                Px::rgb(0xE6, 0xE6, 0x1E), // yellow
                Px::rgb(0x2E, 0xC8, 0x32), // green
                Px::rgb(0x1E, 0x86, 0xE6), // blue
                Px::rgb(0x9C, 0x39, 0xC8), // violet
                Px::rgb(0xF0, 0xF0, 0xF0), // light
                Px::rgb(0x20, 0x20, 0x20), // dark
            ],
            sel: 4, // blue
            erasing: false,
            brush: 1,
            cur_w: 0,
            cur_h: 0,
        }
    }

    fn ensure_size(&mut self, w: u32, h: u32) {
        if self.cur_w == w && self.cur_h == h && !self.cells.is_empty() {
            return;
        }
        self.cells = vec![vec![None; w as usize]; h as usize];
        self.cur_w = w;
        self.cur_h = h;
    }

    /// A click/drag at grid `(col, row)`: the left sidebar selects a tool;
    /// the canvas to its right paints (a `brush`-sized square).
    fn touch(&mut self, col: u32, row: u32) {
        if col < PAINT_SIDEBAR_W {
            self.sidebar_action(row);
            return;
        }
        if col < PAINT_CANVAS_X {
            return; // the divider column
        }
        let cx = (col - PAINT_CANVAS_X) as usize;
        let cy = row as usize;
        if self.cells.is_empty() {
            return;
        }
        let r = self.brush as usize;
        for dy in 0..r {
            for dx in 0..r {
                let y = cy + dy;
                let x = cx + dx;
                if y < self.cells.len() && x < self.cells[0].len() {
                    self.cells[y][x] = if self.erasing {
                        None
                    } else {
                        Some(self.palette[self.sel])
                    };
                }
            }
        }
    }

    /// Sidebar rows: 1..=8 colours, then Eraser, Clear, Size-, Size+.
    fn sidebar_action(&mut self, row: u32) {
        let n = self.palette.len() as u32;
        if (1..=n).contains(&row) {
            self.sel = (row - 1) as usize;
            self.erasing = false;
        } else if row == n + 1 {
            self.erasing = !self.erasing;
        } else if row == n + 2 {
            self.clear();
        } else if row == n + 3 {
            self.brush = self.brush.saturating_sub(1).max(1);
        } else if row == n + 4 {
            self.brush = (self.brush + 1).min(5);
        }
    }

    fn clear(&mut self) {
        for row in &mut self.cells {
            for c in row.iter_mut() {
                *c = None;
            }
        }
    }
}

impl WindowApp for PaintApp {
    fn icon(&self) -> IconKind {
        IconKind::Paint
    }
    fn title(&self) -> String {
        if self.erasing {
            "DrDrPaint - eraser".into()
        } else {
            "DrDrPaint".into()
        }
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Char('c') | KeyCode::Char('C') => self.clear(),
            KeyCode::Char('e') | KeyCode::Char('E') => self.erasing = !self.erasing,
            KeyCode::Char(d @ '1'..='8') => {
                self.sel = (d as u32 - '1' as u32) as usize;
                self.erasing = false;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.brush = (self.brush + 1).min(5),
            KeyCode::Char('-') | KeyCode::Char('_') => self.brush = self.brush.saturating_sub(1).max(1),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _double: bool) -> AppControl {
        self.touch(col, row);
        AppControl::Continue
    }

    fn on_drag(&mut self, col: u32, row: u32) -> AppControl {
        self.touch(col, row);
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let canvas_w = g.cols.saturating_sub(PAINT_CANVAS_X);
        self.ensure_size(canvas_w, g.rows);
        let fg = g.fg();
        let bg = g.bg();

        // ── Tool sidebar ───────────────────────────────────────────────
        g.write(1, 0, "TOOLS", fg, bg);
        for (i, c) in self.palette.iter().enumerate() {
            let row = i as u32 + 1;
            // A solid swatch + selection marker.
            for col in 1..4u32 {
                g.put(col, row, '\u{2588}', *c, bg);
            }
            if i == self.sel && !self.erasing {
                g.write(5, row, "<", fg, bg);
            }
        }
        let n = self.palette.len() as u32;
        let mark = |on: bool| if on { "[x]" } else { "[ ]" };
        g.write(1, n + 1, &format!("{} Eras", mark(self.erasing)), fg, bg);
        g.write(1, n + 2, "Clear", fg, bg);
        g.write(1, n + 3, "Size -", fg, bg);
        g.write(1, n + 4, &format!("Size + ({})", self.brush), fg, bg);

        // Divider between sidebar and canvas.
        for r in 0..g.rows {
            g.put(PAINT_SIDEBAR_W, r, '|', fg, bg);
        }

        // ── Canvas ─────────────────────────────────────────────────────
        for (cy, row) in self.cells.iter().enumerate() {
            for (cx, cell) in row.iter().enumerate() {
                if let Some(px) = cell {
                    g.put(PAINT_CANVAS_X + cx as u32, cy as u32, '\u{2588}', *px, bg);
                }
            }
        }
    }
}

// ─── DrDrSnake — the game ────────────────────────────────────────────

/// Classic Snake on a `TextGrid`. Tick-driven (the WM's `on_tick`
/// already fires periodically), so the snake advances even while no
/// keys are pressed — the same heartbeat the clock + DrDrNet panel use.
pub struct SnakeApp {
    body: std::collections::VecDeque<(i32, i32)>,
    /// Current direction unit vector (dx, dy). Snake moves one cell per
    /// game tick; we coalesce real ticks into game ticks below.
    dir: (i32, i32),
    /// Buffered next direction — applied at the next game tick so two
    /// rapid keypresses can't fold the snake on itself in one frame.
    next_dir: (i32, i32),
    food: (i32, i32),
    score: u32,
    over: bool,
    /// Width/height the canvas was last seen with. The board shrinks /
    /// grows with the window; on resize we recentre everything.
    cw: i32,
    ch: i32,
    /// PRNG state for placing food. xorshift64 — tiny, no crate.
    rng: u64,
    /// Real ticks since the last game step. The WM's heartbeat is
    /// faster than we want the snake to crawl, so we step every
    /// `STEP_TICKS` ticks.
    tick_accum: u32,
}

const SNAKE_STEP_TICKS: u32 = 1; // tweak if the WM tick rate changes

impl SnakeApp {
    pub fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xCAFE_BABE);
        let mut s = Self {
            body: std::collections::VecDeque::new(),
            dir: (1, 0),
            next_dir: (1, 0),
            food: (0, 0),
            score: 0,
            over: false,
            cw: 0,
            ch: 0,
            rng: now.max(1),
            tick_accum: 0,
        };
        s.reset_for(40, 20);
        s
    }

    fn reset_for(&mut self, w: i32, h: i32) {
        self.cw = w.max(8);
        self.ch = h.max(6);
        self.body.clear();
        let cx = self.cw / 2;
        let cy = self.ch / 2;
        // Front of the deque is the *head*; tail is at the back. We're
        // heading right, so the head sits at (cx, cy) and earlier body
        // cells extend left. push_back ensures front == head.
        for i in 0..4 {
            self.body.push_back((cx - i, cy));
        }
        self.dir = (1, 0);
        self.next_dir = (1, 0);
        self.score = 0;
        self.over = false;
        self.place_food();
    }

    fn xorshift(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn place_food(&mut self) {
        // Try a few times to land on an empty cell; if the body fills the
        // board we'd loop forever, so cap attempts and just plant on the
        // head if every cell is taken (the user has won).
        for _ in 0..64 {
            let r = self.xorshift();
            let x = (r as i32).rem_euclid(self.cw);
            let y = ((r >> 32) as i32).rem_euclid(self.ch);
            if !self.body.iter().any(|&p| p == (x, y)) {
                self.food = (x, y);
                return;
            }
        }
        self.food = *self.body.front().unwrap_or(&(0, 0));
    }

    fn step(&mut self) {
        if self.over {
            return;
        }
        // Apply the buffered direction unless it's a direct reversal.
        let (ndx, ndy) = self.next_dir;
        if (ndx, ndy) != (-self.dir.0, -self.dir.1) {
            self.dir = (ndx, ndy);
        }
        let head = self.body.front().copied().unwrap_or((0, 0));
        let nx = head.0 + self.dir.0;
        let ny = head.1 + self.dir.1;
        // Wall collision = game over (no wrap; classic).
        if nx < 0 || ny < 0 || nx >= self.cw || ny >= self.ch {
            self.over = true;
            return;
        }
        // Self-collision = game over.
        if self.body.iter().any(|&p| p == (nx, ny)) {
            self.over = true;
            return;
        }
        self.body.push_front((nx, ny));
        if (nx, ny) == self.food {
            self.score += 1;
            self.place_food();
        } else {
            self.body.pop_back();
        }
    }
}

impl WindowApp for SnakeApp {
    fn icon(&self) -> IconKind {
        IconKind::Snake
    }
    fn title(&self) -> String {
        if self.over {
            format!("DrDrSnake - GAME OVER (score {}) - R to restart", self.score)
        } else {
            format!("DrDrSnake - score {}  (arrows to steer)", self.score)
        }
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Up => self.next_dir = (0, -1),
            KeyCode::Down => self.next_dir = (0, 1),
            KeyCode::Left => self.next_dir = (-1, 0),
            KeyCode::Right => self.next_dir = (1, 0),
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.reset_for(self.cw, self.ch);
            }
            _ => {}
        }
        AppControl::Continue
    }

    fn on_tick(&mut self) -> AppControl {
        self.tick_accum += 1;
        if self.tick_accum >= SNAKE_STEP_TICKS {
            self.tick_accum = 0;
            self.step();
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        // First render at this size, or a resized window? Re-fit the board
        // so the snake never leaves the canvas after the user maximises.
        if self.cw != g.cols as i32 || self.ch != g.rows as i32 {
            self.reset_for(g.cols as i32, g.rows as i32);
        }
        let bg = g.bg();
        let snake = Px::rgb(0x2E, 0xC8, 0x32);
        let head_c = Px::rgb(0x6E, 0xFF, 0x72);
        let food = Px::rgb(0xE6, 0x1E, 0x1E);
        for (i, &(x, y)) in self.body.iter().enumerate() {
            let c = if i == 0 { head_c } else { snake };
            g.put(x as u32, y as u32, '\u{2588}', c, bg);
        }
        g.put(self.food.0 as u32, self.food.1 as u32, '\u{25CF}', food, bg);
        if self.over {
            let msg = format!(" GAME OVER  score {}  - press R ", self.score);
            let col = (g.cols.saturating_sub(msg.len() as u32)) / 2;
            let row = g.rows / 2;
            g.write(col, row, &msg, g.bg(), Px::rgb(0xFF, 0xFF, 0xFF));
        }
    }
}

// ─── DrDrTasks — a persistent to-do list ─────────────────────────────

/// A to-do list that **persists**. It loads and saves through
/// [`drdr_store`], so your tasks survive a reboot once a disk is mounted
/// — and it autosaves, so you never lose one. Type a task and press Enter
/// to add it; click a task to tick it off, double-click to delete. This is
/// the app that best shows DrDrOS storage doing its everyday job.
pub struct TasksApp {
    /// `(done, text)` per task.
    tasks: Vec<(bool, String)>,
    input: String,
    status: String,
    modified: bool,
    autosave: u16,
}

const TASKS_FILE: &str = "tasks.txt";

impl TasksApp {
    pub fn new() -> Self {
        let mut t = Self {
            tasks: Vec::new(),
            input: String::new(),
            status: String::new(),
            modified: false,
            autosave: 0,
        };
        t.load();
        t
    }

    fn load(&mut self) {
        match drdr_store::load(TASKS_FILE) {
            Ok(bytes) => {
                let s = String::from_utf8_lossy(&bytes);
                for line in s.lines() {
                    let done = line.starts_with("[x]") || line.starts_with("[X]");
                    let text = line
                        .trim_start_matches(|c| matches!(c, '[' | ']' | 'x' | 'X' | ' '))
                        .to_string();
                    if !text.is_empty() {
                        self.tasks.push((done, text));
                    }
                }
                self.status = format!("loaded {} task(s)", self.tasks.len());
            }
            Err(_) => self.status = "new list".into(),
        }
    }

    fn save(&mut self) {
        let body: String = self
            .tasks
            .iter()
            .map(|(d, t)| format!("[{}] {}\n", if *d { "x" } else { " " }, t))
            .collect();
        match drdr_store::save(TASKS_FILE, body.as_bytes()) {
            Ok(_) => {
                self.modified = false;
                self.status = if drdr_store::data_is_persistent() {
                    "saved (persistent)".into()
                } else {
                    "saved to RAM - mount a disk in Disks to keep it!".into()
                };
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    fn add(&mut self) {
        let t = self.input.trim();
        if !t.is_empty() {
            self.tasks.push((false, t.to_string()));
            self.input.clear();
            self.modified = true;
            self.save();
        }
    }
}

impl WindowApp for TasksApp {
    fn icon(&self) -> IconKind {
        IconKind::Tasks
    }
    fn title(&self) -> String {
        let open = self.tasks.iter().filter(|(d, _)| !*d).count();
        format!(
            "Tasks - {} open / {} total{}",
            open,
            self.tasks.len(),
            if self.modified { " *" } else { "" }
        )
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Space => self.input.push(' '),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.add(),
            KeyCode::Escape => self.save(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, _col: u32, row: u32, double: bool) -> AppControl {
        if row >= 3 {
            let idx = (row - 3) as usize;
            if idx < self.tasks.len() {
                if double {
                    self.tasks.remove(idx);
                } else {
                    self.tasks[idx].0 = !self.tasks[idx].0;
                }
                self.modified = true;
                self.save();
            }
        }
        AppControl::Continue
    }

    fn on_tick(&mut self) -> AppControl {
        if self.modified {
            self.autosave = self.autosave.saturating_add(1);
            if self.autosave >= 40 {
                self.save();
                self.autosave = 0;
            }
        } else {
            self.autosave = 0;
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        g.text(0, 0, "DrDrTasks - type + Enter to add   click=toggle done  double-click=delete");
        g.text(0, 1, &format!("New task: {}_", self.input));
        g.text(0, 2, &self.status);
        let dim = Px::rgb(0x80, 0x86, 0x94);
        let done_c = Px::rgb(0x3C, 0xA8, 0x4B);
        for (i, (done, text)) in self.tasks.iter().enumerate() {
            let row = i as u32 + 3;
            if row >= g.rows {
                break;
            }
            if *done {
                let line = format!("[x] {text}");
                g.write(0, row, "[x] ", done_c, g.bg());
                g.write(4, row, text, dim, g.bg());
                let _ = line;
            } else {
                g.text(0, row, &format!("[ ] {text}"));
            }
        }
        if self.tasks.is_empty() {
            g.text(0, 4, "(no tasks yet - type one above and press Enter)");
        }
    }
}

// ─── DrDr2048 — the sliding-tile game ────────────────────────────────

#[derive(Copy, Clone)]
enum Dir2048 {
    Left,
    Right,
    Up,
    Down,
}

/// 2048 on a 4×4 board. Arrow keys slide and merge tiles; R starts a new
/// game. Our own merge logic and a tiny xorshift PRNG — no crate.
pub struct Game2048 {
    board: [[u32; 4]; 4],
    score: u32,
    best: u32,
    rng: u64,
    over: bool,
    won: bool,
}

impl Game2048 {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2048_2048);
        let mut g = Self {
            board: [[0; 4]; 4],
            score: 0,
            best: 0,
            rng: seed.max(1),
            over: false,
            won: false,
        };
        g.spawn();
        g.spawn();
        g
    }

    fn restart(&mut self) {
        self.best = self.best.max(self.score);
        self.board = [[0; 4]; 4];
        self.score = 0;
        self.over = false;
        self.won = false;
        self.spawn();
        self.spawn();
    }

    fn xs(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn spawn(&mut self) {
        let empties: Vec<(usize, usize)> = (0..4)
            .flat_map(|y| (0..4).map(move |x| (x, y)))
            .filter(|&(x, y)| self.board[y][x] == 0)
            .collect();
        if empties.is_empty() {
            return;
        }
        let pick = (self.xs() as usize) % empties.len();
        let (x, y) = empties[pick];
        self.board[y][x] = if self.xs() % 10 == 0 { 4 } else { 2 };
    }

    /// The four cells of line `idx` in `dir`, ordered destination-first
    /// (slide collapses everything toward element 0).
    fn line_coords(idx: usize, dir: Dir2048) -> [(usize, usize); 4] {
        match dir {
            Dir2048::Left => [(0, idx), (1, idx), (2, idx), (3, idx)],
            Dir2048::Right => [(3, idx), (2, idx), (1, idx), (0, idx)],
            Dir2048::Up => [(idx, 0), (idx, 1), (idx, 2), (idx, 3)],
            Dir2048::Down => [(idx, 3), (idx, 2), (idx, 1), (idx, 0)],
        }
    }

    /// Collapse a line toward index 0, merging equal neighbours once.
    /// Returns the new line, the score gained, and whether anything moved.
    fn slide(row: [u32; 4]) -> ([u32; 4], u32, bool) {
        let vals: Vec<u32> = row.iter().copied().filter(|&v| v != 0).collect();
        let mut out: Vec<u32> = Vec::with_capacity(4);
        let mut gained = 0;
        let mut i = 0;
        while i < vals.len() {
            if i + 1 < vals.len() && vals[i] == vals[i + 1] {
                let merged = vals[i] * 2;
                out.push(merged);
                gained += merged;
                i += 2;
            } else {
                out.push(vals[i]);
                i += 1;
            }
        }
        let mut new = [0u32; 4];
        for (j, v) in out.iter().enumerate() {
            new[j] = *v;
        }
        (new, gained, new != row)
    }

    fn move_dir(&mut self, dir: Dir2048) -> bool {
        let mut moved = false;
        for idx in 0..4 {
            let coords = Self::line_coords(idx, dir);
            let row = [
                self.board[coords[0].1][coords[0].0],
                self.board[coords[1].1][coords[1].0],
                self.board[coords[2].1][coords[2].0],
                self.board[coords[3].1][coords[3].0],
            ];
            let (new, gained, m) = Self::slide(row);
            moved |= m;
            self.score += gained;
            for (k, &(x, y)) in coords.iter().enumerate() {
                self.board[y][x] = new[k];
                if new[k] >= 2048 {
                    self.won = true;
                }
            }
        }
        if moved {
            self.spawn();
            self.best = self.best.max(self.score);
            if !self.any_moves() {
                self.over = true;
            }
        }
        moved
    }

    fn any_moves(&self) -> bool {
        for y in 0..4 {
            for x in 0..4 {
                if self.board[y][x] == 0 {
                    return true;
                }
                if x + 1 < 4 && self.board[y][x] == self.board[y][x + 1] {
                    return true;
                }
                if y + 1 < 4 && self.board[y][x] == self.board[y + 1][x] {
                    return true;
                }
            }
        }
        false
    }

    fn tile_color(v: u32) -> Px {
        match v {
            2 => Px::rgb(0xEE, 0xE4, 0xDA),
            4 => Px::rgb(0xED, 0xE0, 0xC8),
            8 => Px::rgb(0xF2, 0xB1, 0x79),
            16 => Px::rgb(0xF5, 0x95, 0x63),
            32 => Px::rgb(0xF6, 0x7C, 0x5F),
            64 => Px::rgb(0xF6, 0x5E, 0x3B),
            128 => Px::rgb(0xED, 0xCF, 0x72),
            256 => Px::rgb(0xED, 0xCC, 0x61),
            512 => Px::rgb(0xED, 0xC8, 0x50),
            1024 => Px::rgb(0xED, 0xC5, 0x3F),
            _ => Px::rgb(0xED, 0xC2, 0x2E),
        }
    }
}

impl WindowApp for Game2048 {
    fn icon(&self) -> IconKind {
        IconKind::Dice2048
    }
    fn title(&self) -> String {
        format!("DrDr2048 - score {} (best {})", self.score, self.best.max(self.score))
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
            KeyCode::Left => {
                self.move_dir(Dir2048::Left);
            }
            KeyCode::Right => {
                self.move_dir(Dir2048::Right);
            }
            KeyCode::Up => {
                self.move_dir(Dir2048::Up);
            }
            KeyCode::Down => {
                self.move_dir(Dir2048::Down);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => self.restart(),
            _ => {}
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let dark = Px::rgb(0x2B, 0x27, 0x22);
        g.text(0, 0, &format!("Arrows = slide   R = new game    score {}", self.score));
        if self.won {
            g.write(0, 1, "You reached 2048!  keep going or press R", Px::rgb(0xF6, 0x5E, 0x3B), g.bg());
        } else if self.over {
            g.write(0, 1, "Game over - press R to try again", Px::rgb(0xF6, 0x5E, 0x3B), g.bg());
        } else {
            g.text(0, 1, "Merge equal tiles to reach 2048.");
        }
        // Each tile is a 6-wide × 3-tall block; the board starts at row 3.
        let tw = 6u32;
        let th = 3u32;
        for by in 0..4u32 {
            for bx in 0..4u32 {
                let v = self.board[by as usize][bx as usize];
                let ox = bx * tw;
                let oy = 3 + by * th;
                let (fill, fg) = if v == 0 {
                    (Px::rgb(0x3A, 0x35, 0x2F), g.bg())
                } else {
                    let f = Self::tile_color(v);
                    let text = if v <= 4 { dark } else { Px::rgb(0xFF, 0xFF, 0xFF) };
                    (f, text)
                };
                // paint the tile block
                for dy in 0..th {
                    for dx in 0..tw {
                        let cx = ox + dx;
                        let cy = oy + dy;
                        if cx < g.cols && cy < g.rows {
                            g.put(cx, cy, ' ', fg, fill);
                        }
                    }
                }
                if v != 0 {
                    let s = v.to_string();
                    let tx = ox + (tw.saturating_sub(s.len() as u32)) / 2;
                    let ty = oy + th / 2;
                    if ty < g.rows {
                        g.write(tx, ty, &s, fg, fill);
                    }
                }
            }
        }
    }
}

// ─── DrDrMines — Minesweeper ─────────────────────────────────────────

/// Classic Minesweeper, mouse-driven. Left-click reveals a cell;
/// double-click toggles a flag. The first click is always safe (mines are
/// placed afterwards, avoiding it), revealing flood-fills through empty
/// regions. Press R for a new board.
pub struct MinesApp {
    w: usize,
    h: usize,
    mines: usize,
    bomb: Vec<bool>,
    revealed: Vec<bool>,
    flagged: Vec<bool>,
    adj: Vec<u8>,
    placed: bool,
    over: bool,
    won: bool,
    rng: u64,
}

impl MinesApp {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x4D_1235_7E15);
        let (w, h, mines) = (16usize, 12usize, 30usize);
        let mut m = Self {
            w,
            h,
            mines,
            bomb: vec![false; w * h],
            revealed: vec![false; w * h],
            flagged: vec![false; w * h],
            adj: vec![0; w * h],
            placed: false,
            over: false,
            won: false,
            rng: seed.max(1),
        };
        m.reset();
        m
    }

    fn reset(&mut self) {
        let n = self.w * self.h;
        self.bomb = vec![false; n];
        self.revealed = vec![false; n];
        self.flagged = vec![false; n];
        self.adj = vec![0; n];
        self.placed = false;
        self.over = false;
        self.won = false;
    }

    fn xs(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.w + x
    }

    fn place_mines(&mut self, safe: usize) {
        let n = self.w * self.h;
        let mut placed = 0;
        while placed < self.mines && placed < n.saturating_sub(1) {
            let p = (self.xs() as usize) % n;
            if p == safe || self.bomb[p] {
                continue;
            }
            self.bomb[p] = true;
            placed += 1;
        }
        // Precompute adjacency counts.
        for y in 0..self.h {
            for x in 0..self.w {
                let mut c = 0u8;
                for (nx, ny) in self.neighbors(x, y) {
                    if self.bomb[self.idx(nx, ny)] {
                        c += 1;
                    }
                }
                let i = self.idx(x, y);
                self.adj[i] = c;
            }
        }
        self.placed = true;
    }

    fn neighbors(&self, x: usize, y: usize) -> Vec<(usize, usize)> {
        let mut v = Vec::with_capacity(8);
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                if nx >= 0 && ny >= 0 && (nx as usize) < self.w && (ny as usize) < self.h {
                    v.push((nx as usize, ny as usize));
                }
            }
        }
        v
    }

    fn reveal(&mut self, x: usize, y: usize) {
        if !self.placed {
            self.place_mines(self.idx(x, y));
        }
        let start = self.idx(x, y);
        if self.revealed[start] || self.flagged[start] {
            return;
        }
        if self.bomb[start] {
            self.over = true;
            // Reveal all bombs on loss.
            for i in 0..self.bomb.len() {
                if self.bomb[i] {
                    self.revealed[i] = true;
                }
            }
            return;
        }
        // Flood-fill through zero-adjacency cells.
        let mut stack = vec![(x, y)];
        while let Some((cx, cy)) = stack.pop() {
            let i = self.idx(cx, cy);
            if self.revealed[i] || self.flagged[i] {
                continue;
            }
            self.revealed[i] = true;
            if self.adj[i] == 0 && !self.bomb[i] {
                for (nx, ny) in self.neighbors(cx, cy) {
                    if !self.revealed[self.idx(nx, ny)] {
                        stack.push((nx, ny));
                    }
                }
            }
        }
        self.check_win();
    }

    fn check_win(&mut self) {
        let revealed = self.revealed.iter().filter(|&&r| r).count();
        if revealed == self.w * self.h - self.mines {
            self.won = true;
        }
    }

    fn flags_used(&self) -> usize {
        self.flagged.iter().filter(|&&f| f).count()
    }

    fn number_color(n: u8) -> Px {
        match n {
            1 => Px::rgb(0x42, 0x8A, 0xF0),
            2 => Px::rgb(0x3C, 0xA8, 0x4B),
            3 => Px::rgb(0xE0, 0x4F, 0x4F),
            4 => Px::rgb(0x6B, 0x4F, 0xC9),
            5 => Px::rgb(0xC0, 0x6A, 0x2A),
            6 => Px::rgb(0x36, 0xB9, 0xB0),
            7 => Px::rgb(0xC8, 0x36, 0x52),
            _ => Px::rgb(0x88, 0x88, 0x88),
        }
    }
}

impl WindowApp for MinesApp {
    fn icon(&self) -> IconKind {
        IconKind::Mine
    }
    fn title(&self) -> String {
        let state = if self.won {
            "  WON!"
        } else if self.over {
            "  BOOM"
        } else {
            ""
        };
        format!(
            "DrDrMines - {} mines, {} flagged{}",
            self.mines,
            self.flags_used(),
            state
        )
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if matches!(key, KeyCode::Char('r') | KeyCode::Char('R')) {
            self.reset();
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, double: bool) -> AppControl {
        if self.over || self.won {
            return AppControl::Continue;
        }
        // Board starts at row 2; cells are 1 char wide.
        if row < 2 {
            return AppControl::Continue;
        }
        let x = col as usize;
        let y = (row - 2) as usize;
        if x >= self.w || y >= self.h {
            return AppControl::Continue;
        }
        let i = self.idx(x, y);
        if double {
            // Toggle a flag (only on hidden cells).
            if !self.revealed[i] {
                self.flagged[i] = !self.flagged[i];
            }
        } else {
            self.reveal(x, y);
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let hint = if self.over {
            "BOOM - press R for a new board"
        } else if self.won {
            "Cleared! every safe cell found - press R"
        } else {
            "click = reveal   double-click = flag   R = new board"
        };
        g.text(0, 0, hint);
        g.text(
            0,
            1,
            &format!("mines {}   flags {}", self.mines, self.flags_used()),
        );
        let hidden = Px::rgb(0x6B, 0x73, 0x80);
        let open_bg = g.bg();
        for y in 0..self.h {
            let row = y as u32 + 2;
            if row >= g.rows {
                break;
            }
            for x in 0..self.w {
                let cx = x as u32;
                if cx >= g.cols {
                    break;
                }
                let i = self.idx(x, y);
                if self.flagged[i] && !self.revealed[i] {
                    g.put(cx, row, 'F', Px::rgb(0xE8, 0x4E, 0x4E), open_bg);
                } else if !self.revealed[i] {
                    g.put(cx, row, '#', open_bg, hidden);
                } else if self.bomb[i] {
                    g.put(cx, row, '*', Px::rgb(0xFF, 0xFF, 0xFF), Px::rgb(0xC8, 0x36, 0x52));
                } else if self.adj[i] == 0 {
                    g.put(cx, row, '.', Px::rgb(0x55, 0x55, 0x55), open_bg);
                } else {
                    let n = self.adj[i];
                    g.put(
                        cx,
                        row,
                        (b'0' + n) as char,
                        Self::number_color(n),
                        open_bg,
                    );
                }
            }
        }
    }
}

// ─── DrDrFetch — a system-info card ──────────────────────────────────

/// A "neofetch"-style card: a DrDrOS wordmark beside live system facts —
/// hostname, kernel, uptime, memory, CPU, process count, and where your
/// files are being saved. Refreshes on the heartbeat.
pub struct SysInfoApp;

impl SysInfoApp {
    pub fn new() -> Self {
        SysInfoApp
    }
}

fn first_line_field(path: &str, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.split_once(':') {
            if rest.0.trim() == key {
                return Some(rest.1.trim().to_string());
            }
        }
    }
    None
}

fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else {
        format!("{m}m {s}s")
    }
}

impl WindowApp for SysInfoApp {
    fn icon(&self) -> IconKind {
        IconKind::Info
    }
    fn title(&self) -> String {
        "System Info".into()
    }

    fn on_tick(&mut self) -> AppControl {
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let accent = Px::rgb(0x2D, 0x82, 0xF0);
        // A compact DrDrOS logo on the left.
        let logo = [
            "####  ####  ####  #### #### ####",
            "#   # #   # #   # #    #  # #   ",
            "#   # ####  #   # #### #  # ####",
            "#   # #  #  #   # #    #  #    #",
            "####  #   # ####  #### #### ####",
        ];
        for (i, l) in logo.iter().enumerate() {
            g.write(1, i as u32 + 1, l, accent, g.bg());
        }

        let host = fs::read_to_string("/etc/hostname")
            .or_else(|_| fs::read_to_string("/proc/sys/kernel/hostname"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "drdros".into());
        let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into());
        let cpu = first_line_field("/proc/cpuinfo", "model name")
            .unwrap_or_else(|| "unknown CPU".into());
        let mem_total = meminfo_kb("MemTotal");
        let mem_avail = meminfo_kb("MemAvailable");
        let mem_used = mem_total.saturating_sub(mem_avail);
        let up = fmt_uptime(uptime_secs());
        let procs = proc_count();
        let data = drdr_store::data_dir();
        let persist = if drdr_store::data_is_persistent() {
            "persistent disk"
        } else {
            "RAM (not saved across reboots)"
        };

        let mut row = 8u32;
        let mut line = |g: &mut TextGrid, k: &str, v: &str| {
            g.write(1, row, k, accent, g.bg());
            g.text(14, row, v);
            row += 1;
        };
        line(g, "Host", &host);
        line(g, "OS", "DrDrOS (custom Rust userland)");
        line(g, "Kernel", &format!("Linux {kernel}"));
        line(g, "Uptime", &up);
        line(g, "CPU", &cpu);
        line(
            g,
            "Memory",
            &format!(
                "{} / {} MiB used",
                mem_used / 1024,
                mem_total / 1024
            ),
        );
        line(g, "Processes", &procs.to_string());
        line(g, "Desktop", &format!("drdr-desk v{}", env!("CARGO_PKG_VERSION")));
        line(g, "Storage", persist);
        line(g, "Data dir", &data.display().to_string());
    }
}

// ─── File types, syntax highlighting & a clickable menu bar ──────────
//
// These three pieces are what make the editor and file manager feel like
// a real desktop: the file manager knows a `.png` from a `.rs`, the
// editor colours code by language, and a Windows-style menu bar offers
// File / Format / Colour actions with the mouse — no memorising keys.

/// Lowercased extension of a filename (`""` if none).
pub fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, e)) if !e.is_empty() => e.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Broad class of a file — the file manager uses it to pick how to open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileClass {
    Text,
    Code,
    Web,
    Image,
    Pdf,
    Archive,
    Media,
    Binary,
}

/// Classify a filename by extension.
pub fn classify(name: &str) -> FileClass {
    match ext_of(name).as_str() {
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "ppm" | "webp" | "ico" | "tiff" => {
            FileClass::Image
        }
        "pdf" => FileClass::Pdf,
        "zip" | "docx" | "doc" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "jar" | "apk"
        | "epub" => FileClass::Archive,
        "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "flv" | "wmv" | "mpg" | "mpeg"
        | "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "wma" => FileClass::Media,
        "html" | "htm" | "md" | "markdown" => FileClass::Web,
        "rs" | "js" | "ts" | "jsx" | "tsx" | "mjs" | "java" | "c" | "h" | "cpp" | "hpp"
        | "cc" | "py" | "go" | "css" | "json" | "xml" | "sh" | "bash" | "toml" | "yaml"
        | "yml" | "rb" | "php" | "sql" => FileClass::Code,
        "gz" | "xz" | "tar" | "bin" | "exe" | "o" | "so" | "wasm" => FileClass::Binary,
        _ => FileClass::Text,
    }
}

/// A short tag shown beside a file in the manager list.
pub fn type_tag(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "DIR";
    }
    match classify(name) {
        FileClass::Image => "IMG",
        FileClass::Pdf => "PDF",
        FileClass::Archive => "ZIP",
        FileClass::Media => "AV",
        FileClass::Web => "WEB",
        FileClass::Code => "<>",
        FileClass::Binary => "BIN",
        FileClass::Text => "TXT",
    }
}

// ── Syntax highlighting ──────────────────────────────────────────────

/// The languages the editor colours. `Plain` means "no highlighting".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Plain,
    Rust,
    JavaScript,
    Java,
    CLike,
    Python,
    Web,
    Css,
    Json,
    Shell,
}

/// Pick a language from a filename.
pub fn lang_for(name: &str) -> Lang {
    match ext_of(name).as_str() {
        "rs" => Lang::Rust,
        "js" | "ts" | "jsx" | "tsx" | "mjs" => Lang::JavaScript,
        "java" => Lang::Java,
        "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "go" => Lang::CLike,
        "py" => Lang::Python,
        "html" | "htm" | "xml" => Lang::Web,
        "css" => Lang::Css,
        "json" => Lang::Json,
        "sh" | "bash" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => Lang::Shell,
        _ => Lang::Plain,
    }
}

fn keywords(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &[
            "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait",
            "for", "while", "loop", "if", "else", "match", "return", "self", "Self",
            "const", "static", "ref", "move", "as", "in", "where", "type", "dyn", "crate",
            "true", "false", "Some", "None", "Ok", "Err", "Box", "Vec", "String",
        ],
        Lang::JavaScript => &[
            "function", "let", "const", "var", "if", "else", "for", "while", "return",
            "class", "new", "this", "import", "export", "from", "async", "await", "try",
            "catch", "throw", "typeof", "null", "undefined", "true", "false", "of",
        ],
        Lang::Java => &[
            "public", "private", "protected", "class", "interface", "void", "int", "long",
            "double", "float", "boolean", "char", "new", "return", "if", "else", "for",
            "while", "this", "static", "final", "import", "package", "extends", "implements",
            "true", "false", "null", "try", "catch",
        ],
        Lang::CLike => &[
            "int", "char", "void", "long", "short", "float", "double", "struct", "enum",
            "union", "const", "static", "return", "if", "else", "for", "while", "switch",
            "case", "break", "continue", "sizeof", "typedef", "unsigned", "signed", "func",
            "package", "import", "type",
        ],
        Lang::Python => &[
            "def", "class", "if", "elif", "else", "for", "while", "return", "import",
            "from", "as", "try", "except", "finally", "with", "lambda", "None", "True",
            "False", "and", "or", "not", "in", "is", "pass", "yield", "self",
        ],
        Lang::Web => &["html", "head", "body", "div", "span", "script", "style", "a", "p"],
        Lang::Css => &["color", "background", "margin", "padding", "border", "font", "display"],
        Lang::Json => &["true", "false", "null"],
        Lang::Shell => &[
            "if", "then", "else", "fi", "for", "do", "done", "while", "case", "esac",
            "function", "echo", "export", "true", "false",
        ],
        Lang::Plain => &[],
    }
}

/// Colour each character of `line` for `lang`; `fg` is the default. A
/// tiny single-line lexer: line comments, quoted strings, numbers and
/// keywords. Good enough to make code readable, cheap enough to run on
/// every visible line each frame.
pub fn highlight(line: &str, lang: Lang, fg: Px) -> Vec<Px> {
    let chars: Vec<char> = line.chars().collect();
    let mut col = vec![fg; chars.len()];
    if lang == Lang::Plain || chars.is_empty() {
        return col;
    }
    let kw = Px::rgb(0x9B, 0x6B, 0xF0);
    let strc = Px::rgb(0x2E, 0x9E, 0x4F);
    let numc = Px::rgb(0xCB, 0x7B, 0x2E);
    let comc = Px::rgb(0x8C, 0x8C, 0x96);
    let line_comment = match lang {
        Lang::Python | Lang::Shell => "#",
        Lang::Css | Lang::Json | Lang::Web => "",
        _ => "//",
    };
    let kws = keywords(lang);

    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        // line comment → colour to end of line.
        if !line_comment.is_empty()
            && chars[i..].iter().collect::<String>().starts_with(line_comment)
        {
            for cc in col.iter_mut().skip(i) {
                *cc = comc;
            }
            break;
        }
        // string literal.
        if c == '"' || c == '\'' || c == '`' {
            let quote = c;
            col[i] = strc;
            i += 1;
            while i < chars.len() {
                col[i] = strc;
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    if i < chars.len() {
                        col[i] = strc;
                    }
                }
                i += 1;
            }
            continue;
        }
        // number.
        if c.is_ascii_digit() && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            for cc in col.iter_mut().take(i).skip(start) {
                *cc = numc;
            }
            continue;
        }
        // identifier / keyword.
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if kws.contains(&word.as_str()) {
                for cc in col.iter_mut().take(i).skip(start) {
                    *cc = kw;
                }
            }
            continue;
        }
        i += 1;
    }
    col
}

// ── Menu bar ─────────────────────────────────────────────────────────

/// One row in a drop-down menu. `color` paints the label in its own
/// colour (used for the colour picker swatches).
struct MenuItem {
    label: String,
    action: &'static str,
    color: Option<Px>,
}

/// A top-level menu: a title plus its drop-down rows.
struct Menu {
    title: String,
    items: Vec<MenuItem>,
}

/// What a click on the menu bar did.
pub enum MenuClick {
    /// An item fired; here is its action id.
    Action(&'static str),
    /// The bar handled the click (toggled a menu) — don't treat as a
    /// document click.
    Consumed,
    /// The click missed the bar; the app should handle it normally.
    Passthrough,
}

/// A Windows-style clickable menu bar that renders into the top row of a
/// [`TextGrid`] and pops a drop-down below the open menu.
pub struct MenuBar {
    menus: Vec<Menu>,
    open: Option<usize>,
}

impl MenuBar {
    fn item(label: &str, action: &'static str) -> MenuItem {
        MenuItem { label: label.into(), action, color: None }
    }
    fn swatch(label: &str, action: &'static str, c: Px) -> MenuItem {
        MenuItem { label: label.into(), action, color: Some(c) }
    }

    /// Column where menu `idx`'s padded title starts.
    fn title_start(&self, idx: usize) -> u32 {
        let mut c = 1u32;
        for m in &self.menus[..idx] {
            c += m.title.chars().count() as u32 + 2 + 1; // " title " + gap
        }
        c
    }

    fn title_at(&self, col: u32) -> Option<usize> {
        for i in 0..self.menus.len() {
            let s = self.title_start(i);
            let e = s + self.menus[i].title.chars().count() as u32 + 2;
            if col >= s && col < e {
                return Some(i);
            }
        }
        None
    }

    fn dropdown_w(&self, i: usize) -> u32 {
        let longest = self.menus[i]
            .items
            .iter()
            .map(|it| it.label.chars().count())
            .max()
            .unwrap_or(4) as u32;
        longest + 4
    }

    /// True while a drop-down is showing (the app suppresses typing then).
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Close any open drop-down.
    pub fn close(&mut self) {
        self.open = None;
    }

    /// Paint the bar (row 0) and, if open, its drop-down. `fg`/`bg` are
    /// the grid's text/background; the bar reverses them so it reads as
    /// chrome.
    pub fn render(&self, g: &mut TextGrid, fg: Px, bg: Px) {
        // Toolbar strip across the top in reverse video.
        g.fill_row(0, bg, fg);
        for (i, m) in self.menus.iter().enumerate() {
            let s = self.title_start(i);
            let label = format!(" {} ", m.title);
            if self.open == Some(i) {
                g.write(s, 0, &label, fg, bg); // highlighted = normal video
            } else {
                g.write(s, 0, &label, bg, fg); // reverse
            }
        }
        if let Some(i) = self.open {
            let s = self.title_start(i);
            let w = self.dropdown_w(i);
            for (j, it) in self.menus[i].items.iter().enumerate() {
                let row = 1 + j as u32;
                for c in 0..w {
                    g.put(s + c, row, ' ', fg, bg);
                }
                let lc = it.color.unwrap_or(fg);
                g.write(s + 2, row, &it.label, lc, bg);
            }
        }
    }

    /// Route a grid click. Row 0 toggles menus; a click inside the open
    /// drop-down fires an action; anything else closes the menu.
    pub fn on_click(&mut self, col: u32, row: u32) -> MenuClick {
        if row == 0 {
            self.open = match self.title_at(col) {
                Some(i) if self.open == Some(i) => None,
                other => other,
            };
            return MenuClick::Consumed;
        }
        if let Some(i) = self.open {
            let s = self.title_start(i);
            let w = self.dropdown_w(i);
            let n = self.menus[i].items.len() as u32;
            if row >= 1 && row <= n && col >= s && col < s + w {
                let action = self.menus[i].items[(row - 1) as usize].action;
                self.open = None;
                return MenuClick::Action(action);
            }
            self.open = None;
            return MenuClick::Passthrough;
        }
        MenuClick::Passthrough
    }
}

/// Build the editor's menu bar. Colour swatches are painted in their own
/// hue so the picker reads at a glance.
fn editor_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "File".into(),
                items: vec![
                    MenuBar::item("New", "new"),
                    MenuBar::item("Save", "save"),
                    MenuBar::item("Save As", "saveas"),
                    MenuBar::item("Close", "close"),
                ],
            },
            Menu {
                title: "Format".into(),
                items: vec![
                    MenuBar::item("Bigger text", "size_up"),
                    MenuBar::item("Smaller text", "size_down"),
                    MenuBar::item("Colour this line", "colorline"),
                    MenuBar::item("Reset colours", "color_reset"),
                ],
            },
            Menu {
                title: "Colour".into(),
                items: vec![
                    MenuBar::item("Default", "ink_default"),
                    MenuBar::swatch("Red", "ink_red", Px::rgb(0xD0, 0x3A, 0x3A)),
                    MenuBar::swatch("Orange", "ink_orange", Px::rgb(0xD9, 0x7A, 0x1E)),
                    MenuBar::swatch("Green", "ink_green", Px::rgb(0x2E, 0x9E, 0x4F)),
                    MenuBar::swatch("Teal", "ink_teal", Px::rgb(0x1E, 0x9E, 0x9E)),
                    MenuBar::swatch("Blue", "ink_blue", Px::rgb(0x2D, 0x6C, 0xD8)),
                    MenuBar::swatch("Purple", "ink_purple", Px::rgb(0x9B, 0x5C, 0xE0)),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle syntax", "syntax"),
                    MenuBar::item("Toggle theme", "theme"),
                ],
            },
        ],
    }
}

/// Map a colour action id to its ink (None = default text colour).
fn ink_for_action(a: &str) -> Option<Px> {
    match a {
        "ink_red" => Some(Px::rgb(0xD0, 0x3A, 0x3A)),
        "ink_orange" => Some(Px::rgb(0xD9, 0x7A, 0x1E)),
        "ink_green" => Some(Px::rgb(0x2E, 0x9E, 0x4F)),
        "ink_teal" => Some(Px::rgb(0x1E, 0x9E, 0x9E)),
        "ink_blue" => Some(Px::rgb(0x2D, 0x6C, 0xD8)),
        "ink_purple" => Some(Px::rgb(0x9B, 0x5C, 0xE0)),
        _ => None,
    }
}

// ─── DrDrBrowser — a real (http) web + local document browser ────────

/// Where a clickable link points.
enum Link {
    Local(PathBuf),
    Web(String),
}

/// A from-scratch browser. It renders local HTML/Markdown **and** fetches
/// real `http://` pages over our own tiny HTTP client ([`crate::http`]),
/// with an editable address bar, a menu bar (Go / View), and Back
/// history. `https://` is refused with an honest "no TLS stack" message;
/// there is no JS engine — it's a readable document browser, not Chrome.
pub struct BrowserApp {
    /// The rendered lines (text + colour) of the current page.
    lines: Vec<(String, Px)>,
    /// Clickable targets keyed by absolute line index.
    links: Vec<(usize, Link)>,
    title: String,
    /// The address-bar text (a URL or "home").
    addr: String,
    /// True while the user is typing into the address bar.
    editing: bool,
    /// Visited addresses, for Back.
    history: Vec<String>,
    status: String,
    menu: MenuBar,
    scroll: usize,
    spawns: Vec<Spawn>,
}

impl BrowserApp {
    pub fn new() -> Self {
        let mut a = Self {
            lines: Vec::new(),
            links: Vec::new(),
            title: "Home".into(),
            addr: "home".into(),
            editing: false,
            history: Vec::new(),
            status: "type a URL (http://…) and press Enter".into(),
            menu: browser_menu(),
            scroll: 0,
            spawns: Vec::new(),
        };
        a.home();
        a
    }

    pub fn open(path: PathBuf) -> Self {
        let mut a = Self::new();
        a.load(&path);
        a
    }

    fn home(&mut self) {
        self.title = "Home".into();
        self.addr = "home".into();
        self.links.clear();
        let accent = Px::rgb(0x2D, 0x6C, 0xD8);
        let fg = Px::rgb(0x1B, 0x1B, 0x1B);
        let mut lines = vec![
            ("DrDrBrowser".to_string(), accent),
            ("A browser for http:// pages, plus local HTML/Markdown.".to_string(), fg),
            ("Type a URL in the bar above (e.g. http://example.com).".to_string(), fg),
            (String::new(), fg),
            ("Your Documents:".to_string(), fg),
        ];
        let docs = drdr_store::documents_dir();
        let mut links: Vec<(usize, Link)> = Vec::new();
        if let Ok(rd) = fs::read_dir(&docs) {
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for n in names {
                links.push((lines.len(), Link::Local(docs.join(&n))));
                lines.push((format!("  -> {n}"), accent));
            }
        }
        if links.is_empty() {
            lines.push(("  (no documents yet)".to_string(), Px::rgb(0x8C, 0x8C, 0x8C)));
        }
        self.lines = lines;
        self.links = links;
        self.scroll = 0;
        self.status = "home".into();
    }

    fn load(&mut self, path: &std::path::Path) {
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.title = name.clone();
        self.addr = path.to_string_lossy().into_owned();
        self.links.clear();
        self.scroll = 0;
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                self.lines = vec![(format!("cannot open {name}: {e}"), Px::rgb(0xD0, 0x3A, 0x3A))];
                return;
            }
        };
        self.lines = match ext_of(&name).as_str() {
            "html" | "htm" => render_html(&text),
            _ => render_markdown(&text),
        };
        self.status = format!("loaded {name}");
    }

    /// Fetch and render a real web page over HTTP.
    fn go_web(&mut self, url: &str) {
        self.scroll = 0;
        self.links.clear();
        match crate::http::fetch(url) {
            Ok(page) => {
                self.addr = page.final_url.clone();
                self.title = page.final_url.clone();
                let mut lines = if page.content_type.contains("text/html")
                    || page.content_type.is_empty()
                {
                    render_html(&page.body)
                } else {
                    page.body.lines().map(|l| (l.to_string(), Px::rgb(0x1B, 0x1B, 0x1B))).collect()
                };
                // Append a clickable link index harvested from the HTML.
                let weblinks = extract_links(&page.body, &page.final_url);
                if !weblinks.is_empty() {
                    let accent = Px::rgb(0x1E, 0x6E, 0xC0);
                    lines.push((String::new(), accent));
                    lines.push(("-- Links on this page --".to_string(), accent));
                    for (label, target) in weblinks.into_iter().take(80) {
                        self.links.push((lines.len(), Link::Web(target)));
                        lines.push((format!("  -> {label}"), accent));
                    }
                }
                self.lines = lines;
                self.status = format!("{} ({} lines)", page.status, self.lines.len());
            }
            Err(e) => {
                self.addr = url.to_string();
                self.title = "Error".into();
                self.lines = e
                    .lines()
                    .map(|l| (l.to_string(), Px::rgb(0xD0, 0x3A, 0x3A)))
                    .collect();
                self.status = "request failed".into();
            }
        }
    }

    /// Decide whether the address is a web URL or a local path and go.
    fn navigate(&mut self, input: &str) {
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        // Remember where we were so Back works.
        if self.addr != input {
            self.history.push(self.addr.clone());
        }
        if input == "home" {
            self.home();
        } else if input.starts_with("http://") || input.starts_with("https://") {
            self.go_web(&input);
        } else if looks_like_domain(&input) {
            self.go_web(&format!("http://{input}"));
        } else {
            self.load(std::path::Path::new(&input));
        }
    }

    fn back(&mut self) {
        if let Some(prev) = self.history.pop() {
            let cur = self.addr.clone();
            // navigate() would re-push; call the destination directly.
            self.addr = prev.clone();
            if prev == "home" {
                self.home();
            } else if prev.starts_with("http") {
                self.go_web(&prev);
            } else {
                self.load(std::path::Path::new(&prev));
            }
            let _ = cur;
        } else {
            self.status = "no history".into();
        }
    }

    fn activate_link(&mut self, line_idx: usize) {
        let target = self.links.iter().find(|(i, _)| *i == line_idx);
        match target {
            Some((_, Link::Web(url))) => {
                let url = url.clone();
                self.navigate(&url);
            }
            Some((_, Link::Local(path))) => {
                let path = path.clone();
                match classify(&path.to_string_lossy()) {
                    FileClass::Image => self.spawns.push(Spawn {
                        rect: spawn_rect(),
                        app: Box::new(ImageApp::open(path)),
                    }),
                    FileClass::Web | FileClass::Text => self.load(&path),
                    _ => self.spawns.push(Spawn {
                        rect: spawn_rect(),
                        app: Box::new(EditApp::new(path)),
                    }),
                }
            }
            None => {}
        }
    }

    fn do_action(&mut self, action: &str) -> AppControl {
        match action {
            "back" => self.back(),
            "reload" => {
                let a = self.addr.clone();
                self.go_web_or_local(&a);
            }
            "home" => self.home(),
            "edit_addr" => self.editing = true,
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }

    /// Reload helper that doesn't touch history.
    fn go_web_or_local(&mut self, addr: &str) {
        if addr == "home" {
            self.home();
        } else if addr.starts_with("http") || looks_like_domain(addr) {
            let url = if addr.starts_with("http") { addr.to_string() } else { format!("http://{addr}") };
            self.go_web(&url);
        } else {
            self.load(std::path::Path::new(addr));
        }
    }
}

impl WindowApp for BrowserApp {
    fn icon(&self) -> IconKind {
        IconKind::Browser
    }
    fn title(&self) -> String {
        format!("Browser - {}", self.title)
    }

    fn take_spawns(&mut self) -> Vec<Spawn> {
        std::mem::take(&mut self.spawns)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        // Address-bar editing captures input until Enter / Esc.
        if self.editing {
            match key {
                KeyCode::Char(c) => self.addr.push(c),
                KeyCode::Space => self.addr.push(' '),
                KeyCode::Backspace => {
                    self.addr.pop();
                }
                KeyCode::Enter => {
                    self.editing = false;
                    let a = self.addr.clone();
                    self.navigate(&a);
                }
                KeyCode::Escape => self.editing = false,
                _ => {}
            }
            return AppControl::Continue;
        }
        let page = self.lines.len().saturating_sub(1);
        match key {
            KeyCode::Down => self.scroll = (self.scroll + 1).min(page),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Space => self.scroll = (self.scroll + 10).min(page),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home => self.scroll = 0,
            KeyCode::Backspace => self.back(),
            KeyCode::Char('g') => self.editing = true, // 'g' = go to address
            KeyCode::Char('h') => self.home(),
            KeyCode::Escape => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _double: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        // Row 1 is the address bar — click to edit it.
        if row == 1 {
            self.editing = true;
            return AppControl::Continue;
        }
        // Content starts at row 2.
        if row >= 2 {
            let line_idx = self.scroll + (row as usize - 2);
            self.activate_link(line_idx);
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let fg = g.fg();
        let bg = g.bg();
        let accent = Px::rgb(0x2D, 0x6C, 0xD8);
        // Row 1: the address bar (reverse-video field), with status to its
        // right. A caret shows while editing.
        let bar = if self.editing {
            format!("{}_", self.addr)
        } else {
            self.addr.clone()
        };
        let label = "URL: ";
        g.write(0, 1, label, accent, bg);
        let field_x = label.len() as u32;
        let maxc = g.cols.saturating_sub(field_x + 1) as usize;
        let shown: String = bar.chars().rev().take(maxc).collect::<Vec<_>>().into_iter().rev().collect();
        // A subtle field background.
        for c in field_x..g.cols {
            g.put(c, 1, ' ', fg, bg.lerp(accent, 18));
        }
        g.write(field_x, 1, &shown, fg, bg.lerp(accent, 18));

        // Content from row 2.
        let rows = g.rows.saturating_sub(2) as usize;
        for vis in 0..rows {
            let li = self.scroll + vis;
            if li >= self.lines.len() {
                break;
            }
            let (text, color) = &self.lines[li];
            g.write(0, vis as u32 + 2, text, *color, bg);
        }

        // Status into the last row's tail if room (kept subtle).
        if g.rows >= 2 {
            let s = format!(" {} ", self.status);
            let sx = g.cols.saturating_sub(s.chars().count() as u32);
            g.write(sx, 1, &s, g.fg(), bg.lerp(accent, 18));
        }

        // Menu bar painted last so its drop-down overlays the page.
        self.menu.render(g, fg, bg);
    }
}

/// True if the string looks like a bare domain (so we can prepend http://).
fn looks_like_domain(s: &str) -> bool {
    !s.contains(' ')
        && !s.starts_with('/')
        && !s.starts_with('.')
        && s.contains('.')
        && s.split('.').next_back().map(|t| t.len() >= 2).unwrap_or(false)
        && !std::path::Path::new(s).exists()
}

/// Harvest `<a href="…">label</a>` pairs from HTML into absolute URLs.
fn extract_links(html: &str, base: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("<a ") {
        let tag_start = i + rel;
        let Some(tag_end_rel) = lower[tag_start..].find('>') else { break };
        let tag_end = tag_start + tag_end_rel;
        let tag = &html[tag_start..tag_end];
        // Pull href="…" or href='…'.
        if let Some(href) = attr_value(tag, "href") {
            // The link text runs until the closing </a>.
            let after = tag_end + 1;
            let label_end = lower[after..].find("</a>").map(|e| after + e).unwrap_or(after);
            let raw_label = strip_tags(&html[after..label_end]);
            let label = raw_label.trim();
            let label = if label.is_empty() { href.clone() } else { label.to_string() };
            if !href.starts_with('#') && !href.starts_with("javascript:") {
                out.push((label, crate::http::resolve(base, &href)));
            }
            i = label_end + 4;
        } else {
            i = tag_end + 1;
        }
        let _ = bytes;
    }
    out
}

/// Extract a quoted attribute value from a tag fragment.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let key = format!("{name}=");
    let at = lower.find(&key)? + key.len();
    let rest = &tag[at..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)? + 1;
        Some(rest[1..end].to_string())
    } else {
        // Unquoted: up to whitespace or end.
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Strip any tags from a small HTML fragment (for link labels).
fn strip_tags(frag: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in frag.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The browser's menu bar.
fn browser_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Go".into(),
                items: vec![
                    MenuBar::item("Back", "back"),
                    MenuBar::item("Reload", "reload"),
                    MenuBar::item("Home", "home"),
                    MenuBar::item("Edit address", "edit_addr"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

/// Render HTML to coloured text lines: strip tags, surface headings and
/// links, collapse whitespace. A deliberately small parser — enough to
/// read a hand-written page, not a full DOM.
fn render_html(src: &str) -> Vec<(String, Px)> {
    let fg = Px::rgb(0x1B, 0x1B, 0x1B);
    let head = Px::rgb(0x2D, 0x6C, 0xD8);
    let link = Px::rgb(0x1E, 0x6E, 0xC0);
    let mut out: Vec<(String, Px)> = Vec::new();
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut cur = String::new();
    let mut color = fg;
    let mut in_script = false;
    let flush = |out: &mut Vec<(String, Px)>, cur: &mut String, color: Px| {
        let t = cur.trim();
        if !t.is_empty() {
            out.push((t.to_string(), color));
        }
        cur.clear();
    };
    while i < bytes.len() {
        if bytes[i] == '<' {
            // read the tag name
            let mut j = i + 1;
            let mut tag = String::new();
            while j < bytes.len() && bytes[j] != '>' {
                tag.push(bytes[j]);
                j += 1;
            }
            let lower = tag.to_ascii_lowercase();
            let name: String = lower
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect();
            match name.as_str() {
                "script" | "style" => in_script = !lower.starts_with('/'),
                "h1" | "h2" | "h3" | "h4" => {
                    flush(&mut out, &mut cur, color);
                    color = head;
                }
                "br" | "p" | "div" | "li" | "tr" | "ul" | "ol" => {
                    flush(&mut out, &mut cur, color);
                    color = fg;
                }
                "a" => {
                    // surface href as a trailing marker
                    if let Some(h) = extract_attr(&lower, "href") {
                        cur.push_str(" [");
                        cur.push_str(&h);
                        cur.push(']');
                    }
                    color = link;
                }
                _ => {}
            }
            if name == "h1" || name == "h2" || name == "h3" || name == "h4" {
                if lower.starts_with('/') {
                    flush(&mut out, &mut cur, head);
                    color = fg;
                }
            }
            i = j + 1;
            continue;
        }
        if !in_script {
            cur.push(bytes[i]);
        }
        i += 1;
    }
    flush(&mut out, &mut cur, color);
    if out.is_empty() {
        out.push(("(empty page)".to_string(), fg));
    }
    out
}

fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=");
    let pos = tag.find(&key)? + key.len();
    let rest = &tag[pos..];
    let rest = rest.trim_start_matches(['"', '\'']);
    let end = rest.find(['"', '\'', ' ']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Render Markdown to coloured lines: headings emphasised, list bullets
/// kept, `*`/`_`/`` ` `` markers stripped.
fn render_markdown(src: &str) -> Vec<(String, Px)> {
    let fg = Px::rgb(0x1B, 0x1B, 0x1B);
    let head = Px::rgb(0x2D, 0x6C, 0xD8);
    let code = Px::rgb(0x2E, 0x9E, 0x4F);
    let mut out = Vec::new();
    for raw in src.split('\n') {
        let line = raw.trim_end();
        if let Some(h) = line.trim_start().strip_prefix('#') {
            let title = h.trim_start_matches('#').trim();
            out.push((title.to_uppercase(), head));
        } else if line.trim_start().starts_with("```") {
            out.push(("----".to_string(), code));
        } else {
            let cleaned: String = line.chars().filter(|&c| c != '*' && c != '`' && c != '_').collect();
            out.push((cleaned, fg));
        }
    }
    if out.is_empty() {
        out.push(("(empty)".to_string(), fg));
    }
    out
}

// ─── Network & Wi-Fi panel ───────────────────────────────────────────

struct Iface {
    name: String,
    state: String,
    mac: String,
    wireless: bool,
    carrier: bool,
}

/// A read-only network status panel: it enumerates the kernel's network
/// interfaces from `/sys/class/net`, flags which are wireless, shows
/// DrDrNet's LAN peers, and explains how to bring Wi-Fi up. Real data,
/// honest about what the current build can and can't do.
/// Which screen of the Network panel is showing.
#[derive(PartialEq, Eq)]
enum NetView {
    Interfaces,
    Wifi,
    Password,
}

pub struct NetworkApp {
    net: SharedNet,
    ifaces: Vec<Iface>,
    sel: usize,
    status: String,
    view: NetView,
    wifi_iface: Option<String>,
    networks: Vec<crate::wifi::Network>,
    wifi_status: crate::wifi::WifiStatus,
    msg: String,
    pw: String,
    pw_ssid: String,
    /// Countdown after a scan request before results are read; also the
    /// periodic status-refresh timer.
    scan_ticks: u8,
    tick: u8,
}

impl NetworkApp {
    pub fn new(net: SharedNet) -> Self {
        let mut a = Self {
            net,
            ifaces: Vec::new(),
            sel: 0,
            status: String::new(),
            view: NetView::Interfaces,
            wifi_iface: None,
            networks: Vec::new(),
            wifi_status: Default::default(),
            msg: String::new(),
            pw: String::new(),
            pw_ssid: String::new(),
            scan_ticks: 0,
            tick: 0,
        };
        a.reload();
        a
    }

    fn reload(&mut self) {
        self.ifaces = list_ifaces();
        self.sel = self.sel.min(self.ifaces.len().saturating_sub(1));
        self.wifi_iface = crate::wifi::wireless_ifaces().into_iter().next();
        self.status = format!("{} interface(s)", self.ifaces.len());
    }

    /// Enter the Wi-Fi view and kick off a scan.
    fn start_wifi(&mut self) {
        let Some(iface) = self.wifi_iface.clone() else {
            self.msg = "no Wi-Fi radio found".into();
            return;
        };
        self.view = NetView::Wifi;
        self.sel = 0;
        self.msg = match crate::wifi::trigger_scan(&iface) {
            Ok(()) => "scanning...".into(),
            Err(e) => e,
        };
        self.scan_ticks = 4; // ~1s before reading results
    }

    fn do_connect(&mut self, ssid: &str, psk: &str) {
        let Some(iface) = self.wifi_iface.clone() else { return };
        self.msg = match crate::wifi::connect(&iface, ssid, psk) {
            Ok(()) => format!("connecting to {ssid}..."),
            Err(e) => e,
        };
        self.view = NetView::Wifi;
    }
}

fn list_ifaces() -> Vec<Iface> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            let rd = |f: &str| fs::read_to_string(p.join(f)).map(|s| s.trim().to_string());
            let state = rd("operstate").unwrap_or_else(|_| "unknown".into());
            let mac = rd("address").unwrap_or_default();
            let wireless = p.join("wireless").exists() || p.join("phy80211").exists();
            let carrier = rd("carrier").map(|s| s == "1").unwrap_or(false);
            v.push(Iface { name, state, mac, wireless, carrier });
        }
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

impl WindowApp for NetworkApp {
    fn icon(&self) -> IconKind {
        IconKind::Network
    }
    fn title(&self) -> String {
        match self.view {
            NetView::Interfaces => "Network & Wi-Fi".into(),
            _ => format!(
                "Wi-Fi{}",
                if self.wifi_status.connected() {
                    format!(" - {}", self.wifi_status.ssid)
                } else {
                    String::new()
                }
            ),
        }
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match self.view {
            NetView::Interfaces => match key {
                KeyCode::Up => self.sel = self.sel.saturating_sub(1),
                KeyCode::Down => self.sel = (self.sel + 1).min(self.ifaces.len().saturating_sub(1)),
                KeyCode::Char('r') => self.reload(),
                KeyCode::Char('w') | KeyCode::Enter => self.start_wifi(),
                _ => {}
            },
            NetView::Wifi => match key {
                KeyCode::Up => self.sel = self.sel.saturating_sub(1),
                KeyCode::Down => self.sel = (self.sel + 1).min(self.networks.len().saturating_sub(1)),
                KeyCode::Char('s') => self.start_wifi(),
                KeyCode::Backspace | KeyCode::Left => self.view = NetView::Interfaces,
                KeyCode::Enter => {
                    if let Some(n) = self.networks.get(self.sel).cloned() {
                        if n.is_open() {
                            self.do_connect(&n.ssid, "");
                        } else {
                            self.pw.clear();
                            self.pw_ssid = n.ssid.clone();
                            self.view = NetView::Password;
                        }
                    }
                }
                _ => {}
            },
            NetView::Password => match key {
                KeyCode::Char(c) => self.pw.push(c),
                KeyCode::Space => self.pw.push(' '),
                KeyCode::Backspace => {
                    self.pw.pop();
                }
                KeyCode::Enter => {
                    let (ssid, pw) = (self.pw_ssid.clone(), std::mem::take(&mut self.pw));
                    self.do_connect(&ssid, &pw);
                }
                KeyCode::Escape => self.view = NetView::Wifi,
                _ => {}
            },
        }
        AppControl::Continue
    }

    fn on_tick(&mut self) -> AppControl {
        // Only touch wpa_supplicant from the Wi-Fi screens (keeps the
        // Interfaces view — and the host snapshot — free of subprocesses).
        if self.view == NetView::Interfaces {
            return AppControl::Continue;
        }
        if self.scan_ticks > 0 {
            self.scan_ticks -= 1;
            if self.scan_ticks == 0 {
                if let Some(iface) = &self.wifi_iface {
                    self.networks = crate::wifi::scan_results(iface);
                    self.msg = format!("{} network(s) found", self.networks.len());
                }
            }
        }
        self.tick = self.tick.wrapping_add(1);
        if self.tick % 8 == 0 {
            if let Some(iface) = &self.wifi_iface {
                self.wifi_status = crate::wifi::status(iface);
            }
        }
        AppControl::Continue
    }

    fn on_click(&mut self, _c: u32, row: u32, double: bool) -> AppControl {
        if self.view == NetView::Wifi && row >= 3 {
            let idx = (row - 3) as usize;
            if idx < self.networks.len() {
                self.sel = idx;
                if double {
                    return self.on_key(KeyCode::Enter);
                }
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let teal = Px::rgb(0x2B, 0x9B, 0x8A);
        let muted = Px::rgb(0x8C, 0x8C, 0x96);
        let red = Px::rgb(0xC8, 0x2B, 0x2B);
        let green = Px::rgb(0x2E, 0x9E, 0x4F);
        match self.view {
            NetView::Interfaces => {
                g.write(0, 0, "Network interfaces   (w = scan Wi-Fi, r = rescan)", teal, g.bg());
                g.text(0, 1, "NAME        TYPE   STATE     LINK  MAC");
                let mut row = 2u32;
                for (i, f) in self.ifaces.iter().enumerate() {
                    let kind = if f.wireless { "wifi" } else if f.name == "lo" { "loop" } else { "lan" };
                    let link = if f.carrier { "up" } else { "down" };
                    let line = format!("{:<11} {:<6} {:<9} {:<5} {}", f.name, kind, f.state, link, f.mac);
                    if i == self.sel {
                        selected(g, row, &line);
                    } else {
                        g.text(0, row, &line);
                    }
                    row += 1;
                }
                if self.ifaces.is_empty() {
                    g.write(0, row, "(no interfaces found)", red, g.bg());
                    row += 1;
                }
                row += 1;
                match net_snapshot(&self.net) {
                    Some(net) => {
                        let peers = net.directory.lock().map(|d| d.snapshot()).unwrap_or_default();
                        g.write(0, row, &format!("DrDrNet: online, {} LAN peer(s)", peers.len()), teal, g.bg());
                    }
                    None => {
                        g.write(0, row, "DrDrNet: starting...", muted, g.bg());
                    }
                }
                row += 2;
                match &self.wifi_iface {
                    Some(w) => {
                        g.write(0, row, &format!("Wi-Fi radio: {w}  -  press 'w' to scan and connect"), green, g.bg());
                    }
                    None => {
                        g.write(0, row, "No Wi-Fi radio. Wired Ethernet auto-connects via DHCP.", muted, g.bg());
                    }
                }
            }
            NetView::Wifi => {
                let iface = self.wifi_iface.clone().unwrap_or_default();
                g.write(0, 0, &format!("Wi-Fi on {iface}   (Enter=connect  s=rescan  Backspace=back)"), teal, g.bg());
                if self.wifi_status.connected() {
                    g.write(0, 1, &format!("Connected: {}  IP {}", self.wifi_status.ssid, self.wifi_status.ip), green, g.bg());
                } else if !self.msg.is_empty() {
                    g.write(0, 1, &self.msg, muted, g.bg());
                }
                g.text(0, 2, "SIGNAL  SECURITY  SSID");
                let visible = (g.rows as usize).saturating_sub(3);
                for (i, n) in self.networks.iter().take(visible).enumerate() {
                    let bars: String = (0..4).map(|b| if (b as u8) < n.bars() { '|' } else { '.' }).collect();
                    let lock = if n.is_open() { "open" } else { "lock" };
                    let line = format!("[{bars}]  {:<6} {}", lock, n.ssid);
                    let row = i as u32 + 3;
                    if i == self.sel {
                        selected(g, row, &line);
                    } else {
                        g.text(0, row, &line);
                    }
                }
                if self.networks.is_empty() {
                    g.write(0, 3, "(no networks yet - press 's' to scan)", muted, g.bg());
                }
            }
            NetView::Password => {
                g.write(0, 0, &format!("Connect to: {}", self.pw_ssid), teal, g.bg());
                let stars: String = std::iter::repeat_n('*', self.pw.chars().count()).collect();
                g.text(0, 2, &format!("Password: {stars}_"));
                g.write(0, 4, "Enter = connect    Esc = cancel", muted, g.bg());
                g.text(0, 6, "WPA2/WPA3 is handled by wpa_supplicant; DrDrOS");
                g.text(0, 7, "writes the config and brings the link up, then");
                g.text(0, 8, "udhcpc pulls an address.");
            }
        }
        if !self.msg.is_empty() && self.view != NetView::Password {
            g.write(0, g.rows.saturating_sub(1), &self.msg, muted, g.bg());
        }
    }
}

// ─── Image viewer ────────────────────────────────────────────────────

/// A decoded image as row-major pixels.
struct DecodedImg {
    w: u32,
    h: u32,
    px: Vec<Px>,
}

/// A real image viewer. It decodes **PNG, GIF and baseline JPEG** (via
/// our own `drdr-codec`), plus PPM (P6) and uncompressed 24/32-bit BMP,
/// and paints them as colour cells (each grid cell is one down-sampled
/// pixel — the closest a character grid gets to a bitmap), aspect-corrected
/// for the 8×16 cell.
pub struct ImageApp {
    name: String,
    img: Option<DecodedImg>,
    info: Vec<String>,
    /// Show the metadata/details list instead of the picture.
    show_info: bool,
    menu: MenuBar,
}

impl ImageApp {
    pub fn open(path: PathBuf) -> Self {
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = fs::read(&path).unwrap_or_default();
        let (img, info) = decode_image(&name, &bytes);
        Self { name, img, info, show_info: false, menu: image_menu() }
    }

    fn do_action(&mut self, a: &str) -> AppControl {
        match a {
            "picture" => self.show_info = false,
            "details" => self.show_info = true,
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }
}

impl WindowApp for ImageApp {
    fn icon(&self) -> IconKind {
        IconKind::Image
    }
    fn title(&self) -> String {
        format!("Image - {}", self.name)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        match key {
            KeyCode::Escape => return AppControl::Close,
            KeyCode::Char('i') => self.show_info = !self.show_info,
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let fg = g.fg();
        let bg = g.bg();
        // Row 0 = menu bar; row 1 = caption; picture/details fill the rest.
        let no_pic = self.img.is_none();
        if self.show_info || no_pic {
            let dim = self
                .img
                .as_ref()
                .map(|im| format!("{}   {}x{}   (press 'i' for the picture)", self.name, im.w, im.h))
                .unwrap_or_else(|| format!("{}   (no preview available)", self.name));
            g.text(0, 1, &dim);
            for (i, l) in self.info.iter().enumerate() {
                let row = i as u32 + 3;
                if row >= g.rows {
                    break;
                }
                g.text(1, row, l);
            }
            if self.info.is_empty() && !no_pic {
                g.write(1, 3, "A decoded picture - choose Image > Picture to view it.", fg, bg);
            }
        } else if let Some(img) = &self.img {
            g.text(0, 1, &format!("{}   {}x{}   (press 'i' for details)", self.name, img.w, img.h));
            let avail_cols = g.cols.max(1);
            let avail_rows = g.rows.saturating_sub(2).max(1);
            // Fit aspect: cells are 8 wide, 16 tall, so a column is half
            // the physical span of a row — halve the row count.
            let ar = img.w as f32 / img.h as f32;
            let mut uc = avail_cols;
            let mut ur = (((uc * 8) as f32 / ar) / 16.0).round() as u32;
            if ur > avail_rows {
                ur = avail_rows;
                uc = (((ur * 16) as f32 * ar) / 8.0).round() as u32;
            }
            uc = uc.clamp(1, avail_cols);
            ur = ur.clamp(1, avail_rows);
            let ox = (avail_cols - uc) / 2;
            let oy = 2 + (avail_rows - ur) / 2;
            for ry in 0..ur {
                for cx in 0..uc {
                    let ix = (cx * img.w / uc).min(img.w - 1);
                    let iy = (ry * img.h / ur).min(img.h - 1);
                    let c = img.px[(iy * img.w + ix) as usize];
                    g.put(ox + cx, oy + ry, ' ', c, c);
                }
            }
        }
        // Menu bar last so its drop-down overlays the picture.
        self.menu.render(g, fg, bg);
    }
}

/// The image viewer's menu bar: switch between the picture and its
/// decoded details, and the usual view toggles.
fn image_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Image".into(),
                items: vec![
                    MenuBar::item("Picture", "picture"),
                    MenuBar::item("Details", "details"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

/// Convert a `drdr_codec` RGBA image into the viewer's `Px` buffer,
/// compositing any alpha over white so transparent PNG/GIF read cleanly.
fn img_from_codec(img: drdr_codec::Image) -> DecodedImg {
    let mut px = Vec::with_capacity((img.w * img.h) as usize);
    for p in img.rgba.chunks_exact(4) {
        let (r, g, b, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
        let over = |c: u32| ((c * a + 255 * (255 - a)) / 255) as u8;
        px.push(Px::rgb(over(r), over(g), over(b)));
    }
    DecodedImg { w: img.w, h: img.h, px }
}

/// Sniff a file's magic bytes and decode (or describe) it. PNG, GIF and
/// baseline JPEG are decoded for real by our own `drdr-codec`; PPM and
/// BMP have their own small decoders here.
fn decode_image(name: &str, b: &[u8]) -> (Option<DecodedImg>, Vec<String>) {
    if b.len() >= 2 && &b[0..2] == b"P6" {
        if let Some(img) = decode_ppm(b) {
            return (Some(img), vec![]);
        }
    }
    if b.len() >= 2 && &b[0..2] == b"BM" {
        if let Some(img) = decode_bmp(b) {
            return (Some(img), vec![]);
        }
    }
    if b.len() >= 8 && &b[0..8] == b"\x89PNG\r\n\x1a\n" {
        return match drdr_codec::decode_png(b) {
            Ok(img) => (Some(img_from_codec(img)), vec![]),
            Err(e) => (None, vec![format!("{name}: {e}")]),
        };
    }
    if b.len() >= 6 && (&b[0..6] == b"GIF87a" || &b[0..6] == b"GIF89a") {
        return match drdr_codec::decode_gif(b) {
            Ok(img) => (Some(img_from_codec(img)), vec![]),
            Err(e) => (None, vec![format!("{name}: {e}")]),
        };
    }
    if b.len() >= 2 && b[0] == 0xFF && b[1] == 0xD8 {
        return match drdr_codec::decode_jpeg(b) {
            Ok(img) => (Some(img_from_codec(img)), vec![]),
            Err(e) => (
                None,
                vec![
                    format!("{name}: {e}"),
                    String::new(),
                    "(baseline JPEG decodes; progressive is not supported yet)".into(),
                ],
            ),
        };
    }
    (
        None,
        vec![
            format!("{name}: unrecognised image ({} bytes)", b.len()),
            "Supported: PNG, GIF, baseline JPEG, BMP, PPM.".into(),
        ],
    )
}

fn decode_ppm(b: &[u8]) -> Option<DecodedImg> {
    let mut pos = 2usize;
    let mut tok = || -> Option<u32> {
        // skip whitespace and # comments
        loop {
            while pos < b.len() && (b[pos] as char).is_whitespace() {
                pos += 1;
            }
            if pos < b.len() && b[pos] == b'#' {
                while pos < b.len() && b[pos] != b'\n' {
                    pos += 1;
                }
            } else {
                break;
            }
        }
        let start = pos;
        while pos < b.len() && b[pos].is_ascii_digit() {
            pos += 1;
        }
        std::str::from_utf8(&b[start..pos]).ok()?.parse().ok()
    };
    let w = tok()?;
    let h = tok()?;
    let maxv = tok()?;
    if w == 0 || h == 0 || maxv == 0 {
        return None;
    }
    pos += 1; // single whitespace after maxval
    let need = (w * h * 3) as usize;
    if pos + need > b.len() {
        return None;
    }
    let mut px = Vec::with_capacity((w * h) as usize);
    for i in 0..(w * h) as usize {
        let o = pos + i * 3;
        px.push(Px::rgb(b[o], b[o + 1], b[o + 2]));
    }
    Some(DecodedImg { w, h, px })
}

fn decode_bmp(b: &[u8]) -> Option<DecodedImg> {
    if b.len() < 54 || &b[0..2] != b"BM" {
        return None;
    }
    let u32le = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let i32le = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let u16le = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let data_off = u32le(10) as usize;
    let w = i32le(18);
    let h = i32le(22);
    let bpp = u16le(28);
    let compression = u32le(30);
    if compression != 0 || !(bpp == 24 || bpp == 32) || w == 0 || h == 0 {
        return None;
    }
    let bytespp = (bpp / 8) as usize;
    let width = w.unsigned_abs();
    let height = h.unsigned_abs();
    let topdown = h < 0;
    let row_size = ((bpp as usize * width as usize + 31) / 32) * 4;
    if width > 8192 || height > 8192 {
        return None;
    }
    let mut px = vec![Px::BLACK; (width * height) as usize];
    for row in 0..height {
        let src_y = if topdown { row } else { height - 1 - row };
        let ro = data_off + src_y as usize * row_size;
        for x in 0..width {
            let o = ro + x as usize * bytespp;
            if o + 2 >= b.len() {
                continue;
            }
            px[(row * width + x) as usize] = Px::rgb(b[o + 2], b[o + 1], b[o]);
        }
    }
    Some(DecodedImg { w: width, h: height, px })
}

// ─── Binary file info ────────────────────────────────────────────────

/// A safe, read-only viewer for binary files (PDF, DOCX, archives…): it
/// never tries to interpret them as text (which would risk an empty
/// "new file" overwrite), just reports size and a hex preview.
pub struct BinaryInfoApp {
    name: String,
    size: u64,
    head: Vec<u8>,
}

impl BinaryInfoApp {
    pub fn open(path: PathBuf) -> Self {
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = fs::read(&path).unwrap_or_default();
        let size = bytes.len() as u64;
        let head = bytes.into_iter().take(256).collect();
        Self { name, size, head }
    }
}

impl WindowApp for BinaryInfoApp {
    fn icon(&self) -> IconKind {
        IconKind::Document
    }
    fn title(&self) -> String {
        format!("File info - {}", self.name)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if key == KeyCode::Escape {
            return AppControl::Close;
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        g.text(0, 0, &format!("{}  ({} bytes)", self.name, self.size));
        g.text(0, 1, "This is a binary file. Hex preview of the first bytes:");
        let mut row = 3u32;
        for chunk in self.head.chunks(16) {
            if row >= g.rows {
                break;
            }
            let mut hex = String::new();
            let mut asc = String::new();
            for byte in chunk {
                hex.push_str(&format!("{byte:02x} "));
                asc.push(if byte.is_ascii_graphic() { *byte as char } else { '.' });
            }
            g.text(0, row, &format!("{hex:<48} {asc}"));
            row += 1;
        }
    }
}

// ─── A scrollable read-only text pane (shared by the doc viewers) ────

/// The common machinery behind the PDF / archive / media viewers: a list
/// of lines you can scroll with the arrows / PageUp-Down, Esc to close.
struct TextPane {
    lines: Vec<String>,
    scroll: usize,
}

impl TextPane {
    fn new(lines: Vec<String>) -> Self {
        Self { lines, scroll: 0 }
    }

    fn on_key(&mut self, key: KeyCode, page: usize) -> AppControl {
        match key {
            KeyCode::Escape => return AppControl::Close,
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll += 1,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Space => self.scroll += page,
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.lines.len(),
            _ => {}
        }
        AppControl::Continue
    }

    /// Paint from `top` row, returning nothing. Clamps the scroll so the
    /// last page always shows content.
    fn render(&mut self, g: &mut TextGrid, top: u32) {
        let body = g.rows.saturating_sub(top) as usize;
        let max_scroll = self.lines.len().saturating_sub(body);
        self.scroll = self.scroll.min(max_scroll);
        for (i, line) in self.lines.iter().skip(self.scroll).take(body).enumerate() {
            g.text(0, top + i as u32, line);
        }
    }
}

/// Strip XML tags to readable text, turning paragraph/break closers into
/// newlines and decoding the handful of entities Office documents use.
/// Good enough to surface the words in a `.docx` / `.xlsx` / `.pptx`.
fn xml_to_text(xml: &str) -> String {
    let mut out = String::new();
    let mut chars = xml.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '<' {
            // Read the tag name to decide whether it ends a paragraph.
            let rest = &xml[i..];
            let end = rest.find('>').map(|e| i + e + 1).unwrap_or(xml.len());
            let tag = &xml[i..end];
            if tag.starts_with("</w:p")
                || tag.starts_with("</a:p")
                || tag.starts_with("</text:p")
                || tag.starts_with("<w:br")
                || tag.starts_with("</tr")
            {
                out.push('\n');
            }
            // Skip to the end of the tag.
            while let Some(&(j, _)) = chars.peek() {
                if j >= end {
                    break;
                }
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Pull readable text out of an Office Open XML container (docx/xlsx/pptx)
/// already opened as a ZIP. Returns `None` if it isn't an office file.
fn office_text(data: &[u8], entries: &[drdr_codec::ZipEntry]) -> Option<String> {
    // The part that holds the body text differs per app.
    let part = entries.iter().find(|e| {
        e.name == "word/document.xml"
            || e.name == "xl/sharedStrings.xml"
            || e.name == "ppt/slides/slide1.xml"
    })?;
    let raw = drdr_codec::read_zip_entry(data, part).ok()?;
    let xml = String::from_utf8_lossy(&raw);
    Some(xml_to_text(&xml))
}

// ─── PDF viewer ──────────────────────────────────────────────────────

/// Opens a PDF and shows its extracted text (via our own `drdr-codec`
/// PDF text extractor — DEFLATE-decompressing content streams and pulling
/// the strings out). Not a full renderer; an honest, readable text view.
pub struct PdfApp {
    name: String,
    pane: TextPane,
}

impl PdfApp {
    pub fn open(path: PathBuf) -> Self {
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = fs::read(&path).unwrap_or_default();
        let lines = match drdr_codec::pdf::extract_pdf_text(&bytes) {
            Ok(ls) if !ls.is_empty() => ls,
            Ok(_) => vec!["(no extractable text — likely a scanned/image PDF)".into()],
            Err(e) => vec![format!("Could not read PDF: {e}")],
        };
        Self { name, pane: TextPane::new(lines) }
    }
}

impl WindowApp for PdfApp {
    fn icon(&self) -> IconKind {
        IconKind::Document
    }
    fn title(&self) -> String {
        format!("PDF - {}", self.name)
    }
    fn on_key(&mut self, key: KeyCode) -> AppControl {
        let page = 10;
        self.pane.on_key(key, page)
    }
    fn render(&mut self, g: &mut TextGrid) {
        g.text(0, 0, &format!("{}   (arrows/PageUp-Down scroll, Esc close)", self.name));
        self.pane.render(g, 2);
    }
}

// ─── Archive / Office document viewer ────────────────────────────────

/// Lists a ZIP's entries and, when it's an Office document, shows the
/// extracted body text underneath — all via our own ZIP reader + DEFLATE.
pub struct ArchiveApp {
    name: String,
    pane: TextPane,
}

impl ArchiveApp {
    pub fn open(path: PathBuf) -> Self {
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = fs::read(&path).unwrap_or_default();
        let mut lines = Vec::new();
        match drdr_codec::list_zip(&bytes) {
            Ok(entries) => {
                if let Some(text) = office_text(&bytes, &entries) {
                    lines.push("-- Document text --".into());
                    for l in text.lines() {
                        let l = l.trim_end();
                        if !l.is_empty() {
                            lines.push(l.to_string());
                        }
                    }
                    lines.push(String::new());
                }
                lines.push(format!("-- {} entries --", entries.len()));
                for e in &entries {
                    lines.push(format!(
                        "{:>9} B  {}",
                        e.uncomp_size,
                        e.name
                    ));
                }
            }
            Err(e) => lines.push(format!("Could not read archive: {e}")),
        }
        Self { name, pane: TextPane::new(lines) }
    }
}

impl WindowApp for ArchiveApp {
    fn icon(&self) -> IconKind {
        IconKind::Folder
    }
    fn title(&self) -> String {
        format!("Archive - {}", self.name)
    }
    fn on_key(&mut self, key: KeyCode) -> AppControl {
        self.pane.on_key(key, 10)
    }
    fn render(&mut self, g: &mut TextGrid) {
        g.text(0, 0, &format!("{}   (arrows scroll, Esc close)", self.name));
        self.pane.render(g, 2);
    }
}

// ─── Media (audio/video) info ────────────────────────────────────────

/// Shows what a video/audio file *is* — container, duration, resolution,
/// codecs, tracks — parsed from MP4 / Matroska metadata by our own
/// `drdr-codec`. It does not decode compressed frames (an honest line in
/// the panel says so); it's the "properties" view a desktop should give.
pub struct MediaApp {
    name: String,
    pane: TextPane,
}

impl MediaApp {
    pub fn open(path: PathBuf) -> Self {
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = fs::read(&path).unwrap_or_default();
        let info = drdr_codec::probe_media(&bytes, &ext_of(&name));
        Self { name, pane: TextPane::new(info.summary()) }
    }
}

impl WindowApp for MediaApp {
    fn icon(&self) -> IconKind {
        IconKind::Music
    }
    fn title(&self) -> String {
        format!("Media - {}", self.name)
    }
    fn on_key(&mut self, key: KeyCode) -> AppControl {
        self.pane.on_key(key, 6)
    }
    fn render(&mut self, g: &mut TextGrid) {
        g.text(0, 0, &format!("{}   (Esc close)", self.name));
        self.pane.render(g, 2);
    }
}

// ─── DrDrMessages — a WhatsApp-style LAN messenger ───────────────────

/// Conversation rail width, in character cells.
const MSG_SIDEBAR_W: u32 = 20;
/// WhatsApp-ish brand greens.
const MSG_GREEN: Px = Px::rgb(0x25, 0xD3, 0x66);
const MSG_GREEN_DK: Px = Px::rgb(0x07, 0x5E, 0x54);

/// A modern two-pane messenger riding the DrDrNet chat bus. The left rail
/// lists conversations — the LAN broadcast "Everyone", then every peer the
/// discovery directory knows — and the right pane is the thread, drawn as
/// chat bubbles (your own lines right-aligned in green, others left in
/// grey) with a composer at the bottom. Same wire protocol as DrDrChat,
/// dressed up like a phone messenger.
pub struct MessagesApp {
    net: SharedNet,
    input: String,
    /// 0 = "Everyone"; otherwise the 1-based index into the peer list.
    sel: usize,
    menu: MenuBar,
}

impl MessagesApp {
    pub fn new(net: SharedNet) -> Self {
        Self { net, input: String::new(), sel: 0, menu: messenger_menu() }
    }

    fn do_action(&mut self, a: &str) -> AppControl {
        match a {
            "everyone" => self.sel = 0,
            "clear" => self.input.clear(),
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }

    fn send(&mut self) {
        let Some(net) = net_snapshot(&self.net) else {
            self.input.clear();
            return;
        };
        let text = std::mem::take(&mut self.input);
        fanout_chat(&net, text);
    }
}

impl WindowApp for MessagesApp {
    fn icon(&self) -> IconKind {
        IconKind::Messages
    }
    fn title(&self) -> String {
        match net_snapshot(&self.net) {
            None => "Messages - connecting...".into(),
            Some(net) => {
                let n = net.directory.lock().map(|d| d.len()).unwrap_or(0);
                format!("Messages - {} ({} online)", net.me.host, n)
            }
        }
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        match key {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Space => self.input.push(' '),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.send(),
            KeyCode::Escape => self.input.clear(),
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = self.sel.saturating_add(1), // clamped at render
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        // Sidebar click selects a conversation (row 1 is the "Chats" head).
        if col < MSG_SIDEBAR_W && row >= 2 {
            self.sel = (row - 2) as usize;
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let cols = g.cols;
        let rows = g.rows;
        if cols < MSG_SIDEBAR_W + 10 || rows < 5 {
            g.text(0, 1, "window too small");
            return;
        }
        let dark = theme_is_dark();
        // WhatsApp-ish palette: beige thread, green outgoing, white incoming.
        let (thread_bg, in_bg, in_fg, out_bg, out_fg, sub) = if dark {
            (Px::rgb(0x07, 0x0C, 0x0B), Px::rgb(0x20, 0x2C, 0x33), Px::rgb(0xE8, 0xEC, 0xF4),
             Px::rgb(0x12, 0x5C, 0x4A), Px::WHITE, Px::rgb(0x9A, 0xB0, 0xA6))
        } else {
            (Px::rgb(0xEC, 0xE5, 0xDD), Px::WHITE, Px::rgb(0x11, 0x11, 0x11),
             Px::rgb(0xD9, 0xFD, 0xD3), Px::rgb(0x07, 0x2A, 0x14), Px::rgb(0x6B, 0x6B, 0x6B))
        };
        let surface = g.bg();
        let fg = g.fg();

        let me = net_snapshot(&self.net);
        let me_host = me.as_ref().map(|n| n.me.host.clone()).unwrap_or_else(|| "me".into());
        let peers: Vec<String> = me
            .as_ref()
            .and_then(|n| n.directory.lock().ok().map(|d| d.snapshot()))
            .unwrap_or_default()
            .iter()
            .map(|p| p.peer.host.clone())
            .collect();

        let mut convos: Vec<String> = vec!["Everyone".to_string()];
        convos.extend(peers.iter().cloned());
        if self.sel >= convos.len() {
            self.sel = convos.len() - 1;
        }

        // ── Left rail ────────────────────────────────────────────────
        for r in 0..rows {
            fill_cells(g, 0, r, MSG_SIDEBAR_W, fg, surface);
        }
        fill_cells(g, 0, 1, MSG_SIDEBAR_W, Px::WHITE, MSG_GREEN_DK);
        g.write(1, 1, "Chats", Px::WHITE, MSG_GREEN_DK);
        for (i, name) in convos.iter().enumerate() {
            let row = 2 + i as u32;
            if row >= rows {
                break;
            }
            let on = i == self.sel;
            let (cfg, cbg) = if on { (surface, MSG_GREEN) } else { (fg, surface) };
            if on {
                fill_cells(g, 0, row, MSG_SIDEBAR_W, cfg, cbg);
            }
            let dot = if i == 0 { '*' } else { 'o' };
            g.put(1, row, dot, if on { surface } else { MSG_GREEN }, cbg);
            let shown: String = name.chars().take((MSG_SIDEBAR_W - 4) as usize).collect();
            g.write(3, row, &shown, cfg, cbg);
        }
        for r in 0..rows {
            g.put(MSG_SIDEBAR_W, r, ' ', sub, surface);
        }

        // ── Thread pane ──────────────────────────────────────────────
        let tx0 = MSG_SIDEBAR_W + 1;
        let tw = cols - tx0;
        for r in 0..rows {
            fill_cells(g, tx0, r, tw, in_fg, thread_bg);
        }
        fill_cells(g, tx0, 1, tw, Px::WHITE, MSG_GREEN_DK);
        let htitle: String = convos[self.sel].chars().take((tw - 14) as usize).collect();
        g.write(tx0 + 1, 1, &htitle, Px::WHITE, MSG_GREEN_DK);
        let status = if me.is_none() {
            "connecting"
        } else if self.sel == 0 {
            "LAN broadcast"
        } else {
            "online"
        };
        let sw = status.chars().count() as u32;
        if tw > sw + 2 {
            g.write(tx0 + tw - sw - 1, 1, status, Px::rgb(0xCF, 0xF5, 0xE0), MSG_GREEN_DK);
        }

        // Filter the shared log for the selected conversation, stripping
        // any "#channel " tag so DrDrCord traffic still reads cleanly.
        let log = me
            .as_ref()
            .and_then(|n| n.chat_log.lock().ok().map(|l| l.clone()))
            .unwrap_or_default();
        let msgs: Vec<(String, String, u64)> = log
            .iter()
            .filter_map(|m| {
                let (_, body) = parse_channel(&m.text);
                let keep = self.sel == 0 || m.from == convos[self.sel] || m.from == me_host;
                keep.then(|| (m.from.clone(), body.to_string(), m.ts_unix_secs))
            })
            .collect();

        let area_top = 2u32;
        let area_bot = rows.saturating_sub(1); // composer on the last row
        let area_h = area_bot.saturating_sub(area_top);
        let inner_w = (tw as usize).saturating_sub(6).max(6);
        let mut brows: Vec<BubbleRow> = Vec::new();
        for (from, body, ts) in &msgs {
            let mine = *from == me_host;
            let show_name = !mine && self.sel == 0;
            let lines = wrap_text(body, inner_w);
            let mut content_w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            if show_name {
                content_w = content_w.max(from.chars().count());
            }
            let time = fmt_hhmm(*ts);
            content_w = content_w.max(time.chars().count());
            let bw = (content_w as u32 + 2).min(tw.saturating_sub(2));
            let bbg = if mine { out_bg } else { in_bg };
            let tfg = if mine { out_fg } else { in_fg };
            let x0 = if mine { tx0 + (tw - bw).saturating_sub(1) } else { tx0 + 1 };
            if show_name {
                brows.push((x0, bw, bbg, vec![(1, from.clone(), user_color(from))]));
            }
            for l in &lines {
                brows.push((x0, bw, bbg, vec![(1, l.clone(), tfg)]));
            }
            let toff = bw.saturating_sub(time.chars().count() as u32 + 1);
            brows.push((x0, bw, bbg, vec![(toff, time, sub)]));
            brows.push((0, 0, thread_bg, Vec::new())); // gap between bubbles
        }

        if msgs.is_empty() {
            let hint = "No messages yet - say hi below";
            let hw = hint.chars().count() as u32;
            if tw > hw {
                g.write(tx0 + (tw - hw) / 2, area_top + area_h / 2, hint, sub, thread_bg);
            }
        } else {
            let start = brows.len().saturating_sub(area_h as usize);
            let vis = &brows[start..];
            let first = area_bot.saturating_sub(vis.len() as u32);
            for (i, (x0, bw, bbg, runs)) in vis.iter().enumerate() {
                let row = first + i as u32;
                if row < area_top || row >= area_bot {
                    continue;
                }
                if *bw > 0 {
                    fill_cells(g, *x0, row, *bw, *bbg, *bbg);
                }
                for (off, text, rfg) in runs {
                    g.write(x0 + off, row, text, *rfg, *bbg);
                }
            }
        }

        // ── Composer (last row) ──────────────────────────────────────
        let crow = rows - 1;
        fill_cells(g, tx0, crow, tw, in_fg, in_bg);
        let ph = self.input.is_empty();
        let text = if ph { "Type a message".to_string() } else { format!("{}_", self.input) };
        let tfg = if ph { sub } else { in_fg };
        let shown: String = text.chars().take((tw - 2) as usize).collect();
        g.write(tx0 + 1, crow, &shown, tfg, in_bg);

        // Menu bar owns row 0 (drawn last so its drop-down overlays).
        self.menu.render(g, fg, surface);
    }
}

fn messenger_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Chat".into(),
                items: vec![
                    MenuBar::item("Everyone", "everyone"),
                    MenuBar::item("Clear box", "clear"),
                    MenuBar::item("Close", "close"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![MenuBar::item("Toggle theme", "theme")],
            },
        ],
    }
}

// ─── DrDrCord — a Discord-style channels + members chat ──────────────

const DISCORD_CHANNELS: [&str; 4] = ["general", "random", "help", "dev"];
const DCORD_RAIL_W: u32 = 14;
const DCORD_MEMBERS_W: u32 = 16;
const DCORD_BLURPLE: Px = Px::rgb(0x58, 0x65, 0xF2);

/// Split a LAN chat line into (channel index, body). A line another
/// DrDrCord client tagged `#chan ` routes to that channel; everything else
/// (DrDrChat / Messages traffic) lands in #general.
fn parse_channel(text: &str) -> (usize, &str) {
    if let Some(rest) = text.strip_prefix('#') {
        if let Some(sp) = rest.find(' ') {
            let chan = &rest[..sp];
            if let Some(i) = DISCORD_CHANNELS.iter().position(|c| *c == chan) {
                return (i, rest[sp + 1..].trim_start());
            }
        }
    }
    (0, text)
}

/// A Discord-style three-pane chat: a channel rail, the message stream for
/// the active channel, and an online-members list — over the same DrDrNet
/// bus as DrDrChat, with channels carried as a `#chan ` text tag so other
/// DrDrOS machines drop into the same rooms. Always dark, like the real thing.
pub struct DiscordApp {
    net: SharedNet,
    input: String,
    chan: usize,
    menu: MenuBar,
}

impl DiscordApp {
    pub fn new(net: SharedNet) -> Self {
        Self { net, input: String::new(), chan: 0, menu: discord_menu() }
    }
    fn do_action(&mut self, a: &str) -> AppControl {
        match a {
            "theme" => toggle_theme(),
            "clear" => self.input.clear(),
            "close" => return AppControl::Close,
            _ => {
                if let Some(i) = a.strip_prefix("chan").and_then(|s| s.parse::<usize>().ok()) {
                    if i < DISCORD_CHANNELS.len() {
                        self.chan = i;
                    }
                }
            }
        }
        AppControl::Continue
    }
    fn send(&mut self) {
        let Some(net) = net_snapshot(&self.net) else {
            self.input.clear();
            return;
        };
        let text = std::mem::take(&mut self.input);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        fanout_chat(&net, format!("#{} {}", DISCORD_CHANNELS[self.chan], text));
    }
}

impl WindowApp for DiscordApp {
    fn icon(&self) -> IconKind {
        IconKind::Discord
    }
    fn title(&self) -> String {
        format!("DrDrCord - #{}", DISCORD_CHANNELS[self.chan])
    }
    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        match key {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Space => self.input.push(' '),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.send(),
            KeyCode::Escape => self.input.clear(),
            KeyCode::Up => self.chan = self.chan.saturating_sub(1),
            KeyCode::Down => self.chan = (self.chan + 1).min(DISCORD_CHANNELS.len() - 1),
            _ => {}
        }
        AppControl::Continue
    }
    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        if col < DCORD_RAIL_W && row >= 2 {
            let i = (row - 2) as usize;
            if i < DISCORD_CHANNELS.len() {
                self.chan = i;
            }
        }
        AppControl::Continue
    }
    fn render(&mut self, g: &mut TextGrid) {
        let cols = g.cols;
        let rows = g.rows;
        if cols < DCORD_RAIL_W + DCORD_MEMBERS_W + 12 || rows < 5 {
            g.text(0, 1, "window too small");
            return;
        }
        // Discord is always dark — its own palette, independent of theme.
        let rail_bg = Px::rgb(0x1E, 0x1F, 0x22);
        let chat_bg = Px::rgb(0x31, 0x33, 0x38);
        let mem_bg = Px::rgb(0x2B, 0x2D, 0x31);
        let head_bg = Px::rgb(0x2B, 0x2D, 0x31);
        let comp_bg = Px::rgb(0x40, 0x44, 0x4B);
        let white = Px::rgb(0xF2, 0xF3, 0xF5);
        let muted = Px::rgb(0x96, 0x9B, 0xA3);

        let me = net_snapshot(&self.net);
        let me_host = me.as_ref().map(|n| n.me.host.clone()).unwrap_or_else(|| "me".into());
        let peers: Vec<String> = me
            .as_ref()
            .and_then(|n| n.directory.lock().ok().map(|d| d.snapshot()))
            .unwrap_or_default()
            .iter()
            .map(|p| p.peer.host.clone())
            .collect();

        // ── Channel rail ─────────────────────────────────────────────
        for r in 0..rows {
            fill_cells(g, 0, r, DCORD_RAIL_W, white, rail_bg);
        }
        fill_cells(g, 0, 1, DCORD_RAIL_W, white, DCORD_BLURPLE);
        g.write(1, 1, "DrDrOS", white, DCORD_BLURPLE);
        for (i, ch) in DISCORD_CHANNELS.iter().enumerate() {
            let row = 2 + i as u32;
            if row >= rows {
                break;
            }
            let on = i == self.chan;
            let (cfg, cbg) = if on { (white, Px::rgb(0x40, 0x44, 0x4B)) } else { (muted, rail_bg) };
            if on {
                fill_cells(g, 0, row, DCORD_RAIL_W, cfg, cbg);
            }
            g.write(1, row, &format!("# {ch}"), cfg, cbg);
        }

        // ── Members pane (right) ─────────────────────────────────────
        let mx0 = cols - DCORD_MEMBERS_W;
        for r in 0..rows {
            fill_cells(g, mx0, r, DCORD_MEMBERS_W, white, mem_bg);
        }
        g.write(mx0 + 1, 1, &format!("Online - {}", peers.len() + 1), muted, mem_bg);
        let mut mrow = 2u32;
        g.put(mx0 + 1, mrow, '*', MSG_GREEN, mem_bg);
        g.write(mx0 + 3, mrow, &me_host, user_color(&me_host), mem_bg);
        mrow += 1;
        for p in &peers {
            if mrow >= rows {
                break;
            }
            g.put(mx0 + 1, mrow, '*', MSG_GREEN, mem_bg);
            let shown: String = p.chars().take((DCORD_MEMBERS_W - 4) as usize).collect();
            g.write(mx0 + 3, mrow, &shown, user_color(p), mem_bg);
            mrow += 1;
        }

        // ── Chat column (middle) ─────────────────────────────────────
        let cx0 = DCORD_RAIL_W;
        let cw = mx0 - cx0;
        for r in 0..rows {
            fill_cells(g, cx0, r, cw, white, chat_bg);
        }
        fill_cells(g, cx0, 1, cw, white, head_bg);
        g.write(cx0 + 1, 1, &format!("# {}", DISCORD_CHANNELS[self.chan]), white, head_bg);

        let log = me
            .as_ref()
            .and_then(|n| n.chat_log.lock().ok().map(|l| l.clone()))
            .unwrap_or_default();
        let area_top = 2u32;
        let area_bot = rows - 1;
        let inner_w = (cw as usize).saturating_sub(2).max(6);
        let mut painted: Vec<(u32, String, Px)> = Vec::new(); // (offset, text, fg)
        for m in &log {
            let (ci, body) = parse_channel(&m.text);
            if ci != self.chan {
                continue;
            }
            painted.push((
                1,
                format!("{}  {}", m.from, fmt_hhmm(m.ts_unix_secs)),
                user_color(&m.from),
            ));
            for l in wrap_text(body, inner_w) {
                painted.push((1, l, white));
            }
        }
        let area_h = area_bot.saturating_sub(area_top);
        if painted.is_empty() {
            g.write(cx0 + 2, area_top + 1, &format!("Welcome to #{}!", DISCORD_CHANNELS[self.chan]), muted, chat_bg);
            g.write(cx0 + 2, area_top + 2, "This is the start of the channel.", muted, chat_bg);
        } else {
            let start = painted.len().saturating_sub(area_h as usize);
            for (i, (off, text, tfg)) in painted[start..].iter().enumerate() {
                let row = area_top + i as u32;
                if row >= area_bot {
                    break;
                }
                let shown: String = text.chars().take((cw - off - 1) as usize).collect();
                g.write(cx0 + off, row, &shown, *tfg, chat_bg);
            }
        }

        // Composer
        let crow = rows - 1;
        fill_cells(g, cx0, crow, cw, white, comp_bg);
        let ph = self.input.is_empty();
        let text = if ph { format!("Message #{}", DISCORD_CHANNELS[self.chan]) } else { format!("{}_", self.input) };
        let tfg = if ph { muted } else { white };
        let shown: String = text.chars().take((cw - 2) as usize).collect();
        g.write(cx0 + 1, crow, &shown, tfg, comp_bg);

        // Menu bar (its own dark colours so it blends with the app).
        self.menu.render(g, rail_bg, white);
    }
}

fn discord_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Channels".into(),
                items: vec![
                    MenuBar::item("# general", "chan0"),
                    MenuBar::item("# random", "chan1"),
                    MenuBar::item("# help", "chan2"),
                    MenuBar::item("# dev", "chan3"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Clear box", "clear"),
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

// ─── DrDrMail — a local mailbox (compose, read, folders) ─────────────

#[derive(Clone)]
struct MailMsg {
    from: String,
    to: String,
    subject: String,
    body: String,
    ts: u64,
}

const MAIL_FOLDERS: [&str; 3] = ["Inbox", "Sent", "Drafts"];
const MAIL_SIDEBAR_W: u32 = 11;

#[derive(PartialEq, Clone, Copy)]
enum MailField {
    To,
    Subject,
    Body,
}

/// A self-contained mail client. There is no SMTP — mail lives on this
/// machine — but it behaves like the real thing: three folders, a message
/// list, a reading pane and a compose form. Sent mail is also written to
/// your Documents as a `.eml` text file, so it shows up in Files too.
pub struct MailApp {
    folders: [Vec<MailMsg>; 3],
    folder: usize,
    sel: usize,
    composing: Option<(MailMsg, MailField)>,
    status: String,
    menu: MenuBar,
}

impl MailApp {
    pub fn new() -> Self {
        let inbox = vec![
            MailMsg {
                from: "DrDrOS Team".into(),
                to: "you".into(),
                subject: "Welcome to DrDrMail".into(),
                body: "Hi there!\n\nThis is your local mailbox. Use Mail > Compose to write a \
                       message; sent mail is saved into your Documents as an .eml file so \
                       you can find it in Files too.\n\nThere is no SMTP server - everything \
                       here stays on this machine.\n\n- The DrDrOS Team"
                    .into(),
                ts: 1_700_000_000,
            },
            MailMsg {
                from: "DrDrOS Tips".into(),
                to: "you".into(),
                subject: "Getting around".into(),
                body: "Click a folder on the left to switch between Inbox, Sent and Drafts.\n\
                       Click a message to read it. Press Tab to cycle folders, or use the \
                       Mail menu at the top.\n\nEnjoy your new desktop!"
                    .into(),
                ts: 1_700_100_000,
            },
        ];
        Self {
            folders: [inbox, Vec::new(), Vec::new()],
            folder: 0,
            sel: 0,
            composing: None,
            status: "ready".into(),
            menu: mail_menu(),
        }
    }

    fn do_action(&mut self, a: &str) -> AppControl {
        match a {
            "compose" => {
                self.composing = Some((
                    MailMsg { from: "me".into(), to: String::new(), subject: String::new(), body: String::new(), ts: 0 },
                    MailField::To,
                ));
                self.status = "composing - Tab switches fields, menu Mail>Send".into();
            }
            "send" => self.finish_compose(true),
            "draft" => self.finish_compose(false),
            "discard" => {
                self.composing = None;
                self.status = "discarded".into();
            }
            "delete" => {
                let f = self.folder;
                if self.sel < self.folders[f].len() {
                    self.folders[f].remove(self.sel);
                    self.sel = self.sel.min(self.folders[f].len().saturating_sub(1));
                    self.status = "deleted".into();
                }
            }
            "inbox" => { self.folder = 0; self.sel = 0; }
            "sent" => { self.folder = 1; self.sel = 0; }
            "drafts" => { self.folder = 2; self.sel = 0; }
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }

    /// Move the compose buffer to Sent (and write a .eml to Documents) or
    /// to Drafts, then leave compose mode.
    fn finish_compose(&mut self, send: bool) {
        let Some((mut m, _)) = self.composing.take() else {
            self.status = "nothing to send".into();
            return;
        };
        m.ts = crate::net::now_unix_secs();
        if m.to.trim().is_empty() {
            m.to = "(no recipient)".into();
        }
        if m.subject.trim().is_empty() {
            m.subject = "(no subject)".into();
        }
        if send {
            let fname = format!("mail-{}.eml", m.ts);
            let eml = format!(
                "From: {}\nTo: {}\nSubject: {}\nDate: {}\n\n{}\n",
                m.from, m.to, m.subject, m.ts, m.body
            );
            let saved = drdr_store::save(&fname, eml.as_bytes()).is_ok();
            self.folders[1].push(m);
            self.folder = 1;
            self.sel = self.folders[1].len() - 1;
            self.status = if saved {
                format!("sent (saved {fname} to Documents)")
            } else {
                "sent (could not write a copy to Documents)".into()
            };
        } else {
            self.folders[2].push(m);
            self.folder = 2;
            self.sel = self.folders[2].len() - 1;
            self.status = "saved to Drafts".into();
        }
    }

    fn compose_key(&mut self, key: KeyCode) {
        let Some((m, field)) = self.composing.as_mut() else { return };
        let buf = match field {
            MailField::To => &mut m.to,
            MailField::Subject => &mut m.subject,
            MailField::Body => &mut m.body,
        };
        match key {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Space => buf.push(' '),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Tab => {
                *field = match field {
                    MailField::To => MailField::Subject,
                    MailField::Subject => MailField::Body,
                    MailField::Body => MailField::To,
                };
            }
            KeyCode::Enter => {
                if *field == MailField::Body {
                    m.body.push('\n');
                } else {
                    *field = if *field == MailField::To { MailField::Subject } else { MailField::Body };
                }
            }
            KeyCode::Escape => {
                self.composing = None;
                self.status = "compose cancelled".into();
            }
            _ => {}
        }
    }
}

impl WindowApp for MailApp {
    fn icon(&self) -> IconKind {
        IconKind::Mail
    }
    fn title(&self) -> String {
        if self.composing.is_some() {
            "DrDrMail - composing".into()
        } else {
            format!("DrDrMail - {} ({})", MAIL_FOLDERS[self.folder], self.folders[self.folder].len())
        }
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        if self.composing.is_some() {
            self.compose_key(key);
            return AppControl::Continue;
        }
        let n = self.folders[self.folder].len();
        match key {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Tab => {
                self.folder = (self.folder + 1) % MAIL_FOLDERS.len();
                self.sel = 0;
            }
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        if self.composing.is_some() {
            return AppControl::Continue;
        }
        if col < MAIL_SIDEBAR_W {
            if row >= 2 && (row as usize - 2) < MAIL_FOLDERS.len() {
                self.folder = row as usize - 2;
                self.sel = 0;
            }
        } else if row >= 2 {
            let idx = row as usize - 2;
            if idx < self.folders[self.folder].len() {
                self.sel = idx;
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let cols = g.cols;
        let rows = g.rows;
        let fg = g.fg();
        let bg = g.bg();
        let accent = Px::rgb(0x2D, 0x6C, 0xD8);
        let muted = Px::rgb(0x80, 0x80, 0x88);

        if let Some((m, field)) = &self.composing {
            g.write(0, 1, "New message", accent, bg);
            let mark = |f: MailField| if *field == f { '>' } else { ' ' };
            g.write(0, 3, &format!("{} To:      {}", mark(MailField::To), m.to), fg, bg);
            g.write(0, 4, &format!("{} Subject: {}", mark(MailField::Subject), m.subject), fg, bg);
            g.write(0, 6, &format!("{} Body:", mark(MailField::Body)), fg, bg);
            for (i, line) in m.body.split('\n').enumerate() {
                let row = 7 + i as u32;
                if row >= rows {
                    break;
                }
                g.write(2, row, line, fg, bg);
            }
            if *field == MailField::Body {
                // a caret at the end of the body
                let last = m.body.split('\n').count() as u32 - 1;
                let lastlen = m.body.split('\n').next_back().unwrap_or("").chars().count() as u32;
                if 7 + last < rows {
                    g.put(2 + lastlen, 7 + last, '_', bg, fg);
                }
            }
            g.write(0, rows.saturating_sub(1), &format!("Tab=field  Mail>Send / Save draft  Esc=cancel   {}", self.status), muted, bg);
            self.menu.render(g, fg, bg);
            return;
        }

        // ── Folder sidebar ───────────────────────────────────────────
        for r in 0..rows {
            fill_cells(g, 0, r, MAIL_SIDEBAR_W, fg, bg);
        }
        g.write(1, 1, "Folders", muted, bg);
        for (i, name) in MAIL_FOLDERS.iter().enumerate() {
            let row = 2 + i as u32;
            let on = i == self.folder;
            let label = format!("{} {}", name, self.folders[i].len());
            if on {
                fill_cells(g, 0, row, MAIL_SIDEBAR_W, bg, fg);
                g.write(1, row, &label, bg, fg);
            } else {
                g.write(1, row, &label, fg, bg);
            }
        }
        for r in 0..rows {
            g.put(MAIL_SIDEBAR_W, r, '|', muted, bg);
        }

        // ── Message list (top) + reading pane (bottom) ───────────────
        let lx0 = MAIL_SIDEBAR_W + 2;
        let list = &self.folders[self.folder];
        if self.sel >= list.len() {
            self.sel = list.len().saturating_sub(1);
        }
        let split = (rows / 2).max(3);
        g.write(lx0, 1, &format!("{} - {} message(s)", MAIL_FOLDERS[self.folder], list.len()), muted, bg);
        if list.is_empty() {
            g.write(lx0, 3, "(no mail here)", muted, bg);
        }
        for (i, m) in list.iter().enumerate() {
            let row = 2 + i as u32;
            if row >= split {
                break;
            }
            let line = format!("{:<14} {}", trunc(&m.from, 14), m.subject);
            region_line(g, row, lx0, &line, i == self.sel);
        }
        // divider between list and reader
        for c in lx0..cols {
            g.put(c, split, '-', muted, bg);
        }
        // reading pane
        if let Some(m) = list.get(self.sel) {
            g.write(lx0, split + 1, &format!("From: {}", m.from), accent, bg);
            g.write(lx0, split + 2, &format!("Subj: {}", m.subject), fg, bg);
            let wrapw = (cols - lx0).max(8) as usize;
            let mut row = split + 4;
            for para in m.body.split('\n') {
                for line in wrap_text(para, wrapw) {
                    if row >= rows {
                        break;
                    }
                    g.write(lx0, row, &line, fg, bg);
                    row += 1;
                }
            }
        }
        self.menu.render(g, fg, bg);
    }
}

fn mail_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Mail".into(),
                items: vec![
                    MenuBar::item("Compose", "compose"),
                    MenuBar::item("Send", "send"),
                    MenuBar::item("Save draft", "draft"),
                    MenuBar::item("Discard", "discard"),
                    MenuBar::item("Delete", "delete"),
                ],
            },
            Menu {
                title: "Folder".into(),
                items: vec![
                    MenuBar::item("Inbox", "inbox"),
                    MenuBar::item("Sent", "sent"),
                    MenuBar::item("Drafts", "drafts"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

// ─── DrDrCalendar — a month view with saved events ───────────────────

/// A persistent month calendar. The grid shows a month with today ringed;
/// click a day (or arrow to it) to see and add events. Each event is one
/// line saved through drdr_store as `calendar.txt` ("YYYY-MM-DD|text"), so
/// it survives a reboot whenever a disk is mounted.
pub struct CalendarApp {
    year: i64,
    month: i64,
    sel_day: i64,
    events: Vec<(String, String)>,
    input: String,
    status: String,
    menu: MenuBar,
}

impl CalendarApp {
    pub fn new() -> Self {
        let (y, m, d) = civil(now_unix().div_euclid(86_400));
        let events = Self::load_events();
        Self { year: y, month: m, sel_day: d, events, input: String::new(), status: "today".into(), menu: calendar_menu() }
    }

    fn load_events() -> Vec<(String, String)> {
        let mut v = Vec::new();
        if let Ok(bytes) = drdr_store::load("calendar.txt") {
            for line in String::from_utf8_lossy(&bytes).lines() {
                if let Some((date, text)) = line.split_once('|') {
                    v.push((date.to_string(), text.to_string()));
                }
            }
        }
        v
    }

    fn save_events(&mut self) {
        let body: String = self.events.iter().map(|(d, t)| format!("{d}|{t}\n")).collect();
        let _ = drdr_store::save("calendar.txt", body.as_bytes());
    }

    fn sel_date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.sel_day)
    }

    fn days_in_month(y: i64, m: i64) -> i64 {
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1)
    }

    fn shift_month(&mut self, delta: i64) {
        let mut m = self.month + delta;
        while m > 12 {
            m -= 12;
            self.year += 1;
        }
        while m < 1 {
            m += 12;
            self.year -= 1;
        }
        self.month = m;
        self.sel_day = self.sel_day.min(Self::days_in_month(self.year, self.month));
    }

    fn shift_day(&mut self, delta: i64) {
        let dim = Self::days_in_month(self.year, self.month);
        let nd = self.sel_day + delta;
        if nd < 1 {
            self.shift_month(-1);
            self.sel_day = Self::days_in_month(self.year, self.month);
        } else if nd > dim {
            self.shift_month(1);
            self.sel_day = 1;
        } else {
            self.sel_day = nd;
        }
    }

    fn add_event(&mut self) {
        let text = std::mem::take(&mut self.input);
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.events.push((self.sel_date(), text));
        self.save_events();
        self.status = "event added".into();
    }

    fn do_action(&mut self, a: &str) -> AppControl {
        match a {
            "prev" => self.shift_month(-1),
            "next" => self.shift_month(1),
            "today" => {
                let (y, m, d) = civil(now_unix().div_euclid(86_400));
                self.year = y;
                self.month = m;
                self.sel_day = d;
            }
            "del" => {
                let date = self.sel_date();
                if let Some(pos) = self.events.iter().rposition(|(d, _)| *d == date) {
                    self.events.remove(pos);
                    self.save_events();
                    self.status = "removed last event of the day".into();
                }
            }
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }
}

const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];

impl WindowApp for CalendarApp {
    fn icon(&self) -> IconKind {
        IconKind::Calendar
    }
    fn title(&self) -> String {
        format!("Calendar - {} {}", MONTH_NAMES[(self.month - 1).clamp(0, 11) as usize], self.year)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        match key {
            KeyCode::Left => self.shift_day(-1),
            KeyCode::Right => self.shift_day(1),
            KeyCode::Up => self.shift_day(-7),
            KeyCode::Down => self.shift_day(7),
            KeyCode::PageUp => self.shift_month(-1),
            KeyCode::PageDown => self.shift_month(1),
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Space => self.input.push(' '),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.add_event(),
            KeyCode::Escape => self.input.clear(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        // Grid starts at row 4 (after menu, title, weekday header); each
        // week is one row, each day 4 cells wide from column 1.
        if row >= 4 && col >= 1 && col < 1 + 7 * 4 {
            let wd = ((col - 1) / 4) as i64;
            let week = (row - 4) as i64;
            let first_wd = ((days_from_civil(self.year, self.month, 1) % 7) + 4).rem_euclid(7);
            let day = week * 7 + wd - first_wd + 1;
            if day >= 1 && day <= Self::days_in_month(self.year, self.month) {
                self.sel_day = day;
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let rows = g.rows;
        let fg = g.fg();
        let bg = g.bg();
        let accent = Px::rgb(0x2D, 0x6C, 0xD8);
        let muted = Px::rgb(0x80, 0x80, 0x88);
        let red = Px::rgb(0xD0, 0x3A, 0x3A);

        let title = format!("{}  {}   < PgUp   PgDn >", MONTH_NAMES[(self.month - 1).clamp(0, 11) as usize], self.year);
        g.write(1, 1, &title, accent, bg);
        g.write(1, 2, "Su  Mo  Tu  We  Th  Fr  Sa", muted, bg);

        let (ty, tm, td) = civil(now_unix().div_euclid(86_400));
        let first_wd = ((days_from_civil(self.year, self.month, 1) % 7) + 4).rem_euclid(7);
        let dim = Self::days_in_month(self.year, self.month);
        for day in 1..=dim {
            let cell = first_wd + (day - 1);
            let week = (cell / 7) as u32;
            let wd = (cell % 7) as u32;
            let row = 4 + week;
            let col = 1 + wd * 4;
            if row >= rows {
                break;
            }
            let is_today = self.year == ty && self.month == tm && day == td;
            let is_sel = day == self.sel_day;
            let has_ev = self.events.iter().any(|(d, _)| *d == format!("{:04}-{:02}-{:02}", self.year, self.month, day));
            let label = format!("{day:>2}");
            let (cfg, cbg) = if is_sel {
                (bg, accent)
            } else if is_today {
                (red, bg)
            } else {
                (fg, bg)
            };
            if is_sel {
                fill_cells(g, col, row, 3, cfg, cbg);
            }
            g.write(col, row, &label, cfg, cbg);
            if has_ev {
                g.put(col + 2, row, '.', if is_sel { bg } else { accent }, cbg);
            }
        }

        // ── Events for the selected day + composer ───────────────────
        let panel_row = 4 + 7;
        if panel_row < rows {
            let date = self.sel_date();
            g.write(1, panel_row, &format!("Events on {date}:"), accent, bg);
            let todays: Vec<&String> = self.events.iter().filter(|(d, _)| *d == date).map(|(_, t)| t).collect();
            if todays.is_empty() {
                g.write(2, panel_row + 1, "(none - type below and press Enter)", muted, bg);
            }
            for (i, t) in todays.iter().enumerate() {
                let row = panel_row + 1 + i as u32;
                if row >= rows - 1 {
                    break;
                }
                g.write(2, row, &format!("- {t}"), fg, bg);
            }
        }
        let crow = rows - 1;
        g.write(1, crow, &format!("Add: {}_   ({})", self.input, self.status), accent, bg);

        self.menu.render(g, fg, bg);
    }
}

fn calendar_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Month".into(),
                items: vec![
                    MenuBar::item("Previous", "prev"),
                    MenuBar::item("Next", "next"),
                    MenuBar::item("Jump to today", "today"),
                ],
            },
            Menu {
                title: "Event".into(),
                items: vec![MenuBar::item("Delete day's last", "del")],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

// ─── DrDrContacts — a saved address book ─────────────────────────────

/// A persistent address book. Contacts are saved through drdr_store as
/// `contacts.txt` ("name|detail" per line). Type "Name = detail" in the
/// composer and press Enter to add one; the list survives a reboot when a
/// disk is mounted.
pub struct ContactsApp {
    contacts: Vec<(String, String)>,
    sel: usize,
    input: String,
    adding: bool,
    status: String,
    menu: MenuBar,
}

impl ContactsApp {
    pub fn new() -> Self {
        let mut contacts = Self::load();
        if contacts.is_empty() {
            contacts.push(("DrDrOS Team".into(), "user@example.invalid".into()));
            contacts.push(("Support".into(), "press F1 for shortcuts".into()));
        }
        Self { contacts, sel: 0, input: String::new(), adding: false, status: "ready".into(), menu: contacts_menu() }
    }

    fn load() -> Vec<(String, String)> {
        let mut v = Vec::new();
        if let Ok(bytes) = drdr_store::load("contacts.txt") {
            for line in String::from_utf8_lossy(&bytes).lines() {
                let (name, detail) = line.split_once('|').unwrap_or((line, ""));
                if !name.trim().is_empty() {
                    v.push((name.to_string(), detail.to_string()));
                }
            }
        }
        v
    }

    fn save(&mut self) {
        let body: String = self.contacts.iter().map(|(n, d)| format!("{n}|{d}\n")).collect();
        let _ = drdr_store::save("contacts.txt", body.as_bytes());
    }

    fn commit_add(&mut self) {
        let raw = std::mem::take(&mut self.input);
        let raw = raw.trim();
        if raw.is_empty() {
            self.adding = false;
            return;
        }
        let (name, detail) = raw.split_once('=').map(|(n, d)| (n.trim(), d.trim())).unwrap_or((raw, ""));
        self.contacts.push((name.to_string(), detail.to_string()));
        self.sel = self.contacts.len() - 1;
        self.save();
        self.adding = false;
        self.status = "contact added".into();
    }

    fn do_action(&mut self, a: &str) -> AppControl {
        match a {
            "new" => {
                self.adding = true;
                self.input.clear();
                self.status = "type  Name = detail  then Enter".into();
            }
            "delete" => {
                if self.sel < self.contacts.len() {
                    self.contacts.remove(self.sel);
                    self.sel = self.sel.min(self.contacts.len().saturating_sub(1));
                    self.save();
                    self.status = "deleted".into();
                }
            }
            "message" => {
                self.status = "open DrDrMessages to chat on the LAN".into();
            }
            "theme" => toggle_theme(),
            "close" => return AppControl::Close,
            _ => {}
        }
        AppControl::Continue
    }
}

impl WindowApp for ContactsApp {
    fn icon(&self) -> IconKind {
        IconKind::Contacts
    }
    fn title(&self) -> String {
        format!("Contacts ({})", self.contacts.len())
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        if self.menu.is_open() {
            self.menu.close();
            return AppControl::Continue;
        }
        if self.adding {
            match key {
                KeyCode::Char(c) => self.input.push(c),
                KeyCode::Space => self.input.push(' '),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Enter => self.commit_add(),
                KeyCode::Escape => {
                    self.adding = false;
                    self.input.clear();
                }
                _ => {}
            }
            return AppControl::Continue;
        }
        match key {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(self.contacts.len().saturating_sub(1)),
            KeyCode::Char('n') => return self.do_action("new"),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        match self.menu.on_click(col, row) {
            MenuClick::Action(a) => return self.do_action(a),
            MenuClick::Consumed => return AppControl::Continue,
            MenuClick::Passthrough => {}
        }
        let listw = 22u32;
        if col < listw && row >= 2 {
            let idx = row as usize - 2;
            if idx < self.contacts.len() {
                self.sel = idx;
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let cols = g.cols;
        let rows = g.rows;
        let fg = g.fg();
        let bg = g.bg();
        let accent = Px::rgb(0x2B, 0x9B, 0x8A);
        let muted = Px::rgb(0x80, 0x80, 0x88);
        let listw = 22u32.min(cols / 2);

        if self.sel >= self.contacts.len() {
            self.sel = self.contacts.len().saturating_sub(1);
        }

        // List
        g.write(1, 1, "People", muted, bg);
        for (i, (name, _)) in self.contacts.iter().enumerate() {
            let row = 2 + i as u32;
            if row >= rows - 1 {
                break;
            }
            let on = i == self.sel;
            let shown: String = name.chars().take((listw - 4) as usize).collect();
            if on {
                fill_cells(g, 0, row, listw, bg, fg);
                g.put(1, row, '*', bg, fg);
                g.write(3, row, &shown, bg, fg);
            } else {
                g.put(1, row, 'o', accent, bg);
                g.write(3, row, &shown, fg, bg);
            }
        }
        for r in 0..rows {
            g.put(listw, r, '|', muted, bg);
        }

        // Detail card
        let dx = listw + 2;
        if let Some((name, detail)) = self.contacts.get(self.sel) {
            g.write(dx, 1, name, accent, bg);
            g.write(dx, 3, "Detail:", muted, bg);
            for (i, line) in wrap_text(detail, (cols - dx).max(6) as usize).iter().enumerate() {
                let row = 4 + i as u32;
                if row >= rows - 2 {
                    break;
                }
                g.write(dx, row, line, fg, bg);
            }
        }

        let crow = rows - 1;
        if self.adding {
            g.write(0, crow, &format!("New (Name = detail): {}_", self.input), accent, bg);
        } else {
            g.write(0, crow, &format!("n=new  Contacts menu to delete   {}", self.status), muted, bg);
        }
        self.menu.render(g, fg, bg);
    }
}

fn contacts_menu() -> MenuBar {
    MenuBar {
        open: None,
        menus: vec![
            Menu {
                title: "Contacts".into(),
                items: vec![
                    MenuBar::item("New", "new"),
                    MenuBar::item("Delete", "delete"),
                    MenuBar::item("Message on LAN", "message"),
                ],
            },
            Menu {
                title: "View".into(),
                items: vec![
                    MenuBar::item("Toggle theme", "theme"),
                    MenuBar::item("Close", "close"),
                ],
            },
        ],
    }
}

/// Truncate `s` to `n` chars (no ellipsis) — small helper for table cells.
fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn calculator_evaluates_precedence_and_parens() {
        assert_eq!(eval_expr("1+2*3").unwrap(), 7.0);
        assert_eq!(eval_expr("(1+2)*3").unwrap(), 9.0);
        assert_eq!(eval_expr("-4 + 2").unwrap(), -2.0);
        assert_eq!(eval_expr("10 / 4").unwrap(), 2.5);
        assert_eq!(eval_expr("2 + 2 * 2 - 1").unwrap(), 5.0);
        assert!(eval_expr("1/0").is_err());
        assert!(eval_expr("2++").is_err());
        assert!(eval_expr("(1+2").is_err());
    }

    #[test]
    fn trim_float_is_clean() {
        assert_eq!(trim_float(4.0), "4");
        assert_eq!(trim_float(2.5), "2.5");
        assert_eq!(trim_float(-3.0), "-3");
    }

    #[test]
    fn civil_round_trips_with_days_from_civil() {
        // 2026-05-16 (the project's "today") and a leap day.
        for (y, m, d) in [(2026, 5, 16), (2024, 2, 29), (1970, 1, 1)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil(days), (y, m, d));
        }
    }

    #[test]
    fn progress_bar_fills_proportionally() {
        assert_eq!(bar(0, 10), "[----------]   0%");
        assert_eq!(bar(50, 10), "[#####-----]  50%");
        assert_eq!(bar(100, 10), "[##########] 100%");
        assert_eq!(bar(150, 10), "[##########] 100%"); // clamped
    }

    // ─── phase 8 ─────────────────────────────────────────────────

    #[test]
    fn snake_wall_collision_ends_the_game() {
        let mut s = SnakeApp::new();
        s.reset_for(8, 6);
        // Force the head right against the right wall, then step once.
        // Body is 4 cells long centred → head at (cw/2 + 0, cy). Direction
        // is (1, 0). Stepping (cw/2 + 1) times will walk off the edge.
        for _ in 0..(s.cw - s.body.front().unwrap().0 + 1) {
            s.step();
        }
        assert!(s.over, "snake should die when it walks off the right wall");
    }

    #[test]
    fn snake_eating_food_grows_the_body_and_scores() {
        let mut s = SnakeApp::new();
        s.reset_for(40, 20);
        // Plant food directly in front of the head and step once.
        let head = *s.body.front().unwrap();
        s.food = (head.0 + 1, head.1);
        let len_before = s.body.len();
        s.step();
        assert!(!s.over);
        assert_eq!(s.score, 1);
        assert_eq!(s.body.len(), len_before + 1);
    }

    #[test]
    fn snake_cannot_reverse_into_its_own_neck() {
        let mut s = SnakeApp::new();
        s.reset_for(40, 20);
        // Moving right; ask for "left" — engine must ignore the reversal
        // so the snake doesn't fold onto itself.
        s.next_dir = (-1, 0);
        let head = *s.body.front().unwrap();
        s.step();
        let new_head = *s.body.front().unwrap();
        assert_eq!(new_head, (head.0 + 1, head.1));
        assert!(!s.over);
    }

    #[test]
    fn paint_palette_click_selects_swatch() {
        let mut p = PaintApp::new();
        // Swatches now stack vertically in the sidebar: row 3 = swatch 2.
        p.touch(1, 3);
        assert_eq!(p.sel, 2);
        assert!(!p.erasing);
    }

    #[test]
    fn paint_drag_writes_canvas_cells() {
        let mut p = PaintApp::new();
        p.ensure_size(20, 10);
        p.sel = 0;
        p.brush = 1;
        // Canvas starts at column PAINT_CANVAS_X; this is canvas (3, 5).
        p.touch(PAINT_CANVAS_X + 3, 5);
        assert!(p.cells[5][3].is_some());
        p.erasing = true;
        p.touch(PAINT_CANVAS_X + 3, 5);
        assert!(p.cells[5][3].is_none());
    }

    #[test]
    fn chat_timestamp_formats_as_hhmm() {
        assert_eq!(fmt_hhmm(0), "00:00");
        assert_eq!(fmt_hhmm(60), "00:01");
        // 10:42 UTC on any day past epoch — minutes of day = 10*60+42.
        let ts = (10 * 60 + 42) * 60;
        assert_eq!(fmt_hhmm(ts), "10:42");
    }

    #[test]
    fn g2048_slide_merges_once_toward_zero() {
        // [2,2,2,2] → [4,4,0,0], gained 8, moved.
        let (new, gained, moved) = Game2048::slide([2, 2, 2, 2]);
        assert_eq!(new, [4, 4, 0, 0]);
        assert_eq!(gained, 8);
        assert!(moved);
        // Already-collapsed line doesn't move.
        let (_, _, moved) = Game2048::slide([4, 2, 0, 0]);
        assert!(!moved);
        // A single gap closes up (moved) without merging.
        let (new, gained, moved) = Game2048::slide([0, 2, 0, 4]);
        assert_eq!(new, [2, 4, 0, 0]);
        assert_eq!(gained, 0);
        assert!(moved);
    }

    #[test]
    fn g2048_right_move_collapses_to_the_right() {
        let mut g = Game2048::new();
        g.board = [[2, 2, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]];
        let moved = g.move_dir(Dir2048::Right);
        assert!(moved);
        // The 2+2 merged into a 4 pinned to the right edge…
        assert_eq!(g.board[0][3], 4);
        // …and a new random tile (2 or 4) spawned somewhere.
        let nonzero: u32 = g.board.iter().flatten().filter(|&&v| v != 0).count() as u32;
        assert_eq!(nonzero, 2);
    }

    #[test]
    fn mines_first_click_is_always_safe_and_counts_adjacency() {
        let mut m = MinesApp::new();
        m.reveal(5, 5);
        // The first-clicked cell can never be a bomb.
        assert!(!m.bomb[m.idx(5, 5)]);
        assert!(m.revealed[m.idx(5, 5)]);
        assert!(!m.over);
        // Adjacency of every cell equals its real neighbour-bomb count.
        for y in 0..m.h {
            for x in 0..m.w {
                let want = m
                    .neighbors(x, y)
                    .iter()
                    .filter(|&&(nx, ny)| m.bomb[m.idx(nx, ny)])
                    .count() as u8;
                assert_eq!(m.adj[m.idx(x, y)], want);
            }
        }
    }

    #[test]
    fn tasks_round_trip_done_marker_parsing() {
        // The on-disk format is "[x] text" / "[ ] text"; loading must
        // recover the done flag and the text.
        let mut t = TasksApp { tasks: vec![], input: String::new(), status: String::new(), modified: false, autosave: 0 };
        // Simulate what load() parses, line by line.
        for line in ["[x] buy milk", "[ ] write code"] {
            let done = line.starts_with("[x]");
            let text = line
                .trim_start_matches(|c| matches!(c, '[' | ']' | 'x' | 'X' | ' '))
                .to_string();
            t.tasks.push((done, text));
        }
        assert_eq!(t.tasks[0], (true, "buy milk".to_string()));
        assert_eq!(t.tasks[1], (false, "write code".to_string()));
    }

    #[test]
    fn file_extension_and_classification() {
        assert_eq!(ext_of("notes.TXT"), "txt");
        assert_eq!(ext_of("Makefile"), "");
        assert_eq!(ext_of(".bashrc"), "bashrc");
        assert_eq!(classify("a.png"), FileClass::Image);
        assert_eq!(classify("index.html"), FileClass::Web);
        assert_eq!(classify("main.rs"), FileClass::Code);
        assert_eq!(classify("report.pdf"), FileClass::Pdf);
        assert_eq!(classify("readme"), FileClass::Text);
        // Tags follow the class.
        assert_eq!(type_tag("a.rs", false), "<>");
        assert_eq!(type_tag("a", true), "DIR");
    }

    #[test]
    fn syntax_highlight_colours_keywords_strings_and_comments() {
        let fg = Px::rgb(0x10, 0x10, 0x10);
        let cols = highlight("let x = \"hi\"; // note", Lang::Rust, fg);
        // "let" is a keyword → not the default colour.
        assert_ne!(cols[0], fg);
        // the string body differs from the default too.
        let q = "let x = \"hi\"; // note".find('"').unwrap();
        assert_ne!(cols[q], fg);
        // a plain language leaves everything default.
        let plain = highlight("let x = 1", Lang::Plain, fg);
        assert!(plain.iter().all(|&c| c == fg));
    }

    #[test]
    fn editor_menu_actions_change_state() {
        let mut e = EditApp::new(std::path::PathBuf::from("/tmp/drdr_test_unused.txt"));
        assert_eq!(e.text_zoom, 1);
        e.do_action("size_up");
        assert_eq!(e.text_zoom, 2);
        e.do_action("size_down");
        assert_eq!(e.text_zoom, 1);
        assert!(e.ink.is_none());
        e.do_action("ink_red");
        assert!(e.ink.is_some());
        e.do_action("ink_default");
        assert!(e.ink.is_none());
    }

    #[test]
    fn editor_keeps_colour_buffer_in_lock_step_with_text() {
        let mut e = EditApp::new(std::path::PathBuf::from("/tmp/drdr_test_unused2.txt"));
        e.ink = Some(Px::rgb(1, 2, 3));
        for c in "abc".chars() {
            e.insert(c);
        }
        e.newline();
        for c in "de".chars() {
            e.insert(c);
        }
        // Every line's colour row matches its char count.
        for (line, colrow) in e.lines.iter().zip(e.colors.iter()) {
            assert_eq!(line.chars().count(), colrow.len());
        }
        e.backspace();
        for (line, colrow) in e.lines.iter().zip(e.colors.iter()) {
            assert_eq!(line.chars().count(), colrow.len());
        }
    }

    #[test]
    fn menu_bar_click_fires_actions_and_closes() {
        let mut m = editor_menu();
        // Click the first title (File) → opens.
        assert!(matches!(m.on_click(m.title_start(0), 0), MenuClick::Consumed));
        assert!(m.is_open());
        // Click its first item (row 1) → fires "new" and closes.
        let s = m.title_start(0);
        assert!(matches!(m.on_click(s + 1, 1), MenuClick::Action("new")));
        assert!(!m.is_open());
    }

    #[test]
    fn decodes_a_tiny_ppm() {
        // 2x1 P6: red then green.
        let mut bytes = b"P6 2 1 255 ".to_vec();
        bytes.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
        let (img, _) = decode_image("t.ppm", &bytes);
        let img = img.expect("ppm should decode");
        assert_eq!((img.w, img.h), (2, 1));
        assert_eq!(img.px[0], Px::rgb(255, 0, 0));
        assert_eq!(img.px[1], Px::rgb(0, 255, 0));
    }

    #[test]
    fn html_render_strips_tags_and_surfaces_text() {
        let out = render_html("<h1>Title</h1><p>Hello <b>world</b></p>");
        let joined: String = out.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>().join("|");
        assert!(joined.contains("Title"));
        assert!(joined.contains("Hello"));
        assert!(!joined.contains('<'));
    }

    // ─── phase 12: shell icons + file-manager sidebar ───────────────

    #[test]
    fn apps_report_their_own_icon() {
        // A spread of apps each map to the right pictographic kind, so the
        // taskbar / title bar draw a real icon, not the generic fallback.
        assert_eq!(AboutApp.icon(), IconKind::Info);
        assert_eq!(CalcApp::new().icon(), IconKind::Calculator);
        assert_eq!(ClockApp::new().icon(), IconKind::Clock);
        assert_eq!(BrowserApp::new().icon(), IconKind::Browser);
        assert_eq!(SystemApp::new().icon(), IconKind::Power);
        assert_eq!(
            FilesApp::new(drdr_store::documents_dir()).icon(),
            IconKind::Folder
        );
    }

    #[test]
    fn files_sidebar_has_the_expected_places() {
        let f = FilesApp::new(drdr_store::documents_dir());
        let labels: Vec<&str> = f.places().iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, ["Documents", "My Data", "Filesystem", "Scratch"]);
    }

    #[test]
    fn clicking_a_place_navigates_there() {
        // Row 0 is now the menu bar and row 1 the "PLACES" header, so the
        // place list starts at row 2: place index i sits at row i + 2.
        // "Scratch" → /tmp is place index 3 → sidebar row 5.
        let mut f = FilesApp::new(drdr_store::documents_dir());
        let scratch = std::path::PathBuf::from("/tmp");
        f.on_click(1, 5, false);
        if scratch.is_dir() {
            assert_eq!(f.cwd, scratch, "sidebar click should navigate to /tmp");
        }
    }

    #[test]
    fn region_line_preserves_the_sidebar_columns() {
        // A selected list row must only repaint from x0 rightward, leaving
        // the sidebar cells (col < x0) untouched.
        let mut g = TextGrid::new(20, 3, Px::WHITE, Px::BLACK);
        g.put(2, 1, 'S', Px::WHITE, Px::BLACK); // a sidebar glyph
        region_line(&mut g, 1, FILES_LIST_X, "file.txt", true);
        assert_eq!(g.cell(2, 1).ch, 'S', "sidebar glyph survived the fill");
        assert_eq!(g.cell(FILES_LIST_X, 1).ch, 'f');
    }

    // ─── phase 13: real PNG/GIF/JPEG + PDF/archive/media routing ────

    #[test]
    fn classify_routes_new_formats() {
        assert_eq!(classify("a.png"), FileClass::Image);
        assert_eq!(classify("a.gif"), FileClass::Image);
        assert_eq!(classify("a.jpg"), FileClass::Image);
        assert_eq!(classify("report.pdf"), FileClass::Pdf);
        assert_eq!(classify("notes.docx"), FileClass::Archive);
        assert_eq!(classify("photos.zip"), FileClass::Archive);
        assert_eq!(classify("movie.mp4"), FileClass::Media);
        assert_eq!(classify("clip.mkv"), FileClass::Media);
        assert_eq!(classify("song.mp3"), FileClass::Media);
    }

    // A real 3×2 RGBA PNG written by Pillow.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 3, 0, 0, 0, 2, 8,
        6, 0, 0, 0, 157, 116, 102, 26, 0, 0, 0, 27, 73, 68, 65, 84, 120, 156, 37, 199, 177, 13, 0,
        0, 12, 195, 32, 212, 255, 127, 118, 134, 178, 33, 146, 168, 19, 127, 6, 134, 173, 8, 250,
        147, 205, 134, 116, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn decode_image_decodes_a_real_png() {
        let (img, _) = decode_image("t.png", PNG);
        let img = img.expect("png should decode through drdr-codec");
        assert_eq!((img.w, img.h), (3, 2));
        assert_eq!(img.px[0], Px::rgb(255, 0, 0));
        assert_eq!(img.px[1], Px::rgb(0, 255, 0));
        assert_eq!(img.px[2], Px::rgb(0, 0, 255));
        assert_eq!(img.px[3], Px::rgb(255, 255, 0));
    }

    #[test]
    fn xml_to_text_surfaces_words_and_breaks() {
        let docx = "<w:p><w:r><w:t>Hello</w:t></w:r></w:p>\
                    <w:p><w:r><w:t>world &amp; co</w:t></w:r></w:p>";
        let text = xml_to_text(docx);
        assert!(text.contains("Hello"));
        assert!(text.contains("world & co"));
        assert!(!text.contains('<'));
        // The two paragraphs should be on separate lines.
        assert_eq!(text.lines().filter(|l| !l.trim().is_empty()).count(), 2);
    }

    #[test]
    fn alpha_composites_over_white() {
        // A fully transparent pixel becomes white; opaque keeps its colour.
        let img = drdr_codec::Image {
            w: 2,
            h: 1,
            rgba: vec![10, 20, 30, 0, 10, 20, 30, 255],
        };
        let d = img_from_codec(img);
        assert_eq!(d.px[0], Px::rgb(255, 255, 255));
        assert_eq!(d.px[1], Px::rgb(10, 20, 30));
    }

    // ─── phase 14: web browser + notes/paint menus & palette ────────

    #[test]
    fn domain_detection_distinguishes_urls_from_paths() {
        assert!(looks_like_domain("example.com"));
        assert!(looks_like_domain("sub.example.co.uk"));
        assert!(!looks_like_domain("/etc/hosts"));
        assert!(!looks_like_domain("just text"));
        assert!(!looks_like_domain("README")); // no dot
    }

    #[test]
    fn extracts_links_and_resolves_them() {
        let html = r##"<p>see <a href="/about">About</a> and
            <a href='https://x.com/y'>X</a> and <a href="#frag">skip</a></p>"##;
        let links = extract_links(html, "http://example.com/dir/page.html");
        // The fragment-only link is skipped; the others resolve.
        assert_eq!(links.len(), 2);
        assert_eq!(links[0], ("About".to_string(), "http://example.com:80/about".to_string()));
        assert_eq!(links[1].1, "https://x.com/y");
    }

    #[test]
    fn attr_and_strip_tags_helpers() {
        assert_eq!(attr_value("<a href=\"/x\" class=y>", "href").as_deref(), Some("/x"));
        assert_eq!(attr_value("<a href='/q'>", "href").as_deref(), Some("/q"));
        assert_eq!(strip_tags("a<b>bc</b>d"), "abcd");
    }

    #[test]
    fn notes_menu_actions_work() {
        let mut n = NotesApp::new();
        n.lines = vec!["keep".into(), "me".into()];
        n.do_action("new");
        assert_eq!(n.lines, vec![String::new()], "New clears the note");
        assert!(matches!(n.do_action("close"), AppControl::Close));
    }

    #[test]
    fn paint_sidebar_selects_tools() {
        let mut p = PaintApp::new();
        // Row 3 → the 3rd colour (index 2).
        p.sidebar_action(3);
        assert_eq!(p.sel, 2);
        assert!(!p.erasing);
        // Eraser toggle is the row after the 8 swatches.
        p.sidebar_action(9);
        assert!(p.erasing);
        // Size +/- rows.
        p.sidebar_action(12);
        assert_eq!(p.brush, 2);
        p.sidebar_action(11);
        assert_eq!(p.brush, 1);
    }

    #[test]
    fn paint_brush_paints_a_square_on_the_canvas() {
        let mut p = PaintApp::new();
        p.ensure_size(20, 20);
        p.brush = 2;
        p.erasing = false;
        // Click on the canvas (cols >= PAINT_CANVAS_X).
        p.touch(PAINT_CANVAS_X + 3, 4);
        // A 2×2 square should be painted at canvas (3,4).
        assert!(p.cells[4][3].is_some());
        assert!(p.cells[4][4].is_some());
        assert!(p.cells[5][3].is_some());
        assert!(p.cells[5][4].is_some());
        // A sidebar click must NOT paint.
        let before = p.cells[10][10];
        p.touch(1, 1);
        assert_eq!(p.cells[10][10], before);
    }

    // ─── messaging suite (Messages / DrDrCord / Mail / Calendar) ─────

    #[test]
    fn wrap_text_wraps_words_and_hard_breaks_long_ones() {
        // Plain wrap at a word boundary.
        assert_eq!(wrap_text("the quick brown fox", 9), vec!["the quick", "brown fox"]);
        // A word longer than the line is hard-split, never panicking.
        let v = wrap_text("supercalifragilistic", 6);
        assert!(v.iter().all(|l| l.chars().count() <= 6));
        assert_eq!(v.concat(), "supercalifragilistic");
        // Blank input still yields one (empty) line so a blank stays blank.
        assert_eq!(wrap_text("", 8), vec![String::new()]);
    }

    #[test]
    fn parse_channel_routes_tagged_lines_and_defaults_to_general() {
        assert_eq!(parse_channel("#help anyone there?"), (2, "anyone there?"));
        assert_eq!(parse_channel("#dev shipping it"), (3, "shipping it"));
        // Unknown tag or untagged line → #general (index 0), body unchanged.
        assert_eq!(parse_channel("#nope hi"), (0, "#nope hi"));
        assert_eq!(parse_channel("just chatting"), (0, "just chatting"));
    }

    #[test]
    fn user_color_is_stable_per_name() {
        assert_eq!(user_color("alice"), user_color("alice"));
        // Different names generally differ (these two are picked to).
        assert_ne!(user_color("alice"), user_color("bob"));
    }

    #[test]
    fn calendar_month_length_and_navigation() {
        // Known month lengths incl. a leap February.
        assert_eq!(CalendarApp::days_in_month(2026, 2), 28);
        assert_eq!(CalendarApp::days_in_month(2024, 2), 29);
        assert_eq!(CalendarApp::days_in_month(2026, 4), 30);
        assert_eq!(CalendarApp::days_in_month(2026, 12), 31);
        // Stepping a day off the end of December rolls into next January.
        let mut c = CalendarApp::new();
        c.year = 2026;
        c.month = 12;
        c.sel_day = 31;
        c.shift_day(1);
        assert_eq!((c.year, c.month, c.sel_day), (2027, 1, 1));
        // And back the other way.
        c.shift_day(-1);
        assert_eq!((c.year, c.month, c.sel_day), (2026, 12, 31));
    }

    #[test]
    fn mail_compose_send_moves_to_sent() {
        let mut m = MailApp::new();
        assert_eq!(m.folders[1].len(), 0); // Sent empty
        m.do_action("compose");
        // Fill the compose buffer directly.
        if let Some((msg, _)) = m.composing.as_mut() {
            msg.to = "bob".into();
            msg.subject = "hi".into();
            msg.body = "hello there".into();
        }
        m.finish_compose(true);
        assert_eq!(m.folders[1].len(), 1);
        assert_eq!(m.folder, 1);
        assert_eq!(m.folders[1][0].subject, "hi");
    }

    #[test]
    fn contacts_add_parses_name_and_detail() {
        let mut c = ContactsApp::new();
        let n0 = c.contacts.len();
        c.adding = true;
        c.input = "Ada Lovelace = user@example.invalid".into();
        c.commit_add();
        assert_eq!(c.contacts.len(), n0 + 1);
        let (name, detail) = c.contacts.last().unwrap();
        assert_eq!(name, "Ada Lovelace");
        assert_eq!(detail, "user@example.invalid");
    }
}
