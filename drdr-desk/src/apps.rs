//! The windowed apps DrDrDesk hosts.
//!
//! Each one implements [`WindowApp`]: it never opens `/dev/fb0`, never
//! puts a TTY in raw mode, never emits an escape sequence. It paints
//! characters into the [`TextGrid`] the window manager hands it and
//! reacts to [`KeyCode`]s. That's the whole "app inside a window"
//! mechanism — see `drdr-ui/src/window.rs` for why it's deliberately
//! not a terminal emulator.
//!
//! Selection highlight uses *reverse video* (swap fg/bg) rather than a
//! hard-coded accent colour, so apps stay theme-agnostic — they only
//! ever see the two colours the grid was created with.

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use drdr_net::status::{KIND_STAT_REQ, Stat, StatReq};
use drdr_net::Conn;
use drdr_ui::{AppControl, KeyCode, Px, TextGrid, WindowApp};

use nix::sys::reboot::{RebootMode, reboot};

/// Draw `s` at `(col, row)` in reverse video (selected-row look).
fn selected(grid: &mut TextGrid, row: u32, s: &str) {
    grid.fill_row(row, grid.bg(), grid.fg());
    grid.write(0, row, s, grid.bg(), grid.fg());
}

// ─── About ───────────────────────────────────────────────────────────

/// A static welcome card. Renders the same every frame — perfect for
/// proving the window chrome in a headless screenshot.
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
            "  * move a window  - drag its title bar",
            "  * switch windows - Alt-Tab",
            "  * close a window - click the [x] box",
            "  * mouse + keyboard, no X11, no Wayland",
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

/// A keyboard-driven directory browser. No subprocess, no `ls` — it
/// reads the filesystem itself and paints the listing into the grid.
pub struct FilesApp {
    cwd: PathBuf,
    items: Vec<Item>,
    sel: usize,
    scroll: usize,
    err: Option<String>,
}

impl FilesApp {
    pub fn new(start: impl Into<PathBuf>) -> Self {
        let mut a = Self {
            cwd: start.into(),
            items: Vec::new(),
            sel: 0,
            scroll: 0,
            err: None,
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

    fn enter(&mut self) {
        let Some(it) = self.items.get(self.sel) else { return };
        if it.name == ".." {
            if let Some(p) = self.cwd.parent() {
                self.cwd = p.to_path_buf();
                self.reload();
            }
        } else if it.is_dir {
            self.cwd.push(&it.name);
            self.reload();
        }
    }
}

impl WindowApp for FilesApp {
    fn title(&self) -> String {
        format!("DrDrFiles - {}", self.cwd.display())
    }

    fn on_key(&mut self, key: KeyCode) -> AppControl {
        let n = self.items.len();
        match key {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                if n > 0 {
                    self.sel = (self.sel + 1).min(n - 1);
                }
            }
            KeyCode::Home => self.sel = 0,
            KeyCode::End if n > 0 => self.sel = n - 1,
            KeyCode::Enter => self.enter(),
            KeyCode::Left | KeyCode::Backspace => {
                if let Some(p) = self.cwd.parent() {
                    self.cwd = p.to_path_buf();
                    self.reload();
                }
            }
            KeyCode::Char('r') => self.reload(),
            _ => {}
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        if let Some(e) = &self.err {
            g.text(1, 1, e);
            return;
        }
        let rows = g.rows as usize;
        let visible = rows.saturating_sub(1);
        // Keep the selection inside the viewport.
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if visible > 0 && self.sel >= self.scroll + visible {
            self.scroll = self.sel + 1 - visible;
        }

        let count = self.items.len();
        let header = format!("{} item(s)   Up/Dn Enter   Left/Bksp up   r reload", count);
        g.text(0, 0, &header);

        for vis in 0..visible {
            let idx = self.scroll + vis;
            if idx >= count {
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

// ─── System ──────────────────────────────────────────────────────────

/// The power menu (replaces Tier 1's launcher Reboot / Power off rows).
pub struct SystemApp {
    sel: usize,
}

impl SystemApp {
    pub fn new() -> Self {
        Self { sel: 0 }
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
            KeyCode::Enter | KeyCode::Space => {
                // On success reboot() never returns; under QEMU it exits
                // the VM. The error path only hits if we lack CAP_SYS_BOOT
                // (we won't — DrDrDesk runs from the initramfs).
                let mode = if self.sel == 0 {
                    RebootMode::RB_AUTOBOOT
                } else {
                    RebootMode::RB_POWER_OFF
                };
                let _ = reboot(mode);
            }
            _ => {}
        }
        AppControl::Continue
    }

    fn render(&mut self, g: &mut TextGrid) {
        g.text(1, 0, "Select, then press Enter:");
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

/// A live client of DrDrNet's Tier 3 async reactor. DrDrDesk runs the
/// reactor server in a background thread; this window re-connects every
/// heartbeat, sends a correlated `STAT` request over the binary frame
/// protocol, and renders the reply. This is the "exercised by a real
/// app, not just tests" part of the DrDrNet milestone.
pub struct NetApp {
    addr: Option<SocketAddr>,
    last: Result<Stat, String>,
    polls: u64,
}

impl NetApp {
    pub fn new(addr: Option<SocketAddr>) -> Self {
        Self {
            addr,
            last: Err("connecting...".into()),
            polls: 0,
        }
    }

    /// One request/reply round-trip. Short timeouts everywhere so a
    /// stalled server can never freeze the UI thread.
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
