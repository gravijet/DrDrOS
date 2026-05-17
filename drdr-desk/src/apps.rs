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
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use drdr_net::status::{KIND_STAT_REQ, Stat, StatReq};
use drdr_net::Conn;
use drdr_ui::{AppControl, KeyCode, Px, Rect, Spawn, TextGrid, Theme, WindowApp};

use nix::sys::reboot::{RebootMode, reboot};

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

// ─── About ───────────────────────────────────────────────────────────

/// A static welcome card.
pub struct AboutApp;

impl WindowApp for AboutApp {
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
        };
        a.reload();
        a
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
            self.spawns.push(Spawn {
                rect: spawn_rect(),
                app: Box::new(EditApp::new(path)),
            });
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
}

impl WindowApp for FilesApp {
    fn title(&self) -> String {
        format!("DrDrFiles - {}", self.cwd.display())
    }

    fn take_spawns(&mut self) -> Vec<Spawn> {
        std::mem::take(&mut self.spawns)
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
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

    fn on_click(&mut self, _col: u32, row: u32, double: bool) -> AppControl {
        // Row 0 is the header; entries start at row 1.
        if !matches!(self.mode, FMode::Browse) || row == 0 {
            return AppControl::Continue;
        }
        let idx = self.scroll + (row as usize - 1);
        if idx < self.items.len() {
            self.sel = idx;
            if double {
                self.activate();
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        if let Some(e) = &self.err {
            g.text(1, 1, e);
            g.text(1, 3, "(any key / r to reload)");
        }
        let rows = g.rows as usize;
        let visible = rows.saturating_sub(2);
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
        g.text(0, 0, &header);

        if self.err.is_some() {
            return;
        }
        for vis in 0..visible {
            let idx = self.scroll + vis;
            if idx >= self.items.len() {
                break;
            }
            let it = &self.items[idx];
            let tag = if it.is_dir { "[D] " } else { "    " };
            let line = format!("{tag}{}", it.name);
            let row = vis as u32 + 1;
            if idx == self.sel {
                selected(g, row, &line);
            } else {
                g.text(0, row, &line);
            }
        }
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
    cx: usize,
    cy: usize,
    top: usize,
    modified: bool,
    status: String,
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
        Self { path, lines, cx: 0, cy: 0, top: 0, modified: false, status }
    }

    fn cur_len(&self) -> usize {
        self.lines[self.cy].len()
    }

    fn clamp_cx(&mut self) {
        self.cx = self.cx.min(self.cur_len());
    }

    fn save(&mut self) -> bool {
        let body = self.lines.join("\n");
        match fs::write(&self.path, body) {
            Ok(()) => {
                self.modified = false;
                self.status = "saved".into();
                true
            }
            Err(e) => {
                self.status = format!("save failed: {e}");
                false
            }
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
            let (byte, _) = line.char_indices().nth(self.cx - 1).unwrap();
            line.remove(byte);
            self.cx -= 1;
            self.modified = true;
        } else if self.cy > 0 {
            let cur = self.lines.remove(self.cy);
            self.cy -= 1;
            self.cx = self.cur_len();
            self.lines[self.cy].push_str(&cur);
            self.modified = true;
        }
    }

    fn newline(&mut self) {
        let at = self.cx.min(self.cur_len());
        let rest = self.lines[self.cy].split_off(at);
        self.lines.insert(self.cy + 1, rest);
        self.cy += 1;
        self.cx = 0;
        self.modified = true;
    }
}

impl WindowApp for EditApp {
    fn title(&self) -> String {
        let star = if self.modified { "*" } else { "" };
        format!("DrDrEdit{star} - {}", self.path.display())
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        match key {
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
        // Row 0 is the status line; text starts at row 1.
        if row >= 1 {
            let target = self.top + (row as usize - 1);
            if target < self.lines.len() {
                self.cy = target;
                self.cx = (col as usize).min(self.cur_len());
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let rows = g.rows as usize;
        let text_rows = rows.saturating_sub(1);
        // Keep the caret line on screen.
        if self.cy < self.top {
            self.top = self.cy;
        } else if text_rows > 0 && self.cy >= self.top + text_rows {
            self.top = self.cy + 1 - text_rows;
        }

        let star = if self.modified { " *modified" } else { "" };
        g.text(
            0,
            0,
            &format!(
                "Esc save&close  arrows move  [{}]{star}  {}",
                self.lines.len(),
                self.status
            ),
        );

        for vis in 0..text_rows {
            let li = self.top + vis;
            if li >= self.lines.len() {
                break;
            }
            let row = vis as u32 + 1;
            g.text(0, row, &self.lines[li]);
            // Draw the caret as a reverse-video cell on its line.
            if li == self.cy {
                let line = &self.lines[li];
                let ch = line.chars().nth(self.cx).unwrap_or(' ');
                if (self.cx as u32) < g.cols {
                    g.put(self.cx as u32, row, ch, g.bg(), g.fg());
                }
            }
        }
    }
}

// ─── Launcher ────────────────────────────────────────────────────────

/// The way back: lists every app and opens a fresh window for the
/// chosen one (double-click or Enter). The window manager re-creates
/// this automatically whenever the desktop becomes empty, so closed
/// windows can always be reopened.
pub struct LauncherApp {
    items: Vec<(String, Box<dyn Fn() -> Spawn>)>,
    sel: usize,
    spawns: Vec<Spawn>,
}

/// The single source of truth for "every app you can open" — used by
/// both the Launcher window and the taskbar Start menu, so they can
/// never drift apart. Each entry is a label and a factory that builds
/// the window on demand.
pub fn app_catalog(
    net_addr: Option<SocketAddr>,
) -> Vec<(String, Box<dyn Fn() -> Spawn>)> {
    fn entry(
        label: &str,
        f: impl Fn() -> Box<dyn WindowApp> + 'static,
    ) -> (String, Box<dyn Fn() -> Spawn>) {
        (
            label.to_string(),
            Box::new(move || Spawn { rect: spawn_rect(), app: f() }),
        )
    }
    vec![
        entry("Files", || Box::new(FilesApp::new("/"))),
        entry("Text Editor", || Box::new(EditApp::new("/tmp/untitled.txt"))),
        entry("Notes (saved)", || Box::new(NotesApp::new())),
        entry("Calculator", || Box::new(CalcApp::new())),
        entry("Clock & Calendar", || Box::new(ClockApp::new())),
        entry("System Monitor", || Box::new(SysMonApp::new())),
        entry("DrDrConsole", || Box::new(ConsoleApp::new())),
        entry("Disks", || Box::new(DisksApp::new())),
        entry("Settings", move || Box::new(SettingsApp::new(net_addr))),
        entry("DrDrNet panel", move || Box::new(NetApp::new(net_addr))),
        entry("About DrDrOS", || Box::new(AboutApp)),
        entry("System (power)", || Box::new(SystemApp::new())),
    ]
}

impl LauncherApp {
    pub fn new(net_addr: Option<SocketAddr>) -> Self {
        Self { items: app_catalog(net_addr), sel: 0, spawns: Vec::new() }
    }

    fn launch(&mut self) {
        if let Some((_, factory)) = self.items.get(self.sel) {
            self.spawns.push(factory());
        }
    }
}

impl WindowApp for LauncherApp {
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
        for (i, (label, _)) in self.items.iter().enumerate() {
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

/// A live client of DrDrNet's Tier 3 async reactor.
pub struct NetApp {
    addr: Option<SocketAddr>,
    last: Result<Stat, String>,
    polls: u64,
}

impl NetApp {
    pub fn new(addr: Option<SocketAddr>) -> Self {
        Self { addr, last: Err("connecting...".into()), polls: 0 }
    }

    fn fetch(addr: SocketAddr) -> Result<Stat, String> {
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
    fn title(&self) -> String {
        match (&self.addr, &self.last) {
            (Some(_), Ok(_)) => "DrDrNet  * online".into(),
            _ => "DrDrNet  - offline".into(),
        }
    }

    fn on_tick(&mut self) -> AppControl {
        self.polls += 1;
        if let Some(addr) = self.addr {
            self.last = Self::fetch(addr);
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let teal = Px::rgb(0x3D, 0xD0, 0xBC);
        let red = Px::rgb(0xFF, 0x6B, 0x6B);
        let bg = g.bg();

        match (&self.addr, &self.last) {
            (None, _) => {
                g.write(1, 1, "loopback unavailable", red, bg);
                g.text(1, 3, "lo didn't come up at boot, so the");
                g.text(1, 4, "reactor server has nowhere to bind.");
            }
            (Some(addr), Ok(s)) => {
                g.write(1, 1, "* connected", teal, bg);
                g.text(1, 3, &format!("server   {addr}"));
                g.text(1, 4, &format!("host     {}", s.host));
                g.text(1, 5, &format!("uptime   {} s", s.uptime_secs));
                g.text(1, 6, &format!("served   {} requests", s.requests));
                g.text(1, 7, &format!("polls    {} (this window)", self.polls));
                g.text(1, 9, "transport: DrDrNet binary frames");
                g.text(1, 10, "server:    Tier 3 epoll reactor");
                g.text(1, 11, "           (async, one thread)");
            }
            (Some(addr), Err(e)) => {
                g.write(1, 1, "x disconnected", red, bg);
                g.text(1, 3, &format!("server {addr}"));
                let msg = if e.len() > (g.cols as usize).saturating_sub(2) {
                    &e[..(g.cols as usize).saturating_sub(2)]
                } else {
                    e
                };
                g.text(1, 5, msg);
                g.text(1, 7, "retrying every heartbeat...");
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
    net_addr: Option<SocketAddr>,
    sel: usize,
    spawns: Vec<Spawn>,
}

const SETTINGS_ROWS: usize = 3;

impl SettingsApp {
    pub fn new(net_addr: Option<SocketAddr>) -> Self {
        Self { net_addr, sel: 0, spawns: Vec::new() }
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
                app: Box::new(NetApp::new(self.net_addr)),
            }),
        }
    }
}

impl WindowApp for SettingsApp {
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
        }
    }

    fn cur_len(&self) -> usize {
        self.lines[self.cy].chars().count()
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
    fn title(&self) -> String {
        format!(
            "Notes - {}{}",
            self.name,
            if self.modified { " *" } else { "" }
        )
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
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
        if row >= 1 {
            let target = self.top + (row as usize - 1);
            if target < self.lines.len() {
                self.cy = target;
                self.cx = (col as usize).min(self.cur_len());
            }
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        let rows = g.rows as usize;
        let text_rows = rows.saturating_sub(1);
        if self.cy < self.top {
            self.top = self.cy;
        } else if text_rows > 0 && self.cy >= self.top + text_rows {
            self.top = self.cy + 1 - text_rows;
        }
        g.text(
            0,
            0,
            &format!("Esc=save  [{} lines]  {}", self.lines.len(), self.status),
        );
        for vis in 0..text_rows {
            let li = self.top + vis;
            if li >= self.lines.len() {
                break;
            }
            let row = vis as u32 + 1;
            g.text(0, row, &self.lines[li]);
            if li == self.cy {
                let ch = self.lines[li].chars().nth(self.cx).unwrap_or(' ');
                if (self.cx as u32) < g.cols {
                    g.put(self.cx as u32, row, ch, g.bg(), g.fg());
                }
            }
        }
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
}

impl CalcApp {
    pub fn new() -> Self {
        Self { expr: String::new(), result: None, error: None }
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
    fn title(&self) -> String {
        "Calculator".into()
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
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
            KeyCode::Escape | KeyCode::Char('c') | KeyCode::Char('C') => {
                self.expr.clear();
                self.result = None;
                self.error = None;
            }
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, col: u32, row: u32, _d: bool) -> AppControl {
        // The keypad starts at grid row 4; each key is 5 cols wide.
        if row >= 4 {
            let kr = (row as usize - 4) / 2;
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
        g.text(0, 0, "Calculator  (type, Enter==, c=clear, Bksp)");
        let shown = if self.expr.is_empty() { "0" } else { &self.expr };
        g.text(0, 2, &format!("> {shown}"));
        if let Some(v) = self.result {
            let teal = Px::rgb(0x1F, 0x9E, 0x55);
            g.write(0, 3, &format!("= {}", trim_float(v)), teal, g.bg());
        } else if let Some(e) = &self.error {
            let red = Px::rgb(0xC8, 0x2B, 0x2B);
            g.write(0, 3, &format!("! {e}"), red, g.bg());
        }
        for (r, krow) in KEYPAD.iter().enumerate() {
            let row = 4 + r as u32 * 2;
            let mut line = String::new();
            for k in krow {
                line.push_str(&format!("[ {k} ]"));
            }
            g.text(0, row, &line);
        }
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
}
