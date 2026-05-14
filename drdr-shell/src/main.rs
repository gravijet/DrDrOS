//! drdr-shell — DrDrShell, the DrDrOS interactive command shell (Tier 2).
//!
//! Tier 2 adds a real parser on top of Tier 1's REPL:
//!
//!   - Quoting: `"foo bar"`, `'foo bar'`, `\"` and `\\` escapes inside `"..."`.
//!   - Pipes: `a | b | c` — stdout of each stage feeds stdin of the next.
//!   - Redirects: `< in`, `> out`, `>> out`, `2> err`, `2>> err`.
//!
//! The REPL loop, prompt rendering, and built-ins are unchanged from Tier 1.
//! What's new is that we tokenise + parse the line into a [`Pipeline`]
//! first; built-ins are only dispatched when the pipeline is a single
//! stage with no redirects (everything else fork+execs).
//!
//! What's deliberately not here yet
//! ─────────────────────────────────
//! Command history (needs termios raw mode + scrollback), variables
//! (`$HOME`, `$?`, `export`), globs, command substitution. Those are
//! Tier 3 territory.

mod pipeline;

use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use pipeline::{parse, tokenize};

fn main() {
    let mut last_status: i32 = 0;

    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut stdout = io::stdout();
    let mut line = String::new();

    loop {
        render_prompt(&mut stdout, last_status);

        line.clear();
        match handle.read_line(&mut line) {
            Ok(0) => {
                // EOF — leave cleanly.
                println!();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("drdrsh: read error: {e}");
                return;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Tokenise. Quoting errors are reported and the prompt loops.
        let tokens = match tokenize(trimmed) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("drdrsh: parse error: {e}");
                last_status = 2;
                continue;
            }
        };

        // Parse into a Pipeline. Same handling for errors.
        let pipe = match parse(tokens) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("drdrsh: parse error: {e}");
                last_status = 2;
                continue;
            }
        };
        if pipe.stages.is_empty() {
            continue;
        }

        // Built-in shortcut: single stage, no redirects, recognised verb.
        if pipe.is_simple() {
            let stage = &pipe.stages[0];
            if let Some(result) = try_builtin(&stage.program, &stage.args) {
                match result {
                    Builtin::Exit(code) => std::process::exit(code),
                    Builtin::Status(s) => {
                        last_status = s;
                        continue;
                    }
                }
            }
        }

        // Real pipeline: spawn it, wait, take the final exit code.
        last_status = match pipe.execute() {
            Ok(code) => code,
            Err(e) => {
                eprintln!("drdrsh: {e}");
                127
            }
        };
    }
}

fn render_prompt(stdout: &mut impl Write, last_status: i32) {
    let cwd_label = current_dir_label();
    if last_status == 0 {
        let _ = write!(stdout, "{cwd_label} drdrsh> ");
    } else {
        let _ = write!(stdout, "{cwd_label} drdrsh [{last_status}]> ");
    }
    let _ = stdout.flush();
}

fn current_dir_label() -> String {
    env::current_dir()
        .map(|p| compact_path(&p))
        .unwrap_or_else(|_| "?".into())
}

fn compact_path(p: &PathBuf) -> String {
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rest) = p.strip_prefix(&home) {
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

// ─── Built-ins ───────────────────────────────────────────────────────

enum Builtin {
    Exit(i32),
    Status(i32),
}

fn try_builtin(cmd: &str, args: &[String]) -> Option<Builtin> {
    match cmd {
        "exit" => Some(builtin_exit(args)),
        "cd" => Some(Builtin::Status(builtin_cd(args))),
        "pwd" => Some(Builtin::Status(builtin_pwd())),
        "echo" => Some(Builtin::Status(builtin_echo(args))),
        "help" => Some(Builtin::Status(builtin_help())),
        _ => None,
    }
}

fn builtin_exit(args: &[String]) -> Builtin {
    match args.len() {
        0 => Builtin::Exit(0),
        1 => match args[0].parse::<i32>() {
            Ok(n) => Builtin::Exit(n),
            Err(_) => {
                eprintln!("exit: numeric argument required");
                Builtin::Exit(2)
            }
        },
        _ => {
            eprintln!("exit: too many arguments");
            Builtin::Status(1)
        }
    }
}

fn builtin_cd(args: &[String]) -> i32 {
    let target: PathBuf = match args.len() {
        0 => match env::var_os("HOME") {
            Some(h) => h.into(),
            None => {
                eprintln!("cd: HOME not set");
                return 1;
            }
        },
        1 => PathBuf::from(&args[0]),
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

fn builtin_echo(args: &[String]) -> i32 {
    let mut out = io::stdout().lock();
    let mut first = true;
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

fn builtin_help() -> i32 {
    println!("DrDrShell built-ins:");
    println!("  cd [dir]      change directory (no arg = $HOME)");
    println!("  pwd           print working directory");
    println!("  echo args...  print args separated by spaces");
    println!("  exit [code]   leave the shell (default code 0)");
    println!("  help          show this list");
    println!();
    println!("Pipes: a | b | c    Redirects: < in, > out, >> out, 2> err, 2>> err");
    println!("Quoting: \"...\" and '...' (single = literal, double = supports \\\\ \\\")");
    println!("Anything else is looked up on $PATH and run as a child process.");
    0
}
