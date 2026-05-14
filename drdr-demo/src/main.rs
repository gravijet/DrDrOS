//! drdr-demo — exercise the Phase 3 stack against a real framebuffer
//! and keyboard. Opens `/dev/fb0`, opens the evdev device the user passes,
//! draws a focusable menu, and reacts to key presses.
//!
//! Usage:
//!
//!     drdr-demo --kbd /dev/input/event3
//!     drdr-demo --kbd /dev/input/event3 --fb /dev/fb0
//!
//! Keys:
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

    let mut fb = match Framebuffer::open(&args.fb_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("drdr-demo: open {}: {e}", args.fb_path);
            return ExitCode::from(1);
        }
    };

    let mut keys = match KeyReader::open(&args.kbd_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("drdr-demo: open {}: {e}", args.kbd_path);
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
    let mut focus = 0usize;
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

// ─── Argv ────────────────────────────────────────────────────────────

struct Args {
    fb_path: String,
    kbd_path: String,
}

fn parse_args(mut argv: Vec<String>) -> Result<Args, String> {
    let _ = argv.drain(..1);
    let mut fb_path = String::from("/dev/fb0");
    let mut kbd_path: Option<String> = None;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--fb" => {
                fb_path = argv
                    .get(i + 1)
                    .cloned()
                    .ok_or_else(|| "--fb needs a path".to_string())?;
                i += 2;
            }
            "--kbd" => {
                kbd_path = Some(
                    argv.get(i + 1)
                        .cloned()
                        .ok_or_else(|| "--kbd needs a path".to_string())?,
                );
                i += 2;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg '{other}' (try --help)")),
        }
    }

    let kbd_path = kbd_path.ok_or_else(|| "--kbd /dev/input/eventN is required".to_string())?;
    Ok(Args { fb_path, kbd_path })
}

fn print_help() {
    println!("drdr-demo — DrDrUI Tier 2 showcase");
    println!();
    println!("Usage: drdr-demo --kbd /dev/input/eventN [--fb /dev/fb0]");
    println!();
    println!("Keys: Tab/↓/j next · Shift+Tab/↑/k previous · Enter/Space activate · Esc/q quit");
    println!();
    println!("/dev/fb0 needs the `video` group; /dev/input/eventN needs `input`.");
}

// keep `Pixel` import warning-free even though we don't reference it directly.
#[allow(dead_code)]
const _: Pixel = Pixel::BLACK;
