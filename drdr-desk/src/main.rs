//! drdr-desk — DrDrDesk, the DrDrOS graphical session.
//!
//! This is the program a person actually lands in after the machine
//! boots. drdr-init (PID 1) launches and supervises it. It paints a
//! framebuffer desktop and runs a keyboard-driven launcher for the
//! other DrDr apps.
//!
//! Architecture (Tier 1 of the GUI):
//!   - We draw straight to `/dev/fb0` with drdr-fb + drdr-ui widgets,
//!     themed by the polished DrDrTheme.
//!   - Keyboard comes from `evdev` (`/dev/input/eventN`) via
//!     drdr-ui's `KeyReader`. We auto-detect the keyboard node so the
//!     user never has to pass `--kbd`.
//!   - Launching an app *spawns a child process and waits*. The DrDr
//!     apps (shell/files/edit) are terminal programs, so while one runs
//!     it owns the console; when it exits we redraw the desktop. This
//!     is a "session shell", not a window manager — real overlapping
//!     windows and a mouse are a later tier, and we don't pretend
//!     otherwise.
//!
//! Modes:
//!   drdr-desk                      # production: /dev/fb0 + auto kbd
//!   drdr-desk --kbd /dev/input/eventN [--fb /dev/fb0]
//!   drdr-desk --ppm out.ppm        # render one frame, no devices
//!
//! Keys: ↑/↓ or Tab move the selection, Enter opens it, those are all
//! a keyboard-only desktop needs.

use std::env;
use std::fs;
use std::process::{Command, ExitCode};

use drdr_fb::Framebuffer;
use drdr_font::{GLYPH_HEIGHT, GLYPH_WIDTH, draw_text};
use drdr_ui::{Button, Event, KeyCode, KeyReader, Rect, Theme, Widget};
use nix::sys::reboot::{RebootMode, reboot};

/// What a launcher row does when the user presses Enter.
#[derive(Clone, Copy)]
enum Action {
    /// Spawn a terminal app at this path and wait for it to exit.
    Run(&'static str),
    Reboot,
    PowerOff,
}

struct Entry {
    label: &'static str,
    action: Action,
}

const ENTRIES: &[Entry] = &[
    Entry { label: "DrDrShell", action: Action::Run("/bin/drdr-shell") },
    Entry { label: "DrDrFiles", action: Action::Run("/bin/drdr-files") },
    Entry { label: "DrDrEdit",  action: Action::Run("/bin/drdr-edit") },
    Entry { label: "Reboot",    action: Action::Reboot },
    Entry { label: "Power off", action: Action::PowerOff },
];

fn main() -> ExitCode {
    let args = match parse_args(env::args().collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("drdr-desk: {e}");
            return ExitCode::from(2);
        }
    };

    // Snapshot mode: render one frame to a heap framebuffer and dump a
    // PPM. No /dev access — used to eyeball the desktop on the host.
    if let Some(path) = &args.ppm_path {
        let mut fb = Framebuffer::in_memory(1024, 768);
        draw_desktop(&mut fb, &Theme::DRDR, args.initial_sel);
        return match fb.write_ppm(path) {
            Ok(()) => {
                eprintln!("drdr-desk: wrote {path} ({}x{})", fb.width, fb.height);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("drdr-desk: write_ppm {path}: {e}");
                ExitCode::from(1)
            }
        };
    }

    let mut fb = match Framebuffer::open(&args.fb_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("drdr-desk: open {}: {e}", args.fb_path);
            return ExitCode::from(1);
        }
    };

    // Auto-detect the keyboard if the caller didn't pin one.
    let kbd_path = match args.kbd_path.clone().or_else(detect_keyboard) {
        Some(p) => p,
        None => {
            eprintln!("drdr-desk: no keyboard found under /dev/input — pass --kbd");
            return ExitCode::from(1);
        }
    };
    let mut keys = match KeyReader::open(&kbd_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("drdr-desk: open {kbd_path}: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!("[drdr-desk] keyboard: {kbd_path}");

    let theme = Theme::DRDR;
    let mut sel = args.initial_sel.min(ENTRIES.len() - 1);

    loop {
        draw_desktop(&mut fb, &theme, sel);

        let event = match keys.next_event() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("drdr-desk: input error: {e}");
                return ExitCode::from(1);
            }
        };

        // Selection movement: ↓/Tab/j forward, ↑/Shift-Tab/k back.
        if matches!(event, Event::Key(KeyCode::Down | KeyCode::Tab | KeyCode::Char('j'))) {
            sel = (sel + 1) % ENTRIES.len();
            continue;
        }
        if matches!(event, Event::Key(KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k'))) {
            sel = (sel + ENTRIES.len() - 1) % ENTRIES.len();
            continue;
        }

        let activate = matches!(
            event,
            Event::Key(KeyCode::Enter | KeyCode::Space)
        );
        if !activate {
            continue;
        }

        match ENTRIES[sel].action {
            Action::Run(path) => {
                launch(&mut fb, &theme, path);
                // Loop continues → desktop is redrawn at the top.
            }
            Action::Reboot => {
                let _ = reboot(RebootMode::RB_AUTOBOOT);
            }
            Action::PowerOff => {
                // RB_POWER_OFF asks the kernel/ACPI to cut power; under
                // QEMU this exits the VM. On success reboot() never
                // returns, so the error arm only runs if we lack the
                // privilege (we shouldn't — we run from the initramfs).
                let _ = reboot(RebootMode::RB_POWER_OFF);
                eprintln!("drdr-desk: power off failed (need CAP_SYS_BOOT)");
            }
        }
    }
}

/// Spawn a terminal app and block until it exits. We hand the console
/// over to the child: clear the framebuffer first so its text doesn't
/// sit on top of stale desktop pixels, print a breadcrumb, then wait.
fn launch(fb: &mut Framebuffer, theme: &Theme, path: &str) {
    fb.clear(theme.bg);
    println!("[drdr-desk] launching {path} — exit it to return to the desktop");
    match Command::new(path).status() {
        Ok(st) => eprintln!("[drdr-desk] {path} exited ({st})"),
        Err(e) => eprintln!("[drdr-desk] could not run {path}: {e}"),
    }
}

// ─── Desktop rendering ───────────────────────────────────────────────

/// Paint the whole desktop for the current selection.
fn draw_desktop(fb: &mut Framebuffer, theme: &Theme, sel: usize) {
    let w = fb.width;
    let h = fb.height;

    fb.clear(theme.bg);

    // Top bar: a raised surface strip with the OS name and a hint.
    let bar_h = GLYPH_HEIGHT + 10;
    fb.fill_rect(0, 0, w, bar_h, theme.surface);
    fb.fill_rect(0, bar_h, w, 1, theme.border); // 1px divider
    let pad = 8;
    let bar_text_y = 5;
    draw_text(fb, pad, bar_text_y, "DrDrOS", theme.fg, theme.surface);
    let right = "graphical session";
    let right_w = GLYPH_WIDTH * right.len() as u32;
    draw_text(
        fb,
        w.saturating_sub(right_w + pad),
        bar_text_y,
        right,
        theme.muted,
        theme.surface,
    );

    // Big centred wordmark + tagline below the bar.
    let title = "DrDrOS";
    let title_w = GLYPH_WIDTH * title.len() as u32;
    draw_text(
        fb,
        w.saturating_sub(title_w) / 2,
        bar_h + 48,
        title,
        theme.fg,
        theme.bg,
    );
    let tag = "select an app and press Enter";
    let tag_w = GLYPH_WIDTH * tag.len() as u32;
    draw_text(
        fb,
        w.saturating_sub(tag_w) / 2,
        bar_h + 48 + GLYPH_HEIGHT + 6,
        tag,
        theme.muted,
        theme.bg,
    );

    // Launcher: a vertical stack of buttons, the selected one focused
    // so the polished theme paints its accent fill + focus ring.
    let gap = 12u32;
    let mut bw = 0u32;
    let mut bh = 0u32;
    for e in ENTRIES {
        let b = Button::new(e.label);
        let (pw, ph) = b.preferred_size();
        bw = bw.max(pw + 48);
        bh = ph.max(bh);
    }
    let n = ENTRIES.len() as u32;
    let total_h = bh * n + gap * (n - 1);
    let start_y = (h.saturating_sub(total_h)) / 2 + 24;
    let bx = w.saturating_sub(bw) / 2;
    let mut y = start_y;
    for (i, e) in ENTRIES.iter().enumerate() {
        let mut b = Button::new(e.label);
        b.focused = i == sel;
        b.draw(fb, Rect::new(bx, y, bw, bh), theme);
        y = y.saturating_add(bh).saturating_add(gap);
    }

    // Footer hint.
    let foot = "Up/Down move   Enter open   (auto-respawns)";
    let foot_w = GLYPH_WIDTH * foot.len() as u32;
    draw_text(
        fb,
        w.saturating_sub(foot_w) / 2,
        h.saturating_sub(GLYPH_HEIGHT + 8),
        foot,
        theme.muted,
        theme.bg,
    );
}

// ─── Keyboard auto-detection ─────────────────────────────────────────

/// Find the keyboard's `/dev/input/eventN` by parsing the kernel's
/// `/proc/bus/input/devices` table. Each device is a block of lines;
/// the one whose `H: Handlers=` line lists `kbd` is the keyboard, and
/// the same line names its `eventN` node. Falls back to scanning for
/// the first existing `event*` node if the table is unavailable.
fn detect_keyboard() -> Option<String> {
    if let Ok(table) = fs::read_to_string("/proc/bus/input/devices") {
        for line in table.lines() {
            let Some(handlers) = line.strip_prefix("H: Handlers=") else {
                continue;
            };
            let toks: Vec<&str> = handlers.split_whitespace().collect();
            if toks.iter().any(|t| *t == "kbd") {
                if let Some(ev) = toks.iter().find(|t| t.starts_with("event")) {
                    return Some(format!("/dev/input/{ev}"));
                }
            }
        }
    }
    // Last resort: first event node that exists.
    for n in 0..16 {
        let p = format!("/dev/input/event{n}");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    None
}

// ─── Argv ────────────────────────────────────────────────────────────

struct Args {
    fb_path: String,
    kbd_path: Option<String>,
    ppm_path: Option<String>,
    initial_sel: usize,
}

fn parse_args(mut argv: Vec<String>) -> Result<Args, String> {
    let _ = argv.drain(..1);
    let mut a = Args {
        fb_path: "/dev/fb0".into(),
        kbd_path: None,
        ppm_path: None,
        initial_sel: 0,
    };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--fb" => {
                a.fb_path = argv.get(i + 1).cloned().ok_or("--fb needs a path")?;
                i += 2;
            }
            "--kbd" => {
                a.kbd_path = Some(argv.get(i + 1).cloned().ok_or("--kbd needs a path")?);
                i += 2;
            }
            "--ppm" => {
                a.ppm_path = Some(argv.get(i + 1).cloned().ok_or("--ppm needs a path")?);
                i += 2;
            }
            "--sel" => {
                let raw = argv.get(i + 1).ok_or("--sel needs a number")?;
                a.initial_sel = raw.parse().map_err(|_| format!("--sel: not a number: {raw}"))?;
                i += 2;
            }
            "-h" | "--help" => {
                println!("drdr-desk — DrDrOS graphical session");
                println!("  drdr-desk [--fb /dev/fb0] [--kbd /dev/input/eventN]");
                println!("  drdr-desk --ppm out.ppm [--sel N]   # host snapshot");
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg '{other}' (try --help)")),
        }
    }
    Ok(a)
}
