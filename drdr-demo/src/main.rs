//! drdr-demo — exercise the Phase 3 stack three ways.
//!
//! 1. Real framebuffer + evdev keyboard (the production case):
//!
//!        drdr-demo --kbd /dev/input/event3
//!        drdr-demo --kbd /dev/input/event3 --fb /dev/fb0
//!
//! 2. Render-once snapshot to a PPM file (no device access — runs on
//!    any host with cargo):
//!
//!        drdr-demo --ppm out.ppm
//!        drdr-demo --ppm out.ppm --focus 2
//!
//!    The PPM can be opened in GIMP / Eye of GNOME / any browser with a
//!    PPM extension. Handy for visually diff'ing widget layouts without
//!    booting QEMU.
//!
//! Keys (interactive mode):
//!     Tab / ↓ / j   focus next button
//!     Shift+Tab / ↑ / k   focus previous
//!     Enter / Space   activate the focused button
//!     Esc / q        quit
//!
//! "Activating" a button just logs its label to stderr — wiring the
//! buttons up to actually launch drdr-files / drdr-edit / drdr-shell
//! lands when we're booting inside DrDrOS proper.

use std::env;
use std::process::ExitCode;

use drdr_fb::{Framebuffer, Pixel};
use drdr_ui::{
    input::EventResponse, Button, Event, KeyCode, KeyReader, Label, Rect, Theme, Widget,
};

fn main() -> ExitCode {
    let args = match parse_args(env::args().collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("drdr-demo: {e}");
            return ExitCode::from(2);
        }
    };

    // PPM snapshot mode — no framebuffer device, no keyboard. Renders
    // one frame at a fixed resolution into a heap-backed Framebuffer.
    if let Some(path) = &args.ppm_path {
        return run_ppm_snapshot(path, args.initial_focus);
    }

    let kbd_path = match &args.kbd_path {
        Some(p) => p.clone(),
        None => {
            eprintln!("drdr-demo: --kbd is required for interactive mode (or pass --ppm FILE)");
            return ExitCode::from(2);
        }
    };

    let mut fb = match Framebuffer::open(&args.fb_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("drdr-demo: open {}: {e}", args.fb_path);
            return ExitCode::from(1);
        }
    };

    let mut keys = match KeyReader::open(&kbd_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("drdr-demo: open {kbd_path}: {e}");
            return ExitCode::from(1);
        }
    };

    let theme = Theme::DRDR;
    let mut buttons = vec![
        Button::new("Files"),
        Button::new("Edit"),
        Button::new("Shell"),
        Button::new("Quit"),
    ];
    let title = Label::new("DrDrOS demo — Tab moves focus, Enter activates, Esc quits");
    let mut focus = args.initial_focus.min(buttons.len() - 1);
    buttons[focus].focused = true;

    loop {
        draw_frame(&mut fb, &theme, &title, &buttons);

        let event = match keys.next_event() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("drdr-demo: input error: {e}");
                return ExitCode::from(1);
            }
        };

        // Quit keys are handled at the framework level — they always win.
        if matches!(event, Event::Key(KeyCode::Escape) | Event::Key(KeyCode::Char('q'))) {
            return ExitCode::SUCCESS;
        }

        // Focus traversal.
        if matches!(event, Event::Key(KeyCode::Tab | KeyCode::Down | KeyCode::Char('j'))) {
            move_focus(&mut buttons, &mut focus, 1);
            continue;
        }
        if matches!(event, Event::Key(KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k'))) {
            move_focus(&mut buttons, &mut focus, -1);
            continue;
        }

        // Hand the event to the focused button.
        if let Some(btn) = buttons.get_mut(focus) {
            if btn.handle_event(&event) == EventResponse::Consumed {
                // Some button consumed the event — check whether it was a click.
                if btn.take_click() {
                    let label = btn.text.clone();
                    eprintln!("drdr-demo: clicked '{label}'");
                    if label == "Quit" {
                        return ExitCode::SUCCESS;
                    }
                }
            }
        }
    }
}

fn move_focus(buttons: &mut [Button], focus: &mut usize, delta: isize) {
    if buttons.is_empty() {
        return;
    }
    buttons[*focus].focused = false;
    let n = buttons.len() as isize;
    let next = (*focus as isize + delta).rem_euclid(n);
    *focus = next as usize;
    buttons[*focus].focused = true;
}

fn draw_frame(fb: &mut Framebuffer, theme: &Theme, title: &Label, buttons: &[Button]) {
    let w = fb.width;
    let h = fb.height;

    // Clear to bg.
    fb.fill_rect(0, 0, w, h, theme.bg);

    // Title centred near the top.
    let (tw, th) = title.preferred_size();
    let title_rect = Rect::new(w.saturating_sub(tw) / 2, 24, tw, th);
    title.draw(fb, title_rect, theme);

    // Vertical stack of buttons, centred.
    let gap = 12u32;
    let (bw, bh) = buttons
        .iter()
        .map(|b| b.preferred_size())
        .fold((0u32, 0u32), |(mw, _mh), (pw, ph)| (mw.max(pw + 32), ph));

    let total_h = bh * buttons.len() as u32 + gap * (buttons.len().saturating_sub(1) as u32);
    let mut y = (h.saturating_sub(total_h)) / 2;
    for btn in buttons {
        let rect = Rect::new(w.saturating_sub(bw) / 2, y, bw, bh);
        btn.draw(fb, rect, theme);
        y = y.saturating_add(bh).saturating_add(gap);
    }
}

/// Render one frame of the menu into a heap-backed Framebuffer and
/// dump it to `path` as PPM. Returns an ExitCode for main.
fn run_ppm_snapshot(path: &str, initial_focus: usize) -> ExitCode {
    let mut fb = Framebuffer::in_memory(1024, 768);
    let theme = Theme::DRDR;
    let mut buttons = vec![
        Button::new("Files"),
        Button::new("Edit"),
        Button::new("Shell"),
        Button::new("Quit"),
    ];
    let focus = initial_focus.min(buttons.len() - 1);
    buttons[focus].focused = true;
    let title = Label::new("DrDrOS — Phase 3 widget snapshot");

    draw_frame(&mut fb, &theme, &title, &buttons);

    match fb.write_ppm(path) {
        Ok(()) => {
            eprintln!("drdr-demo: wrote {path} ({}x{} pixels)", fb.width, fb.height);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("drdr-demo: write_ppm {path}: {e}");
            ExitCode::from(1)
        }
    }
}

// ─── Argv ────────────────────────────────────────────────────────────

struct Args {
    fb_path: String,
    kbd_path: Option<String>,
    ppm_path: Option<String>,
    initial_focus: usize,
}

fn parse_args(mut argv: Vec<String>) -> Result<Args, String> {
    let _ = argv.drain(..1);
    let mut fb_path = String::from("/dev/fb0");
    let mut kbd_path: Option<String> = None;
    let mut ppm_path: Option<String> = None;
    let mut initial_focus: usize = 0;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--fb" => {
                fb_path = argv.get(i + 1).cloned()
                    .ok_or_else(|| "--fb needs a path".to_string())?;
                i += 2;
            }
            "--kbd" => {
                kbd_path = Some(argv.get(i + 1).cloned()
                    .ok_or_else(|| "--kbd needs a path".to_string())?);
                i += 2;
            }
            "--ppm" => {
                ppm_path = Some(argv.get(i + 1).cloned()
                    .ok_or_else(|| "--ppm needs a path".to_string())?);
                i += 2;
            }
            "--focus" => {
                let raw = argv.get(i + 1)
                    .ok_or_else(|| "--focus needs a number".to_string())?;
                initial_focus = raw.parse()
                    .map_err(|_| format!("--focus: not a number: '{raw}'"))?;
                i += 2;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg '{other}' (try --help)")),
        }
    }

    Ok(Args { fb_path, kbd_path, ppm_path, initial_focus })
}

fn print_help() {
    println!("drdr-demo — DrDrUI Tier 2 showcase");
    println!();
    println!("Interactive (real framebuffer + keyboard):");
    println!("  drdr-demo --kbd /dev/input/eventN [--fb /dev/fb0]");
    println!();
    println!("Snapshot (no device, writes a PPM image — works on any host):");
    println!("  drdr-demo --ppm OUT.ppm [--focus N]");
    println!();
    println!("Interactive keys:");
    println!("  Tab / ↓ / j         focus next");
    println!("  Shift+Tab / ↑ / k   focus previous");
    println!("  Enter / Space       activate the focused button");
    println!("  Esc / q             quit");
    println!();
    println!("/dev/fb0 needs the `video` group; /dev/input/eventN needs `input`.");
}

// keep `Pixel` import warning-free even though we don't reference it directly.
#[allow(dead_code)]
const _: Pixel = Pixel::BLACK;
