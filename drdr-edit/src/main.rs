//! drdr-edit — DrDrEdit, the DrDrOS text editor (Tier 2).
//!
//! Tier 2 promotes the editor from line-oriented (`ed`-style) to a
//! full-screen modal editor in the vi tradition:
//!
//!   - **NORMAL** mode for navigation and structural edits.
//!   - **INSERT** mode for typing characters into the buffer.
//!
//! Like `drdr-files -i`, all rendering is direct ANSI escape sequences,
//! and the terminal is reset by the [`RawMode`] RAII guard from
//! `drdr-tty` on every exit path — panics included.
//!
//! NORMAL keys
//! ───────────
//!   h / Left      move left           j / Down  move down
//!   k / Up        move up             l / Right move right
//!   0  / Home     start of line       $ / End   end of line
//!   G             jump to last line
//!   i             enter INSERT at cursor
//!   a             enter INSERT one column after cursor
//!   A             enter INSERT at end of line
//!   o             open new line below + INSERT
//!   O             open new line above + INSERT
//!   x             delete the char under the cursor
//!   s             save (write to file)
//!   q             quit (refuses if unsaved)
//!   !             force-quit, discarding unsaved changes
//!   ?             show help overlay
//!
//! INSERT keys
//! ───────────
//!   Esc           back to NORMAL
//!   Enter         split line at cursor
//!   Backspace     delete previous char (joins lines at column 0)
//!   any char      insert at cursor
//!
//! ASCII-only for Tier 2; Tier 3 will add UTF-8 width handling along
//! with the framebuffer port.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use drdr_tty::{read_key, term_size, Key, RawMode};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let path: Option<PathBuf> = args.next().map(PathBuf::from);
    if args.next().is_some() {
        eprintln!("drdr-edit: only one FILE argument is supported");
        return ExitCode::from(2);
    }

    let mut state = match State::load(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("drdr-edit: {e}");
            return ExitCode::from(1);
        }
    };

    let _raw = match RawMode::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("drdr-edit: failed to enter raw mode: {e}");
            return ExitCode::from(1);
        }
    };

    let exit = loop {
        let (rows, cols) = term_size();
        if let Err(e) = render(&state, rows, cols) {
            eprintln!("drdr-edit: render: {e}");
            break ExitCode::from(1);
        }
        let key = match read_key() {
            Ok(k) => k,
            Err(e) => {
                eprintln!("drdr-edit: read: {e}");
                break ExitCode::from(1);
            }
        };
        match state.mode {
            Mode::Normal => match handle_normal(&mut state, key) {
                NormalOutcome::Continue => {}
                NormalOutcome::Quit(c) => break c,
            },
            Mode::Insert => handle_insert(&mut state, key),
        }
    };

    drop(_raw); // explicit restore before the return so the message is on a clean line.
    if let Some(msg) = state.farewell.take() {
        println!("{msg}");
    }
    exit
}

// ─── State ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
}

struct State {
    path: Option<PathBuf>,
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize, // char index within the row
    top_row: usize,    // viewport scroll
    mode: Mode,
    modified: bool,
    /// Sticky status line message; cleared on next keystroke.
    message: Option<String>,
    /// Final note printed AFTER raw mode is restored (e.g. "saved X bytes").
    farewell: Option<String>,
}

impl State {
    fn load(path: Option<PathBuf>) -> io::Result<Self> {
        let lines = match &path {
            Some(p) => match fs::read_to_string(p) {
                Ok(text) => split_lines(&text),
                Err(e) if e.kind() == io::ErrorKind::NotFound => vec![String::new()],
                Err(e) => return Err(e),
            },
            None => vec![String::new()],
        };
        Ok(Self {
            path,
            lines,
            cursor_row: 0,
            cursor_col: 0,
            top_row: 0,
            mode: Mode::Normal,
            modified: false,
            message: None,
            farewell: None,
        })
    }

    fn current_line_len(&self) -> usize {
        self.lines.get(self.cursor_row).map(|s| s.chars().count()).unwrap_or(0)
    }

    fn clamp_cursor(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = self.cursor_row.min(self.lines.len() - 1);
        // In Normal mode the cursor sits ON a char, so max col == len-1.
        // In Insert mode the cursor sits BETWEEN chars, so max col == len.
        let line_len = self.current_line_len();
        let max_col = match self.mode {
            Mode::Normal => line_len.saturating_sub(1),
            Mode::Insert => line_len,
        };
        self.cursor_col = self.cursor_col.min(max_col);
    }
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    if lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

// ─── NORMAL mode ─────────────────────────────────────────────────────

enum NormalOutcome {
    Continue,
    Quit(ExitCode),
}

fn handle_normal(state: &mut State, key: Key) -> NormalOutcome {
    state.message = None;
    match key {
        Key::Char('h') | Key::Left => {
            state.cursor_col = state.cursor_col.saturating_sub(1);
        }
        Key::Char('l') | Key::Right => {
            let max = state.current_line_len().saturating_sub(1);
            if state.cursor_col < max {
                state.cursor_col += 1;
            }
        }
        Key::Char('k') | Key::Up => {
            state.cursor_row = state.cursor_row.saturating_sub(1);
            state.clamp_cursor();
        }
        Key::Char('j') | Key::Down => {
            if state.cursor_row + 1 < state.lines.len() {
                state.cursor_row += 1;
            }
            state.clamp_cursor();
        }
        Key::Char('0') | Key::Home => state.cursor_col = 0,
        Key::Char('$') | Key::End => {
            state.cursor_col = state.current_line_len().saturating_sub(1);
        }
        Key::Char('G') => {
            state.cursor_row = state.lines.len() - 1;
            state.clamp_cursor();
        }
        Key::Char('i') => state.mode = Mode::Insert,
        Key::Char('a') => {
            if !state.lines[state.cursor_row].is_empty() {
                state.cursor_col += 1;
            }
            state.mode = Mode::Insert;
        }
        Key::Char('A') => {
            state.cursor_col = state.current_line_len();
            state.mode = Mode::Insert;
        }
        Key::Char('o') => {
            let row = state.cursor_row;
            state.lines.insert(row + 1, String::new());
            state.cursor_row = row + 1;
            state.cursor_col = 0;
            state.mode = Mode::Insert;
            state.modified = true;
        }
        Key::Char('O') => {
            let row = state.cursor_row;
            state.lines.insert(row, String::new());
            state.cursor_col = 0;
            state.mode = Mode::Insert;
            state.modified = true;
        }
        Key::Char('x') => {
            let line = &mut state.lines[state.cursor_row];
            let n_chars = line.chars().count();
            if n_chars == 0 {
                return NormalOutcome::Continue;
            }
            let byte = char_index_to_byte(line, state.cursor_col);
            let next_byte = next_char_boundary(line, byte);
            line.replace_range(byte..next_byte, "");
            state.modified = true;
            if state.cursor_col >= n_chars - 1 {
                state.cursor_col = (n_chars - 1).saturating_sub(1);
            }
            state.clamp_cursor();
        }
        Key::Char('s') => save(state),
        Key::Char('q') => {
            if state.modified {
                state.message = Some("unsaved changes — `s` to save, `!` to force-quit".into());
            } else {
                return NormalOutcome::Quit(ExitCode::SUCCESS);
            }
        }
        Key::Char('!') => return NormalOutcome::Quit(ExitCode::SUCCESS),
        Key::Char('?') => show_help(state),
        _ => {}
    }
    NormalOutcome::Continue
}

fn save(state: &mut State) {
    let Some(path) = state.path.clone() else {
        state.message = Some("no filename — start drdr-edit with a path to save".into());
        return;
    };
    let mut body = state.lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    match fs::write(&path, body.as_bytes()) {
        Ok(()) => {
            state.modified = false;
            state.message = Some(format!(
                "wrote {} ({} lines, {} bytes)",
                path.display(),
                state.lines.len(),
                body.len()
            ));
        }
        Err(e) => state.message = Some(format!("save: {e}")),
    }
}

fn show_help(state: &mut State) {
    state.message = Some(
        "NORMAL: h/j/k/l move · i/a/A insert · o/O open · x del · s save · q quit · ! force"
            .into(),
    );
}

// ─── INSERT mode ─────────────────────────────────────────────────────

fn handle_insert(state: &mut State, key: Key) {
    state.message = None;
    match key {
        Key::Escape => {
            state.mode = Mode::Normal;
            // Vi convention: leaving insert moves cursor one left, but
            // never past the line start.
            state.cursor_col = state.cursor_col.saturating_sub(1);
            state.clamp_cursor();
        }
        Key::Enter => {
            let row = state.cursor_row;
            let line = state.lines[row].clone();
            let byte = char_index_to_byte(&line, state.cursor_col);
            let (left, right) = line.split_at(byte);
            state.lines[row] = left.to_string();
            state.lines.insert(row + 1, right.to_string());
            state.cursor_row = row + 1;
            state.cursor_col = 0;
            state.modified = true;
        }
        Key::Backspace => {
            if state.cursor_col > 0 {
                let line = &mut state.lines[state.cursor_row];
                let target_col = state.cursor_col - 1;
                let start = char_index_to_byte(line, target_col);
                let end = char_index_to_byte(line, state.cursor_col);
                line.replace_range(start..end, "");
                state.cursor_col = target_col;
                state.modified = true;
            } else if state.cursor_row > 0 {
                // Join this line onto the previous.
                let prev = state.lines.remove(state.cursor_row);
                state.cursor_row -= 1;
                state.cursor_col = state.lines[state.cursor_row].chars().count();
                state.lines[state.cursor_row].push_str(&prev);
                state.modified = true;
            }
        }
        Key::Char(c) => {
            let line = &mut state.lines[state.cursor_row];
            let byte = char_index_to_byte(line, state.cursor_col);
            line.insert(byte, c);
            state.cursor_col += 1;
            state.modified = true;
        }
        _ => {}
    }
}

// ─── Rendering ───────────────────────────────────────────────────────

fn render(state: &State, rows: u16, cols: u16) -> io::Result<()> {
    let body_rows = rows.saturating_sub(2).max(1) as usize; // top status + bottom status
    let top = compute_top(state.cursor_row, state.top_row, body_rows, state.lines.len());

    let mut buf = String::with_capacity(8 * 1024);
    buf.push_str("\x1b[2J\x1b[H");

    // Top status line.
    let path_label = state
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<no file>".into());
    let mode_label = match state.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
    };
    let modified = if state.modified { " ●" } else { "" };
    let header = format!(" DrDrEdit · {mode_label} · {path_label}{modified}");
    buf.push_str("\x1b[7m");
    pad_line(&mut buf, &header, cols);
    buf.push_str("\x1b[0m\r\n");

    // Body. One line per row in the viewport; ~ for rows past EOF.
    for i in 0..body_rows {
        let idx = top + i;
        if idx < state.lines.len() {
            let line = &state.lines[idx];
            pad_line(&mut buf, line, cols);
        } else {
            buf.push('~');
            for _ in 1..cols as usize {
                buf.push(' ');
            }
        }
        buf.push_str("\r\n");
    }

    // Bottom status line.
    let pos = format!(" L{}:C{}", state.cursor_row + 1, state.cursor_col + 1);
    let footer = match &state.message {
        Some(msg) => format!("{pos}  {msg}"),
        None => format!("{pos}  press ? for help"),
    };
    buf.push_str("\x1b[7m");
    pad_line(&mut buf, &footer, cols);
    buf.push_str("\x1b[0m");

    // Position the cursor at its logical spot in the viewport. ANSI
    // CUP uses 1-based row/col coords.
    let screen_row = (state.cursor_row - top) as u16 + 2; // +1 for header, +1 for 1-based.
    let screen_col = state.cursor_col as u16 + 1;
    buf.push_str(&format!("\x1b[{screen_row};{screen_col}H"));
    // Show the cursor (RawMode hid it on entry).
    buf.push_str("\x1b[?25h");

    let mut out = io::stdout().lock();
    out.write_all(buf.as_bytes())?;
    out.flush()
}

fn compute_top(cursor: usize, top: usize, body: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    if cursor < top {
        cursor
    } else if cursor >= top + body {
        cursor + 1 - body
    } else {
        top
    }
}

fn pad_line(out: &mut String, line: &str, cols: u16) {
    let mut written = 0;
    for c in line.chars() {
        if written + 1 > cols as usize {
            break;
        }
        out.push(c);
        written += 1;
    }
    for _ in written..cols as usize {
        out.push(' ');
    }
}

// ─── Char-index ↔ byte-index helpers ─────────────────────────────────
// We expose the cursor as a char index because that's what users mean
// by "column N". String mutation needs byte indices, so the helpers
// convert between them.

fn char_index_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

fn next_char_boundary(s: &str, byte_idx: usize) -> usize {
    if byte_idx >= s.len() {
        return s.len();
    }
    s[byte_idx..]
        .char_indices()
        .nth(1)
        .map(|(b, _)| byte_idx + b)
        .unwrap_or(s.len())
}
