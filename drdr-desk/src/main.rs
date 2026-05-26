//! drdr-desk — DrDrDesk, the DrDrOS graphical session (Tier 2).
//!
//! This is where a person lands after the machine boots: drdr-init
//! (PID 1) launches and supervises it. Tier 1 was a keyboard-only
//! launcher that handed the whole console to one app at a time. Tier 2
//! is a real **window manager**:
//!
//!   - Mouse + keyboard, multiplexed through DrDrUI's
//!     [`InputHub`](drdr_ui::InputHub) (poll over both evdev nodes,
//!     auto-detected). A hand-drawn cursor.
//!   - Overlapping windows with title bars: drag to move, Alt-Tab to
//!     cycle focus, click the `[x]` box to close. A simple stacking
//!     model — paint back-to-front, top window wins. No compositor.
//!   - DrDr apps run *inside* windows via the [`WindowApp`] +
//!     [`TextGrid`] surface (see `drdr-ui/src/window.rs`) — no console
//!     hand-off, no terminal emulator.
//!   - One window, "DrDrNet", is a live client of DrDrNet's Tier 3
//!     async reactor, which we run in a background thread. The custom
//!     protocol is exercised by an actual app, not just unit tests.
//!
//! Modes:
//!   drdr-desk                       # production: /dev/fb0 + auto in/out
//!   drdr-desk --kbd /dev/input/eventN --mouse /dev/input/eventM
//!   drdr-desk --ppm out.ppm         # render one frame, no devices
//!
//! Keys: Alt-Tab cycles windows; the focused window's app gets the rest.

mod apps;
mod net;

use std::env;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use drdr_fb::Framebuffer;
use drdr_font::{GLYPH_HEIGHT, GLYPH_WIDTH, draw_text};
use drdr_ui::{
    HubEvent, InputHub, KeyReader, PointerReader, Px, Rect, Spawn, Theme, VtGuard,
    WindowManager, detect_all_keyboards, detect_all_mice, detect_all_touch,
};

use apps::LauncherApp;
use net::NetState;

/// Shared, late-bound DrDrNet state. Networking can take a while to
/// stand up on real hardware (UDP discovery bind, broadcast setup,
/// reactor port pickup) — too long to keep the user staring at the
/// splash. We start it in a background thread and write the resulting
/// `NetState` into this `Arc<Mutex<…>>` so apps that need it can read
/// it any time, gracefully showing "offline" until the value lands.
pub type SharedNet = Arc<Mutex<Option<NetState>>>;

fn main() -> ExitCode {
    let args = match parse_args(env::args().collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("drdr-desk: {e}");
            return ExitCode::from(2);
        }
    };

    // The active palette is process-global so the Settings window can
    // flip light/dark at runtime; we re-read it before every repaint.
    let mut theme = apps::current_theme();

    // ── A shared, late-bound DrDrNet handle. Networking goes up in a
    // background thread; apps read it whenever they need to. Until
    // it lands the DrDrNet/Chat panels show "offline" — that is the
    // graceful-degradation contract the desktop already had, just
    // without blocking boot if a UDP bind / broadcast is slow on
    // real hardware. THE single biggest source of the "stuck on
    // splash" report was doing this work BEFORE the first present(),
    // so it now happens AFTER the desktop is already on screen.
    let shared_net: SharedNet = Arc::new(Mutex::new(None));

    // Snapshot mode: one frame to a heap framebuffer → PPM. No devices.
    // We start net synchronously here so the panel shows real data in
    // the image; in real boots we defer it (see below).
    if let Some(path) = &args.ppm_path {
        let net_state = NetState::start(hostname()).ok();
        if let Some(s) = &net_state {
            *shared_net.lock().unwrap() = Some(s.clone());
        }
        let mut fb = Framebuffer::in_memory(1024, 768);
        let mut wm = WindowManager::new(fb.width, fb.height);
        build_desktop(&mut wm, shared_net.clone());
        wm.tick();
        wm.draw(&mut fb, &theme);
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

    // ── Step 1: framebuffer. If THIS fails the desktop genuinely has
    //    no screen — fail loud so the supervisor can demote.
    eprintln!("[drdr-desk] opening framebuffer {}", &args.fb_path);
    let mut fb = match Framebuffer::open(&args.fb_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("drdr-desk: open {}: {e}", args.fb_path);
            return ExitCode::from(1);
        }
    };
    eprintln!("[drdr-desk] framebuffer: {}", fb.describe());

    // ── Step 2: take the VT so fbcon stops repainting on top of us.
    //    Non-fatal — a serial-only boot just runs without it.
    let _vt = match VtGuard::acquire() {
        Ok(g) => {
            eprintln!("[drdr-desk] virtual terminal acquired (graphics mode)");
            Some(g)
        }
        Err(e) => {
            eprintln!("[drdr-desk] could not take over the console: {e} (continuing)");
            None
        }
    };

    // ── Step 3: build the WM with the icon grid + start menu, then
    //    paint the FIRST FRAME immediately. No windows are auto-opened;
    //    the desktop greets the user with clickable icons.
    let mut wm = WindowManager::new(fb.width, fb.height);
    build_desktop(&mut wm, shared_net.clone());
    let launcher_net = shared_net.clone();
    wm.set_launcher(move || Spawn {
        rect: Rect::new(360, 250, 470, 280),
        app: Box::new(LauncherApp::new(launcher_net.clone())),
    });

    // Double buffering: render the whole scene off-screen, then blit
    // to /dev/fb0 in one pass — the monitor only ever sees whole frames.
    let mut back = Framebuffer::in_memory(fb.width, fb.height);
    eprintln!("[drdr-desk] painting first frame");
    wm.draw(&mut back, &theme);
    fb.present(&back);
    eprintln!("[drdr-desk] first frame on screen");

    // ── Step 4: start DrDrNet in the background. The desktop is now
    //    on screen, so a slow UDP-broadcast bind on real hardware can
    //    take as long as it needs — the user already has a usable UI.
    {
        let shared_net = shared_net.clone();
        let host = hostname();
        thread::spawn(move || match NetState::start(host) {
            Ok(s) => {
                eprintln!(
                    "[drdr-desk] DrDrNet reactor listening on {}  (peer id {:016x})",
                    s.reactor_addr, s.me.id
                );
                if let Ok(mut g) = shared_net.lock() {
                    *g = Some(s);
                }
            }
            Err(e) => {
                eprintln!("[drdr-desk] DrDrNet disabled: {e} (continuing)");
            }
        });
    }

    // Attach input WITHOUT EVER BLOCKING. The old code looped forever
    // until a keyboard opened — fatal on a Surface Go 2, which is a
    // tablet: its only built-in pointer is the touchscreen and there is
    // no keyboard at all unless the Type Cover is clipped on. That is
    // exactly the "nothing happens, stuck" report. Now we take whatever
    // exists right now (possibly nothing) and keep (re)attaching live in
    // the loop, so the desktop is always on screen and becomes usable
    // the moment a finger, mouse or keyboard appears.
    //
    // Real hardware almost always has more than one input device live at
    // once — internal keyboard + Type Cover, touchpad + TrackPoint + USB
    // mouse, etc. The hub now polls them all in parallel so a user
    // never has to think about which device "the system listens to".
    let mut hub = InputHub::empty();
    attach_all_keyboards(&mut hub, args.kbd_path.as_deref());
    attach_all_pointers(&mut hub, &args, fb.width, fb.height);
    let mut last_scan = Instant::now();

    // Event loop. Block for input or the heartbeat, then COALESCE every
    // packet already queued (a USB mouse fires 60–125/sec; one full
    // redraw per packet makes the renderer fall behind and the cursor
    // lag seconds behind the hand). Repaint ONCE per batch, and only
    // when something actually changed (`needs_redraw`), so an idle
    // desktop is free and motion stays smooth.
    loop {
        // Live hot-plug: USB enumerates a beat after boot and a Type
        // Cover can be attached after the desktop is already up. Rescan
        // a couple of times a second and add ANY newly-appeared device
        // (the hub deduplicates by path) — cheap, and it means the user
        // never has to reboot to get input recognised.
        if last_scan.elapsed() >= Duration::from_millis(700) {
            last_scan = Instant::now();
            attach_all_keyboards(&mut hub, args.kbd_path.as_deref());
            attach_all_pointers(&mut hub, &args, fb.width, fb.height);
        }

        match hub.poll_event(Duration::from_millis(250)) {
            Ok(HubEvent::Key(k)) => wm.handle_key(k),
            Ok(HubEvent::Mouse(m)) => wm.handle_mouse(m),
            Ok(HubEvent::Tick) => wm.tick(),
            Err(e) => {
                eprintln!("drdr-desk: input error: {e}");
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        }
        for _ in 0..16 {
            match hub.poll_event(Duration::from_millis(0)) {
                Ok(HubEvent::Key(k)) => wm.handle_key(k),
                Ok(HubEvent::Mouse(m)) => wm.handle_mouse(m),
                Ok(HubEvent::Tick) => break, // nothing more queued
                Err(_) => break,
            }
        }
        if wm.needs_redraw() {
            theme = apps::current_theme(); // Settings may have toggled it
            wm.draw(&mut back, &theme);
            // Until any input is attached, overlay a hint so a bare
            // tablet boot explains itself instead of looking dead.
            if !hub.has_keyboard() && !hub.has_pointer() {
                draw_waiting_banner(&mut back, &theme);
            }
            fb.present(&back);
        }
    }
}

/// Open every keyboard the system currently exposes and add it to the
/// hub. Already-open paths are skipped, so calling this on every rescan
/// (700 ms) just picks up newly-attached devices. An explicit `--kbd`
/// is opened in addition to auto-detected ones (it's just a hint).
fn attach_all_keyboards(hub: &mut InputHub, explicit: Option<&str>) {
    let mut paths: Vec<String> = detect_all_keyboards();
    if let Some(p) = explicit {
        if !paths.iter().any(|x| x == p) {
            paths.push(p.to_string());
        }
    }
    for path in paths {
        if hub.has_path(&path) {
            continue;
        }
        match KeyReader::open(&path) {
            Ok(k) => {
                eprintln!("[drdr-desk] keyboard: {path}");
                hub.add_keyboard(k, path);
            }
            Err(e) => {
                eprintln!("[drdr-desk] keyboard {path}: {e} (will retry)");
            }
        }
    }
}

/// Open every relative pointer (real mouse, PS/2 touchpad, TrackPoint)
/// AND every touchscreen / absolute pointer, calibrated to the screen.
fn attach_all_pointers(hub: &mut InputHub, args: &Args, sw: u32, sh: u32) {
    // Explicit --mouse wins as a relative pointer.
    if let Some(p) = args.mouse_path.as_deref() {
        if !hub.has_path(p) {
            match PointerReader::open(p) {
                Ok(pr) => {
                    eprintln!("[drdr-desk] mouse (explicit): {p}");
                    hub.add_pointer(pr, p.to_string());
                }
                Err(e) => eprintln!("[drdr-desk] mouse {p}: {e}"),
            }
        }
    }
    for path in detect_all_mice() {
        if hub.has_path(&path) {
            continue;
        }
        match PointerReader::open(&path) {
            Ok(pr) => {
                eprintln!("[drdr-desk] pointer: {path}");
                hub.add_pointer(pr, path);
            }
            Err(e) => eprintln!("[drdr-desk] pointer {path}: {e} (will retry)"),
        }
    }
    for path in detect_all_touch() {
        if hub.has_path(&path) {
            continue;
        }
        match PointerReader::open_abs(&path, sw, sh) {
            Ok(pr) => {
                eprintln!("[drdr-desk] touchscreen: {path} (absolute, {sw}x{sh})");
                hub.add_pointer(pr, path);
            }
            Err(e) => eprintln!("[drdr-desk] touch {path}: {e} (will retry)"),
        }
    }
}

/// A centred, hard-to-miss message while no input device has attached —
/// proof the renderer works and a plain-language instruction for the
/// owner of a keyboardless tablet.
fn draw_waiting_banner(fb: &mut Framebuffer, theme: &Theme) {
    let msg = "Touch the screen, or attach a keyboard / mouse";
    let sub = "DrDrOS is running — waiting for an input device";
    let mw = GLYPH_WIDTH * msg.len() as u32;
    let sw = GLYPH_WIDTH * sub.len() as u32;
    let cx = fb.width / 2;
    let by = fb.height.saturating_sub(GLYPH_HEIGHT * 4);
    fb.shade_rect(
        0,
        by.saturating_sub(10),
        fb.width,
        GLYPH_HEIGHT * 3 + 20,
        Px::rgba(0, 0, 0, 150),
    );
    draw_text(fb, cx.saturating_sub(mw / 2), by, msg, theme.accent, theme.bg);
    draw_text(
        fb,
        cx.saturating_sub(sw / 2),
        by + GLYPH_HEIGHT + 4,
        sub,
        theme.muted,
        theme.bg,
    );
}

/// Greet the user with a clean desktop: an icon grid (one tile per
/// app), a Start menu mirror of the same catalogue, and **no**
/// auto-opened windows. Earlier phases opened four windows on boot
/// which felt cluttered and made the shell hard to find — a clear
/// icon launcher is what a normal user expects.
fn build_desktop(wm: &mut WindowManager, net_state: SharedNet) {
    // The Start menu (taskbar) and the desktop icons share one app
    // catalogue, so a new app shows up in both for free.
    wm.set_start_menu(apps::app_catalog(net_state.clone()));
    wm.set_desktop_icons(apps::desktop_icons(net_state));
}

/// Best-effort node name: prefer the configured `/etc/hostname`, then
/// the live kernel value (`/proc/sys/kernel/hostname`, which drdr-init
/// sets at boot), then the project name.
fn hostname() -> String {
    for path in ["/etc/hostname", "/proc/sys/kernel/hostname"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "drdros".into()
}

// ─── Argv ────────────────────────────────────────────────────────────

struct Args {
    fb_path: String,
    kbd_path: Option<String>,
    mouse_path: Option<String>,
    ppm_path: Option<String>,
}

fn parse_args(mut argv: Vec<String>) -> Result<Args, String> {
    let _ = argv.drain(..1);
    let mut a = Args {
        fb_path: "/dev/fb0".into(),
        kbd_path: None,
        mouse_path: None,
        ppm_path: None,
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
            "--mouse" => {
                a.mouse_path = Some(argv.get(i + 1).cloned().ok_or("--mouse needs a path")?);
                i += 2;
            }
            "--ppm" => {
                a.ppm_path = Some(argv.get(i + 1).cloned().ok_or("--ppm needs a path")?);
                i += 2;
            }
            "-h" | "--help" => {
                println!("drdr-desk — DrDrOS graphical session (Tier 2 WM)");
                println!("  drdr-desk [--fb /dev/fb0] [--kbd eventN] [--mouse eventM]");
                println!("  drdr-desk --ppm out.ppm        # host snapshot");
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg '{other}' (try --help)")),
        }
    }
    Ok(a)
}
