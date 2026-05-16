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

use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use drdr_fb::Framebuffer;
use drdr_net::status::{KIND_STAT_OK, Stat};
use drdr_net::{Frame, pack, reactor};
use drdr_ui::{
    HubEvent, InputHub, KeyReader, PointerReader, Rect, Spawn, Theme, VtGuard, WindowManager,
    detect_keyboard, detect_mouse,
};

use apps::{AboutApp, FilesApp, LauncherApp, NetApp, SystemApp};

fn main() -> ExitCode {
    let args = match parse_args(env::args().collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("drdr-desk: {e}");
            return ExitCode::from(2);
        }
    };

    let theme = Theme::DRDR;

    // Bring DrDrNet's async reactor up first so the "DrDrNet" window has
    // somewhere to connect. Returns None if loopback isn't usable — the
    // desktop still runs, the panel just shows "offline".
    let net_addr = start_status_server();

    // Snapshot mode: one frame to a heap framebuffer → PPM. No devices.
    // We tick once so the DrDrNet panel shows real data in the image.
    if let Some(path) = &args.ppm_path {
        let mut fb = Framebuffer::in_memory(1024, 768);
        let mut wm = WindowManager::new(fb.width, fb.height);
        build_desktop(&mut wm, net_addr);
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

    let mut fb = match Framebuffer::open(&args.fb_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("drdr-desk: open {}: {e}", args.fb_path);
            return ExitCode::from(1);
        }
    };

    // Take the virtual terminal away from the kernel BEFORE we draw, so
    // fbcon stops repainting /dev/fb0 underneath us (flicker + the
    // "always-open terminal"), and keystrokes stop being echoed to the
    // dead console (we read evdev, which is unaffected). Held in `_vt`
    // for the whole program; Drop restores a usable text console on any
    // exit. Non-fatal like the splash — a serial-only boot has no VT.
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

    let mut wm = WindowManager::new(fb.width, fb.height);
    build_desktop(&mut wm, net_addr);
    // The way back: when the last window closes, the WM rebuilds this
    // Launcher so closed windows can always be reopened.
    wm.set_launcher(move || Spawn {
        rect: Rect::new(360, 250, 470, 280),
        app: Box::new(LauncherApp::new(net_addr)),
    });

    // Double buffering: render the whole scene off-screen, then blit it
    // to /dev/fb0 in one pass — the monitor only ever sees whole frames.
    let mut back = Framebuffer::in_memory(fb.width, fb.height);

    // Show the desktop IMMEDIATELY, before waiting for input. The old
    // order (detect input first) meant up to ~16s of the drdr-init
    // splash before any window appeared — and if a device never showed,
    // a hard exit → drdr-init respawn → a permanent splash. Now the
    // desktop is on screen within a frame; we attach input live.
    wm.draw(&mut back, &theme);
    fb.present(&back);

    // Attach the keyboard. On real hardware it is USB and enumerates a
    // beat after boot. We never give up and never exit — we keep the
    // desktop live (ticking + redrawing) until a keyboard opens, so the
    // user always sees a working desktop, never a frozen splash or a
    // respawn loop. An explicit --kbd is tried first each pass.
    let keys = loop {
        if let Some(p) = args.kbd_path.clone().or_else(detect_keyboard) {
            match KeyReader::open(&p) {
                Ok(k) => {
                    eprintln!("[drdr-desk] keyboard: {p}");
                    break k;
                }
                Err(e) => eprintln!("[drdr-desk] open {p}: {e}; retrying"),
            }
        }
        pump(&mut wm, &mut back, &mut fb, &theme);
        thread::sleep(Duration::from_millis(150));
    };

    // The mouse is optional (keyboard-only is valid). Wait a bounded
    // few seconds for the USB pointer to enumerate, pumping the desktop
    // so it stays live, then give up gracefully.
    let mouse_path = match args.mouse_path.clone() {
        Some(p) => Some(p),
        None => {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                if let Some(p) = detect_mouse() {
                    break Some(p);
                }
                if Instant::now() >= deadline {
                    eprintln!("[drdr-desk] no pointer after 4s — keyboard-only");
                    break None;
                }
                pump(&mut wm, &mut back, &mut fb, &theme);
                thread::sleep(Duration::from_millis(150));
            }
        }
    };
    let pointer = match &mouse_path {
        Some(p) => match PointerReader::open(p) {
            Ok(pr) => {
                eprintln!("[drdr-desk] mouse: {p}");
                Some(pr)
            }
            Err(e) => {
                eprintln!("[drdr-desk] mouse {p}: {e} (keyboard-only)");
                None
            }
        },
        None => None,
    };

    let mut hub = InputHub::new(keys, pointer);

    // Event loop. Block for input or the heartbeat, then COALESCE every
    // packet already queued (a USB mouse fires 60–125/sec; one full
    // redraw per packet makes the renderer fall behind and the cursor
    // lag seconds behind the hand). Repaint ONCE per batch, and only
    // when something actually changed (`needs_redraw`), so an idle
    // desktop is free and motion stays smooth.
    loop {
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
            wm.draw(&mut back, &theme);
            fb.present(&back);
        }
    }
}

/// One desktop frame while we're still waiting on input devices: advance
/// the clocks/panels and repaint if anything changed. Keeps the desktop
/// alive and on screen instead of a frozen splash.
fn pump(
    wm: &mut WindowManager,
    back: &mut Framebuffer,
    fb: &mut Framebuffer,
    theme: &Theme,
) {
    wm.tick();
    if wm.needs_redraw() {
        wm.draw(back, theme);
        fb.present(back);
    }
}

/// Open the default set of overlapping windows. Order matters: the last
/// one opened is on top and focused, so DrDrNet (the headline Tier-3
/// demo) gets the accent title bar in the boot screenshot.
fn build_desktop(wm: &mut WindowManager, net_addr: Option<SocketAddr>) {
    let (sw, sh) = wm.screen();

    // Lay out relative to the screen, clamped so nothing falls off a
    // smaller framebuffer than the QEMU default 1024x768.
    let clamp = |x: u32, y: u32, w: u32, h: u32| -> Rect {
        let w = w.min(sw.saturating_sub(8));
        let h = h.min(sh.saturating_sub(8));
        let x = x.min(sw.saturating_sub(w));
        let y = y.min(sh.saturating_sub(h));
        Rect::new(x, y, w, h)
    };

    // Distinct positions (no two windows stacked exactly). The Launcher
    // opens last so it's on top and focused — it's the "what can I do"
    // hub, the right thing to greet the user with.
    wm.open(clamp(40, 56, 540, 270), Box::new(AboutApp));
    wm.open(clamp(560, 70, 440, 320), Box::new(NetApp::new(net_addr)));
    wm.open(clamp(70, 360, 560, 380), Box::new(FilesApp::new("/")));
    wm.open(clamp(660, 470, 340, 160), Box::new(SystemApp::new()));
    wm.open(clamp(380, 200, 470, 300), Box::new(LauncherApp::new(net_addr)));
}

/// Start DrDrNet's Tier 3 reactor on an ephemeral loopback port and
/// serve the `status` protocol from a background thread. Returns the
/// bound address, or `None` if loopback is down (non-fatal).
fn start_status_server() -> Option<SocketAddr> {
    let listener = match reactor::Listener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[drdr-desk] DrDrNet server bind failed: {e} (panel offline)");
            return None;
        }
    };
    let addr = listener.local_addr().ok()?;
    eprintln!("[drdr-desk] DrDrNet reactor listening on {addr}");

    thread::spawn(move || {
        let start = Instant::now();
        let host = hostname();
        let mut served: u64 = 0;
        // One thread, many short connections — exactly what the reactor
        // is for. The handler echoes the request's correlation id.
        let _ = listener.run(move |f: &Frame| {
            served += 1;
            let stat = Stat {
                uptime_secs: start.elapsed().as_secs(),
                requests: served,
                host: host.clone(),
            };
            Some(Frame::with_id(KIND_STAT_OK, f.id, pack(&stat)))
        });
    });

    Some(addr)
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
