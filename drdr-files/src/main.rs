//! drdr-files — DrDrFiles, the DrDrOS file manager.
//!
//! Two modes:
//!
//!   - **Batch (default):** `drdr-files [-a] [PATH]` prints a typed
//!     directory listing. Pipeline-friendly — fine inside DrDrShell
//!     pipelines.
//!
//!   - **Interactive:** `drdr-files -i [-a] [PATH]` opens a full-screen
//!     TUI browser. Arrow keys navigate, Enter descends, Backspace
//!     ascends, `q` quits. Termios is restored automatically on every
//!     exit path — including panics — so a crash leaves a sane TTY.
//!
//! Tier 3 (Phase 3) replaces the termios-driven UI with a DrDrUI
//! window backed by the framebuffer. The directory model + key
//! dispatch in [`interactive`] stays intact across that transition.

mod interactive;
mod list;
mod tty;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

enum Mode {
    Batch,
    Interactive,
}

fn main() -> ExitCode {
    let mut mode = Mode::Batch;
    let mut show_hidden = false;
    let mut target: Option<PathBuf> = None;

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-a" | "--all" => show_hidden = true,
            "-i" | "--interactive" => mode = Mode::Interactive,
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("drdr-files: unknown flag '{other}' (try --help)");
                return ExitCode::from(2);
            }
            other => {
                if target.is_some() {
                    eprintln!("drdr-files: at most one PATH argument");
                    return ExitCode::from(2);
                }
                target = Some(PathBuf::from(other));
            }
        }
    }

    let dir = target.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let result = match mode {
        Mode::Batch => list::list_dir(&dir, show_hidden),
        Mode::Interactive => interactive::run(dir.clone(), show_hidden),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drdr-files: {}: {}", dir.display(), e);
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    println!("drdr-files — DrDrOS file manager");
    println!();
    println!("Usage: drdr-files [-i] [-a] [PATH]");
    println!();
    println!("  -i, --interactive   open the full-screen TUI browser");
    println!("  -a, --all           include entries whose names start with '.'");
    println!("  -h, --help          show this help");
    println!();
    println!("Default mode is batch: print a listing of PATH (or the current dir).");
    println!("Interactive keys: ↑/↓ move, Enter open, Backspace up, q quit.");
}
