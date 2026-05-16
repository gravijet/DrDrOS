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
    HubEvent, InputHub, KeyReader, PointerReader, Rect, Theme, VtGuard, WindowManager,
    detect_keyboard, detect_mouse,
};

use apps::{AboutApp, FilesApp, NetApp, SystemApp};

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

    // Keyboard is required; the mouse is optional (keyboard-only is a
    // valid fallback — Alt-Tab + arrow keys still drive everything).
    // We *wait* for it: on real UEFI hardware the keyboard is USB and
    // enumerates a beat after we start (see `wait_for_device`). Hard-
    // exiting on the first miss is what makes a real machine sit on the
    // boot splash forever (exit → drdr-init respawn → race → repeat).
    // 12s is generous for USB enumeration; an explicit --kbd skips it.
    let kbd_path = match args.kbd_path.clone() {
        Some(p) => p,
        None => match wait_for_device("keyboard", Duration::from_secs(12), detect_keyboard) {
            Some(p) => p,
            None => {
                eprintln!("drdr-desk: no keyboard under /dev/input — pass --kbd");
                return ExitCode::from(1);
            }
        },
    };
    let keys = match KeyReader::open(&kbd_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("drdr-desk: open {kbd_path}: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!("[drdr-desk] keyboard: {kbd_path}");

    // An explicit --mouse wins; otherwise auto-detect, but *wait* for it
    // (see `wait_for_device`): a USB pointer enumerates asynchronously
    // and appears a beat after we start. wait_for_device returns the
    // instant a relative pointer shows up, or None after the budget (a
    // genuinely mouseless box just stays keyboard-only, 4s later).
    let mouse_path = match args.mouse_path.clone() {
        Some(p) => Some(p),
        None => wait_for_device("pointer", Duration::from_secs(4), detect_mouse),
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
        None => {
            eprintln!("[drdr-desk] no mouse found (keyboard-only)");
            None
        }
    };

    // Take the virtual terminal away from the kernel: graphics mode so
    // fbcon stops repainting /dev/fb0 underneath us (the flicker + the
    // "always-open terminal"), and keyboard silenced so keystrokes stop
    // being echoed to the dead console instead of reaching us (we read
    // the keyboard from evdev, which is unaffected). Held in `_vt` for
    // the whole program: its Drop restores a usable text console on any
    // exit, including a panic. Non-fatal like the splash — a serial-only
    // boot has no VT to grab and that's fine.
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

    let mut hub = InputHub::new(keys, pointer);
    let mut wm = WindowManager::new(fb.width, fb.height);
    build_desktop(&mut wm, net_addr);

    // Double buffering: render the whole scene into this off-screen
    // buffer, then blit it to /dev/fb0 in one pass (`fb.present`). The
    // monitor only ever sees complete frames, so there's no flicker from
    // the clear-then-repaint sequence being scanned out mid-draw.
    let mut back = Framebuffer::in_memory(fb.width, fb.height);

    // The event loop. Draw the whole scene to the back buffer, present
    // it in one blit, then block for input or the heartbeat.
    //
    // Then *coalesce*: a full redraw + ~3 MB present is far slower than
    // the 60–125 motion packets/sec a USB mouse emits. Repainting once
    // per packet makes the renderer fall behind, events back up in the
    // kernel buffer, and the cursor lags seconds behind the hand. So
    // after the blocking wait we drain everything already queued,
    // applying it WITHOUT a repaint between events — one repaint per
    // batch. The 0 ms poll is a non-blocking "is anything else waiting?"
    // and `Tick` from it means "queue empty". The count cap still forces
    // a repaint during *continuous* motion (a window drag) so the screen
    // keeps up instead of freezing until the hand stops.
    loop {
        wm.draw(&mut back, &theme);
        fb.present(&back);
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

    wm.open(clamp(40, 56, 540, 250), Box::new(AboutApp));
    wm.open(clamp(90, 300, 560, 410), Box::new(FilesApp::new("/")));
    wm.open(clamp(620, 470, 370, 175), Box::new(SystemApp::new()));
    wm.open(clamp(560, 70, 440, 320), Box::new(NetApp::new(net_addr)));
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

/// Poll `detect` until it finds a device or `budget` elapses, returning
/// the moment one appears (so a present device costs only its
/// enumeration time, not the whole budget).
///
/// Why this is needed for *both* keyboard and mouse: USB HID devices
/// enumerate **asynchronously** — the controller (xHCI) probes, then the
/// device is found a second or two later. A single check at startup
/// races that and loses. In QEMU we hand the VM a *PS/2* keyboard
/// (i8042, synchronous, there instantly) so only the USB mouse raced;
/// but on real UEFI hardware the keyboard is USB too, so it races
/// exactly the same way — and because the keyboard is mandatory,
/// drdr-desk would exit, drdr-init would respawn it, it would race
/// again, and the machine would sit on the boot splash forever.
fn wait_for_device(
    what: &str,
    budget: Duration,
    detect: impl Fn() -> Option<String>,
) -> Option<String> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(p) = detect() {
            return Some(p);
        }
        if Instant::now() >= deadline {
            eprintln!("[drdr-desk] no {what} after {}s", budget.as_secs());
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
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
