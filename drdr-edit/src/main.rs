//! drdr-edit — DrDrEdit, the DrDrOS text editor (Tier 1).
//!
//! Tier 1 is a line-oriented editor in the spirit of `ed(1)`: every command
//! is one letter, the document lives entirely in RAM, and nothing touches
//! disk until the user explicitly runs `w`. The model is intentionally
//! minimal so the read-mutate-write cycle is small enough to hold in your
//! head.
//!
//! Commands (each on its own line):
//!
//!     p              print all lines (with line numbers)
//!     p N            print line N
//!     p N,M          print lines N..=M
//!     a N            append: insert new lines AFTER line N, end with '.'
//!                    (a 0 inserts at the very top of the file)
//!     i N            insert: same as `a` but BEFORE line N
//!     c N            change: replace line N with new lines, end with '.'
//!     d N            delete line N
//!     d N,M          delete lines N..=M
//!     s N TEXT       set: replace line N with the single line TEXT
//!     w              write to the original path
//!     w FILE         write-as to FILE
//!     q              quit (errors if unsaved; use Q to override)
//!     Q              quit, discarding unsaved changes
//!     =              status (line count, modified flag, path)
//!     h              help
//!
//! Tier 2 swaps this for a termios raw-mode full-screen editor; Tier 3
//! (Phase 3) replaces stdin with framebuffer-driven input.
//!
//! Usage:
//!
//!     drdr-edit [FILE]
//!
//! If FILE exists it is loaded; if it does not, the editor starts empty
//! and the file is created on the first `w`.

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

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

    print_intro(&state);

    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut stdout = io::stdout();
    let mut line = String::new();

    loop {
        let _ = write!(stdout, "drdred> ");
        let _ = stdout.flush();

        line.clear();
        match handle.read_line(&mut line) {
            Ok(0) => {
                // EOF — treat like 'q'; bail if unsaved.
                println!();
                if state.modified {
                    eprintln!("drdred: unsaved changes (use Q to discard, w to save)");
                    return ExitCode::from(1);
                }
                return ExitCode::SUCCESS;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("drdred: read error: {e}");
                return ExitCode::from(1);
            }
        }

        let cmd = line.trim_end_matches('\n').to_string();
        match execute(&mut state, &cmd, &mut handle) {
            ControlFlow::Continue => {}
            ControlFlow::Quit(code) => return code,
        }
    }
}

fn print_intro(state: &State) {
    match &state.path {
        Some(p) if state.created => {
            println!("drdr-edit: starting new file {} ({} lines)", p.display(), state.lines.len());
        }
        Some(p) => {
            println!("drdr-edit: loaded {} ({} lines)", p.display(), state.lines.len());
        }
        None => {
            println!("drdr-edit: in-memory buffer ({} lines) — `w FILE` to save somewhere", state.lines.len());
        }
    }
    println!("Type `h` for help, `q` to quit.");
}

/// All editor state. `lines` is the document; `modified` is set true on
/// every mutation and cleared on a successful `w`. `created` records
/// whether we started from an empty buffer (so the intro line is honest).
struct State {
    path: Option<PathBuf>,
    lines: Vec<String>,
    modified: bool,
    created: bool,
}

impl State {
    fn load(path: Option<PathBuf>) -> io::Result<Self> {
        let Some(ref p) = path else {
            return Ok(Self { path, lines: Vec::new(), modified: false, created: true });
        };
        match fs::read_to_string(p) {
            Ok(text) => {
                let lines = if text.is_empty() {
                    Vec::new()
                } else {
                    // Split on '\n'; if the file ends with a newline,
                    // `split` leaves a trailing empty string we trim,
                    // matching how editors usually treat trailing EOL.
                    let mut v: Vec<String> = text.split('\n').map(String::from).collect();
                    if v.last().is_some_and(|s| s.is_empty()) {
                        v.pop();
                    }
                    v
                };
                Ok(Self { path, lines, modified: false, created: false })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok(Self { path, lines: Vec::new(), modified: false, created: true })
            }
            Err(e) => Err(e),
        }
    }
}

/// What the command dispatcher wants the main loop to do next.
enum ControlFlow {
    Continue,
    Quit(ExitCode),
}

/// Execute a single command line. `input` is passed in because some
/// commands (`a`, `i`, `c`) read additional lines until a lone `.`.
fn execute(state: &mut State, cmd: &str, input: &mut impl BufRead) -> ControlFlow {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return ControlFlow::Continue;
    }

    // Split the first whitespace-delimited token off as the verb. The
    // rest is the argument string, which individual handlers parse.
    let (verb, rest) = match cmd.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim_start()),
        None => (cmd, ""),
    };

    match verb {
        "p" => cmd_print(state, rest),
        "a" => cmd_insert(state, rest, InsertWhere::After, input),
        "i" => cmd_insert(state, rest, InsertWhere::Before, input),
        "c" => cmd_change(state, rest, input),
        "d" => cmd_delete(state, rest),
        "s" => cmd_set(state, rest),
        "w" => cmd_write(state, rest),
        "q" => {
            if state.modified {
                eprintln!("drdred: unsaved changes (use Q to discard, w to save)");
            } else {
                return ControlFlow::Quit(ExitCode::SUCCESS);
            }
        }
        "Q" => return ControlFlow::Quit(ExitCode::SUCCESS),
        "=" => cmd_status(state),
        "h" | "?" => cmd_help(),
        other => eprintln!("drdred: unknown command '{other}' (try `h`)"),
    }
    ControlFlow::Continue
}

// ─── Commands ────────────────────────────────────────────────────────

fn cmd_print(state: &State, rest: &str) {
    let (start, end) = match parse_range(rest, state.lines.len()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("drdred: p: {e}");
            return;
        }
    };
    if state.lines.is_empty() {
        println!("(empty buffer)");
        return;
    }
    // `lines` is 0-indexed, the user thinks in 1-indexed line numbers, so
    // we translate at the boundary. `start..=end` is inclusive — matches
    // ed's convention.
    let width = digit_count(state.lines.len());
    for (idx, line) in state.lines[start - 1..end].iter().enumerate() {
        let n = start + idx;
        println!("{n:>width$}  {line}", width = width);
    }
}

#[derive(Clone, Copy)]
enum InsertWhere {
    Before,
    After,
}

fn cmd_insert(state: &mut State, rest: &str, where_: InsertWhere, input: &mut impl BufRead) {
    let pos_arg = match parse_line_number(rest, state.lines.len(), where_) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("drdred: insert: {e}");
            return;
        }
    };
    let insert_at = match where_ {
        InsertWhere::Before => pos_arg.saturating_sub(1),
        InsertWhere::After => pos_arg,
    };
    let new_lines = match read_until_dot(input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("drdred: insert: read error: {e}");
            return;
        }
    };
    if new_lines.is_empty() {
        return;
    }
    state.lines.splice(insert_at..insert_at, new_lines);
    state.modified = true;
}

fn cmd_change(state: &mut State, rest: &str, input: &mut impl BufRead) {
    let n = match parse_existing_line(rest, state.lines.len()) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("drdred: c: {e}");
            return;
        }
    };
    let new_lines = match read_until_dot(input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("drdred: c: read error: {e}");
            return;
        }
    };
    // Replace line n-1 with the collected lines. An empty replacement is
    // a delete; we allow it on purpose.
    state.lines.splice(n - 1..n, new_lines);
    state.modified = true;
}

fn cmd_delete(state: &mut State, rest: &str) {
    if rest.is_empty() {
        eprintln!("drdred: d: line number required");
        return;
    }
    let (start, end) = match parse_range(rest, state.lines.len()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("drdred: d: {e}");
            return;
        }
    };
    state.lines.drain(start - 1..end);
    state.modified = true;
}

fn cmd_set(state: &mut State, rest: &str) {
    // s N TEXT — split off the first whitespace-delimited token as N.
    let (n_str, text) = match rest.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b),
        None => {
            eprintln!("drdred: s: usage: s N TEXT");
            return;
        }
    };
    let n = match parse_existing_line(n_str, state.lines.len()) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("drdred: s: {e}");
            return;
        }
    };
    state.lines[n - 1] = text.to_string();
    state.modified = true;
}

fn cmd_write(state: &mut State, rest: &str) {
    let target: PathBuf = if rest.is_empty() {
        match &state.path {
            Some(p) => p.clone(),
            None => {
                eprintln!("drdred: w: no filename — use `w FILE`");
                return;
            }
        }
    } else {
        PathBuf::from(rest)
    };

    // Reconstruct the file: every line joined by '\n', plus a trailing
    // newline so the file is POSIX-tidy.
    let mut body = state.lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    match fs::write(&target, body.as_bytes()) {
        Ok(()) => {
            println!("wrote {} ({} lines, {} bytes)", target.display(), state.lines.len(), body.len());
            state.modified = false;
            // Adopt the new path so subsequent `w` (no arg) re-saves here.
            state.path = Some(target);
        }
        Err(e) => eprintln!("drdred: w: {}: {e}", target.display()),
    }
}

fn cmd_status(state: &State) {
    let path = state.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<no file>".into());
    println!("{} lines, {}, path: {}",
        state.lines.len(),
        if state.modified { "modified" } else { "saved" },
        path,
    );
}

fn cmd_help() {
    println!("DrDrEdit Tier 1 commands:");
    println!("  p [N|N,M]      print all / line / range");
    println!("  a N            append lines after line N (end with '.')");
    println!("  i N            insert lines before line N (end with '.')");
    println!("  c N            change (replace) line N with new lines");
    println!("  d N[,M]        delete line / range");
    println!("  s N TEXT       set line N to TEXT");
    println!("  w [FILE]       save to original path / FILE");
    println!("  q              quit (refuses if unsaved)");
    println!("  Q              quit, discarding unsaved changes");
    println!("  =              show line count, modified flag, path");
    println!("  h              this help");
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Read lines from `input` until a line containing exactly "." appears.
/// The terminator line itself is not included. EOF before "." treats the
/// collected lines as final input.
fn read_until_dot(input: &mut impl BufRead) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = input.read_line(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        let trimmed = buf.trim_end_matches('\n').to_string();
        if trimmed == "." {
            return Ok(out);
        }
        out.push(trimmed);
    }
}

/// Parse "p" range argument: "" → whole file, "N" → single line, "N,M" → range.
/// Returns 1-based inclusive (start, end), validated against `len`.
fn parse_range(rest: &str, len: usize) -> Result<(usize, usize), String> {
    if rest.is_empty() {
        if len == 0 {
            return Err("buffer is empty".into());
        }
        return Ok((1, len));
    }
    let (a, b) = match rest.split_once(',') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (rest, rest),
    };
    let start = parse_existing_line(a, len)?;
    let end = parse_existing_line(b, len)?;
    if start > end {
        return Err(format!("range out of order: {start} > {end}"));
    }
    Ok((start, end))
}

/// Parse a 1-based line number that refers to an existing line.
fn parse_existing_line(s: &str, len: usize) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|_| format!("not a line number: '{s}'"))?;
    if n == 0 || n > len {
        return Err(format!("line {n} out of range (1..={len})"));
    }
    Ok(n)
}

/// Parse a line number for `a`/`i` where the bounds are slightly different:
/// `a 0` is "insert at the very top", and `a N` past the end means "append
/// at the end", which we clamp.
fn parse_line_number(s: &str, len: usize, where_: InsertWhere) -> Result<usize, String> {
    if s.is_empty() {
        return Err("line number required".into());
    }
    let n: usize = s.parse().map_err(|_| format!("not a line number: '{s}'"))?;
    match where_ {
        InsertWhere::After => {
            if n > len {
                return Err(format!("line {n} out of range (0..={len})"));
            }
            Ok(n)
        }
        InsertWhere::Before => {
            if n == 0 || n > len.max(1) {
                return Err(format!("line {n} out of range (1..={})", len.max(1)));
            }
            Ok(n)
        }
    }
}

fn digit_count(n: usize) -> usize {
    if n == 0 { 1 } else { (n as f64).log10().floor() as usize + 1 }
}
