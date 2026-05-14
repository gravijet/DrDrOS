//! drdr-shell — DrDrShell, the DrDrOS interactive command shell (Tier 1).
//!
//! A small read-eval-print loop:
//!
//!   prompt → read line → tokenise → built-in or spawn → print → loop
//!
//! What Tier 1 covers
//! ──────────────────
//! - Prompt that shows the last command's exit status when it was non-zero.
//! - Whitespace-split tokenisation. No quotes, no escapes, no globs yet —
//!   those land in Tier 2 alongside pipes and redirects.
//! - Built-ins: `exit`, `cd`, `pwd`, `echo`, `help`.
//! - External commands resolved via PATH (`std::process::Command` does this
//!   for us — same logic as `execvp(3)` underneath).
//! - EOF (Ctrl-D on Unix) leaves the loop cleanly with exit code 0.
//! - Empty lines are ignored, so hitting Enter at the prompt is harmless.
//!
//! What's deliberately *not* here yet
//! ──────────────────────────────────
//! Pipes, redirects, command-line history, signal handling, job control,
//! variables, and globbing. Those are Tier 2. Phase 3 will swap stdin for
//! a framebuffer-aware input layer that reads from the kernel's evdev
//! keyboard device — but the parse/dispatch logic here will stay.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

fn main() {
    let mut last_status: i32 = 0;

    // `stdin().lock()` returns a handle to the process's stdin with a
    // built-in buffer (one allocation, reused across lines). Same idea
    // as Node's `readline` interface, but synchronous and one-shot per
    // line — we explicitly call `read_line` ourselves.
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut stdout = io::stdout();

    let mut line = String::new();

    loop {
        // Render the prompt. Showing the last non-zero exit status gives
        // a quick "did that command succeed?" cue without typing `echo $?`.
        let cwd_label = current_dir_label();
        if last_status == 0 {
            let _ = write!(stdout, "{cwd_label} drdrsh> ");
        } else {
            let _ = write!(stdout, "{cwd_label} drdrsh [{last_status}]> ");
        }
        // Flush so the prompt actually appears — stdout is line-buffered
        // when attached to a terminal but we still want to be explicit.
        let _ = stdout.flush();

        line.clear();
        match handle.read_line(&mut line) {
            // 0 bytes read = EOF (Ctrl-D pressed). Exit cleanly.
            Ok(0) => {
                println!();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("drdrsh: read error: {e}");
                return;
            }
        }

        // `read_line` includes the trailing '\n'; trim it plus any other
        // whitespace the user typed. `trim()` returns a &str borrow into
        // `line`, which lives long enough for the rest of the iteration.
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Tokenise on whitespace. `split_whitespace` collapses runs of
        // spaces/tabs into single separators, exactly like sh does for
        // unquoted input.
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        let (cmd, args) = tokens.split_first().expect("non-empty (checked above)");

        // Built-ins are dispatched here. Each returns the exit status it
        // wants reflected in the prompt; `None` from `dispatch_builtin`
        // means "not a built-in, try to spawn it".
        last_status = match dispatch_builtin(cmd, args) {
            Some(Builtin::Exit(code)) => std::process::exit(code),
            Some(Builtin::Status(s)) => s,
            None => spawn_external(cmd, args),
        };
    }
}

/// Friendly current-directory label for the prompt. Falls back to `?` if
/// the cwd has somehow gone away (deleted directory, permission flip).
fn current_dir_label() -> String {
    env::current_dir()
        .map(|p| compact_path(&p))
        .unwrap_or_else(|_| "?".into())
}

/// Replace a leading `$HOME` with `~` so the prompt stays short, matching
/// what every other shell does. Anything else is returned verbatim.
fn compact_path(p: &PathBuf) -> String {
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rest) = p.strip_prefix(&home) {
            // Path::join handles empty `rest` correctly: ~/.
            let mut s = String::from("~");
            if !rest.as_os_str().is_empty() {
                s.push('/');
                s.push_str(&rest.to_string_lossy());
            }
            return s;
        }
    }
    p.to_string_lossy().into_owned()
}

/// What a built-in handler wants us to do next.
enum Builtin {
    /// Leave the shell with this exit code.
    Exit(i32),
    /// Continue the loop, recording this exit status.
    Status(i32),
}

/// Try to handle `cmd` as a built-in. Returns `None` if it isn't one —
/// the caller then tries to spawn an external program with the same name.
fn dispatch_builtin(cmd: &str, args: &[&str]) -> Option<Builtin> {
    match cmd {
        "exit" => Some(builtin_exit(args)),
        "cd" => Some(Builtin::Status(builtin_cd(args))),
        "pwd" => Some(Builtin::Status(builtin_pwd())),
        "echo" => Some(Builtin::Status(builtin_echo(args))),
        "help" => Some(Builtin::Status(builtin_help())),
        _ => None,
    }
}

/// `exit [code]` — leaves the shell. Default exit code 0; a single integer
/// argument overrides it. Garbage arguments produce exit code 2 (POSIX
/// convention for misuse).
fn builtin_exit(args: &[&str]) -> Builtin {
    match args {
        [] => Builtin::Exit(0),
        [code] => match code.parse::<i32>() {
            Ok(n) => Builtin::Exit(n),
            Err(_) => {
                eprintln!("exit: numeric argument required");
                Builtin::Exit(2)
            }
        },
        _ => {
            eprintln!("exit: too many arguments");
            Builtin::Status(1) // stay in the shell — sh does the same.
        }
    }
}

/// `cd [dir]` — change current directory. No arg → $HOME. `-` would mean
/// "previous directory" in bash; we'll add that in Tier 2 once we track
/// state between commands.
fn builtin_cd(args: &[&str]) -> i32 {
    let target: PathBuf = match args {
        [] => match env::var_os("HOME") {
            Some(h) => h.into(),
            None => {
                eprintln!("cd: HOME not set");
                return 1;
            }
        },
        [path] => PathBuf::from(path),
        _ => {
            eprintln!("cd: too many arguments");
            return 1;
        }
    };
    match env::set_current_dir(&target) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cd: {}: {}", target.display(), e);
            1
        }
    }
}

/// `pwd` — print the current working directory.
fn builtin_pwd() -> i32 {
    match env::current_dir() {
        Ok(p) => {
            println!("{}", p.display());
            0
        }
        Err(e) => {
            eprintln!("pwd: {e}");
            1
        }
    }
}

/// `echo args...` — print args separated by single spaces, then newline.
/// No flags yet (no `-n`, no escape interpretation).
fn builtin_echo(args: &[&str]) -> i32 {
    let mut first = true;
    let mut out = io::stdout().lock();
    for a in args {
        if !first {
            let _ = out.write_all(b" ");
        }
        let _ = out.write_all(a.as_bytes());
        first = false;
    }
    let _ = out.write_all(b"\n");
    0
}

/// `help` — list the built-ins so a new user can find their footing.
fn builtin_help() -> i32 {
    println!("DrDrShell built-ins:");
    println!("  cd [dir]      change directory (no arg = $HOME)");
    println!("  pwd           print working directory");
    println!("  echo args...  print args separated by spaces");
    println!("  exit [code]   leave the shell (default code 0)");
    println!("  help          show this list");
    println!();
    println!("Anything else is looked up on $PATH and run as a child process.");
    0
}

/// Spawn `cmd` with `args` as a child process, wait for it to finish,
/// and return its exit code (or 127 for "not found", matching sh).
fn spawn_external(cmd: &str, args: &[&str]) -> i32 {
    match Command::new(cmd).args(args).status() {
        Ok(status) => status_to_code(status),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("drdrsh: {cmd}: command not found");
            127
        }
        Err(e) => {
            eprintln!("drdrsh: {cmd}: {e}");
            1
        }
    }
}

/// Collapse a Unix ExitStatus into a single integer for prompt display.
/// If the child was killed by a signal, we fold the signal number into
/// 128+N — the same encoding bash and dash use.
fn status_to_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        code
    } else {
        // `code()` is None when the process was terminated by a signal.
        // Unix-only `ExitStatusExt::signal()` gives us the signal number.
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    }
}
