//! drdr-files — DrDrFiles, the DrDrOS file manager (Tier 1).
//!
//! Tier 1 is a non-interactive directory lister: print one entry per line
//! with a type tag, human-readable size, and the entry name. Directories
//! get a trailing `/`; symlinks get ` -> target`. Sorting puts directories
//! first, then everything else alphabetically — so the eye lands on the
//! navigable rows first.
//!
//! Tier 2 (Phase 2 finale): turn this into an interactive browser using
//! termios raw mode + arrow keys, with copy / move / delete actions.
//! Tier 3 (Phase 3): replace the terminal frontend with a DrDrUI window.
//!
//! Usage:
//!
//!     drdr-files [-a] [PATH]
//!
//!     -a    include hidden entries (names starting with '.')
//!     PATH  directory to list (defaults to the current working directory)

use std::env;
use std::fs::{self, FileType, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut show_hidden = false;
    let mut target: Option<PathBuf> = None;

    // Hand-rolled argv parsing. No clap dependency — Tier 1 has exactly
    // one flag and one positional, so a clap setup would be all overhead.
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-a" | "--all" => show_hidden = true,
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

    match list_dir(&dir, show_hidden) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drdr-files: {}: {}", dir.display(), e);
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    println!("drdr-files — DrDrOS directory lister (Tier 1)");
    println!();
    println!("Usage: drdr-files [-a] [PATH]");
    println!();
    println!("  -a, --all    include entries whose names start with '.'");
    println!("  -h, --help   show this help");
    println!();
    println!("PATH defaults to the current working directory.");
}

/// One row of output, pre-formatted for stable sorting.
struct Row {
    /// Two-character type tag: "DIR", "LNK", or "  F".
    kind_tag: &'static str,
    /// Human-readable size, or "-" for directories / symlinks.
    size: String,
    /// Display name — directories carry a trailing '/', symlinks include
    /// ` -> target`. The original entry name is in `sort_key`.
    name: String,
    /// Sort key. We want directories first, then alphabetically.
    sort_key: (u8, String),
}

fn list_dir(path: &Path, show_hidden: bool) -> io::Result<()> {
    let mut rows: Vec<Row> = Vec::new();

    // `read_dir` returns an iterator of Result<DirEntry>. The outer ?
    // bubbles up "can't open directory at all" failures. Per-entry
    // failures (a file that has permissions but no readable metadata)
    // we surface inline and keep going.
    for entry in fs::read_dir(path)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("drdr-files: read entry: {e}");
                continue;
            }
        };

        let name_os = entry.file_name();
        let name = name_os.to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        // `symlink_metadata` doesn't follow links — we want the link's own
        // type, not the target's. `metadata` would dereference and hide
        // dangling symlinks behind an error.
        let meta = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("drdr-files: {}: {}", entry.path().display(), e);
                continue;
            }
        };

        rows.push(make_row(&entry.path(), &name, &meta));
    }

    // Stable sort by (kind, name) — sort_key encodes that directly so the
    // sort is deterministic without a custom comparator.
    rows.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

    // Buffer the output through a single `BufWriter` so each line doesn't
    // round-trip to the kernel via a separate write() syscall.
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    writeln!(out, "drdr-files: {}", path.display())?;
    writeln!(out)?;
    if rows.is_empty() {
        writeln!(out, "  (empty)")?;
    } else {
        for row in &rows {
            writeln!(out, "  {} {:>7}  {}", row.kind_tag, row.size, row.name)?;
        }
    }
    Ok(())
}

fn make_row(path: &Path, name: &str, meta: &Metadata) -> Row {
    let ft: FileType = meta.file_type();
    let (kind_tag, sort_class) = if ft.is_dir() {
        ("DIR", 0u8)
    } else if ft.is_symlink() {
        ("LNK", 1)
    } else {
        ("  F", 2)
    };

    let size = if ft.is_dir() || ft.is_symlink() {
        "-".to_string()
    } else {
        human_size(meta.len())
    };

    let mut display = name.to_string();
    if ft.is_dir() {
        display.push('/');
    } else if ft.is_symlink() {
        if let Ok(target) = fs::read_link(path) {
            display.push_str(" -> ");
            display.push_str(&target.to_string_lossy());
        }
    }

    Row {
        kind_tag,
        size,
        name: display,
        sort_key: (sort_class, name.to_string()),
    }
}

/// Format `bytes` as one of "12", "12.4K", "3.1M", "2.0G". We stop at GiB
/// because anything bigger on an initramfs would be a bug, not a file.
fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes < KIB {
        format!("{bytes}")
    } else if bytes < MIB {
        format!("{:.1}K", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    }
}
