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
use std::time::Duration;

use drdr_net::status::{KIND_STAT_REQ, Stat, StatReq};
use drdr_net::Conn;
use drdr_ui::{AppControl, KeyCode, Px, Rect, Spawn, TextGrid, WindowApp};

use nix::sys::reboot::{RebootMode, reboot};

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
            "DrDrOS - graphical session (DrDrDesk Tier 2)",
            "",
            "A complete custom userland on the Linux kernel,",
            "written from scratch in Rust. Framebuffer only,",
            "runs from RAM, every component is ours.",
            "",
            "This is a real window manager:",
            "  * move a window   - drag its title bar",
            "  * switch windows  - Alt-Tab",
            "  * open a window   - double-click an icon/row",
            "  * close a window  - click the [x] box",
            "  * reopen anything - close all -> Launcher returns",
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
    net_addr: Option<SocketAddr>,
    sel: usize,
    spawns: Vec<Spawn>,
}

const LAUNCH_ITEMS: [&str; 5] =
    ["DrDrFiles  (/)", "Text editor", "About DrDrOS", "DrDrNet panel", "System"];

impl LauncherApp {
    pub fn new(net_addr: Option<SocketAddr>) -> Self {
        Self { net_addr, sel: 0, spawns: Vec::new() }
    }

    fn launch(&mut self) {
        let app: Box<dyn WindowApp> = match self.sel {
            0 => Box::new(FilesApp::new("/")),
            1 => Box::new(EditApp::new("/tmp/untitled.txt")),
            2 => Box::new(AboutApp),
            3 => Box::new(NetApp::new(self.net_addr)),
            _ => Box::new(SystemApp::new()),
        };
        self.spawns.push(Spawn { rect: spawn_rect(), app });
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
            KeyCode::Down => self.sel = (self.sel + 1).min(LAUNCH_ITEMS.len() - 1),
            KeyCode::Enter | KeyCode::Space | KeyCode::Right => self.launch(),
            _ => {}
        }
        AppControl::Continue
    }

    fn on_click(&mut self, _col: u32, row: u32, double: bool) -> AppControl {
        if row >= 2 {
            let idx = row as usize - 2;
            if idx < LAUNCH_ITEMS.len() {
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
        for (i, label) in LAUNCH_ITEMS.iter().enumerate() {
            let row = i as u32 + 2;
            let line = format!("  {label}");
            if i == self.sel {
                selected(g, row, &line);
            } else {
                g.text(0, row, &line);
            }
        }
        g.text(1, LAUNCH_ITEMS.len() as u32 + 3, "This window reappears if you close everything.");
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
