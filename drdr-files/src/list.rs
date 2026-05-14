//! Non-interactive (batch / pipeline-friendly) directory listing.
//!
//! This is the original Tier 1 behaviour, kept as the *default* invocation
//! so `drdr-files | grep foo` still works as a Unix tool. The interactive
//! browser is opt-in via `-i`.

use std::fs::{self, FileType, Metadata};
use std::io::{self, Write};
use std::path::Path;

struct Row {
    kind_tag: &'static str,
    size: String,
    name: String,
    sort_key: (u8, String),
}

pub fn list_dir(path: &Path, show_hidden: bool) -> io::Result<()> {
    let mut rows: Vec<Row> = Vec::new();

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

        let meta = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("drdr-files: {}: {}", entry.path().display(), e);
                continue;
            }
        };

        rows.push(make_row(&entry.path(), &name, &meta));
    }

    rows.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

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

/// Pretty bytes — "12", "12.4K", "3.1M", "2.0G". Public because the
/// interactive browser uses the same formatter.
pub fn human_size(bytes: u64) -> String {
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
