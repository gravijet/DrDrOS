//! Interactive directory browser for DrDrFiles Tier 2.
//!
//! Full-screen TUI. The screen layout is:
//!
//!     ┌─ header ────────────────────────────────────────────────┐
//!     │ DrDrFiles  ·  /current/path                              │
//!     ├──────────────────────────────────────────────────────────┤
//!     │ > DIR       -  some-folder/                              │  ← cursor row
//!     │   F     1.4K  a-file.txt                                 │
//!     │   …                                                      │
//!     ├──────────────────────────────────────────────────────────┤
//!     │ ↑/↓ move · Enter open · Backspace up · q quit            │
//!     └──────────────────────────────────────────────────────────┘
//!
//! All drawing is direct ANSI escape sequences — no ncurses, no termion.

use std::env;
use std::fs::{self, FileType, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use drdr_tty::{read_key, term_size, Key, RawMode};

/// Single entry in the list view.
struct Entry {
    name: String,
    display: String,
    kind_tag: &'static str,
    size: String,
    is_dir: bool,
}

/// Browser state — pure data, separate from rendering. Rendering is a
/// function of (state, term_size) so we can redraw any time.
struct State {
    cwd: PathBuf,
    entries: Vec<Entry>,
    cursor: usize,
    scroll: usize,
    show_hidden: bool,
    /// Sticky status message shown on the bottom line until the next key.
    message: Option<String>,
}

impl State {
    fn new(start: PathBuf, show_hidden: bool) -> io::Result<Self> {
        let entries = read_entries(&start, show_hidden)?;
        Ok(Self { cwd: start, entries, cursor: 0, scroll: 0, show_hidden, message: None })
    }

    fn reload(&mut self) -> io::Result<()> {
        self.entries = read_entries(&self.cwd, self.show_hidden)?;
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        self.scroll = 0;
        Ok(())
    }
}

pub fn run(start: PathBuf, show_hidden: bool) -> io::Result<()> {
    let mut state = State::new(start.canonicalize().unwrap_or(start), show_hidden)?;
    let _raw = RawMode::enter()?; // RAII: restored on every return path.

    loop {
        let (rows, cols) = term_size();
        render(&state, rows, cols)?;

        match read_key()? {
            Key::Char('q') | Key::Escape => return Ok(()),
            Key::Up | Key::Char('k') => move_cursor(&mut state, -1),
            Key::Down | Key::Char('j') => move_cursor(&mut state, 1),
            Key::Home => state.cursor = 0,
            Key::End => state.cursor = state.entries.len().saturating_sub(1),
            Key::PageUp => move_cursor(&mut state, -(rows as isize / 2).max(1)),
            Key::PageDown => move_cursor(&mut state, (rows as isize / 2).max(1)),
            Key::Enter | Key::Right | Key::Char('l') => descend(&mut state),
            Key::Backspace | Key::Left | Key::Char('h') => ascend(&mut state),
            Key::Char('.') => {
                state.show_hidden = !state.show_hidden;
                if let Err(e) = state.reload() {
                    state.message = Some(format!("reload: {e}"));
                }
            }
            Key::Char('r') => {
                if let Err(e) = state.reload() {
                    state.message = Some(format!("reload: {e}"));
                }
            }
            _ => {}
        }
    }
}

fn move_cursor(state: &mut State, delta: isize) {
    if state.entries.is_empty() {
        return;
    }
    let len = state.entries.len() as isize;
    let cur = state.cursor as isize + delta;
    state.cursor = cur.clamp(0, len - 1) as usize;
    state.message = None;
}

fn descend(state: &mut State) {
    let Some(entry) = state.entries.get(state.cursor) else { return };
    if !entry.is_dir {
        state.message = Some(format!("{}: not a directory", entry.name));
        return;
    }
    let target = state.cwd.join(&entry.name);
    match read_entries(&target, state.show_hidden) {
        Ok(entries) => {
            state.cwd = target.canonicalize().unwrap_or(target);
            state.entries = entries;
            state.cursor = 0;
            state.scroll = 0;
            state.message = None;
        }
        Err(e) => state.message = Some(format!("open: {e}")),
    }
}

fn ascend(state: &mut State) {
    let Some(parent) = state.cwd.parent().map(PathBuf::from) else {
        state.message = Some("already at /".into());
        return;
    };
    // Remember the name we just came from so we can place the cursor on
    // the source dir in the parent — much nicer than always landing at row 0.
    let prev_name = state
        .cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned());

    match read_entries(&parent, state.show_hidden) {
        Ok(entries) => {
            state.cwd = parent;
            if let Some(name) = prev_name {
                if let Some(idx) = entries.iter().position(|e| e.name == name) {
                    state.cursor = idx;
                } else {
                    state.cursor = 0;
                }
            }
            state.entries = entries;
            state.scroll = 0;
            state.message = None;
        }
        Err(e) => state.message = Some(format!("ascend: {e}")),
    }
}

// ─── Rendering ───────────────────────────────────────────────────────

fn render(state: &State, rows: u16, cols: u16) -> io::Result<()> {
    let mut buf = String::with_capacity(8 * 1024);

    // Clear screen + home cursor.
    buf.push_str("\x1b[2J\x1b[H");

    // Header.
    let path_label = truncate(&state.cwd.to_string_lossy(), cols as usize - 14);
    buf.push_str("\x1b[7m"); // inverse
    pad_line(&mut buf, &format!(" DrDrFiles  ·  {path_label}"), cols);
    buf.push_str("\x1b[0m");
    buf.push_str("\r\n");

    // Body. Reserve 3 rows: header (above), separator below, footer help.
    let body_rows = rows.saturating_sub(3) as usize;
    if body_rows == 0 {
        return flush(&buf);
    }

    // Recompute scroll so cursor stays on screen.
    let scroll = compute_scroll(state.cursor, state.scroll, body_rows, state.entries.len());

    for i in 0..body_rows {
        let idx = scroll + i;
        if idx >= state.entries.len() {
            buf.push_str("\x1b[K\r\n"); // clear-to-eol + newline
            continue;
        }
        let entry = &state.entries[idx];
        let marker = if idx == state.cursor { ">" } else { " " };
        let line = format!("{marker} {} {:>7}  {}", entry.kind_tag, entry.size, entry.display);
        if idx == state.cursor {
            buf.push_str("\x1b[7m"); // inverse
            pad_line(&mut buf, &line, cols);
            buf.push_str("\x1b[0m");
        } else {
            pad_line(&mut buf, &line, cols);
        }
        buf.push_str("\r\n");
    }

    // Footer / status.
    let footer = match &state.message {
        Some(msg) => format!(" {msg}"),
        None => " ↑/↓ move · Enter open · Backspace up · . toggle hidden · r reload · q quit".into(),
    };
    buf.push_str("\x1b[7m");
    pad_line(&mut buf, &footer, cols);
    buf.push_str("\x1b[0m");

    flush(&buf)
}

fn compute_scroll(cursor: usize, scroll: usize, body_rows: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    if cursor < scroll {
        cursor
    } else if cursor >= scroll + body_rows {
        cursor + 1 - body_rows
    } else {
        scroll
    }
}

fn pad_line(out: &mut String, line: &str, cols: u16) {
    let trimmed = truncate(line, cols as usize);
    out.push_str(&trimmed);
    for _ in trimmed.chars().count()..cols as usize {
        out.push(' ');
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.into();
    }
    if max < 1 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn flush(buf: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(buf.as_bytes())?;
    out.flush()
}

// ─── Directory reading (shared with batch mode) ──────────────────────

fn read_entries(path: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let meta = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        entries.push(make_entry(&entry.path(), name, &meta));
    }
    entries.sort_by(|a, b| (a.is_dir.cmp(&b.is_dir).reverse()).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

fn make_entry(path: &Path, name: String, meta: &Metadata) -> Entry {
    let ft: FileType = meta.file_type();
    let (kind_tag, is_dir) = if ft.is_dir() {
        ("DIR", true)
    } else if ft.is_symlink() {
        ("LNK", false)
    } else {
        ("  F", false)
    };
    let size = if ft.is_dir() || ft.is_symlink() {
        "-".to_string()
    } else {
        crate::list::human_size(meta.len())
    };

    let mut display = name.clone();
    if ft.is_dir() {
        display.push('/');
    } else if ft.is_symlink() {
        if let Ok(target) = fs::read_link(path) {
            display.push_str(" -> ");
            display.push_str(&target.to_string_lossy());
        }
    }

    Entry { name, display, kind_tag, size, is_dir }
}

#[allow(dead_code)]
fn cwd_or_root() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}
