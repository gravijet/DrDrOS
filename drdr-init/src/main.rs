//! drdr-init — PID 1 for DrDrOS (Tier 3: framebuffer splash + supervisor).
//!
//! When the Linux kernel finishes booting it unpacks our initramfs into RAM
//! and runs `/init` as process ID 1 — that's us. As PID 1 we are responsible
//! for everything userland: mounting pseudo-filesystems, reaping orphaned
//! children, and keeping the program the human interacts with alive.
//!
//! Tier 3 work (this file):
//!   1. Mount /proc, /sys, /dev (three virtual filesystems the rest of
//!      userland expects to find).
//!   2. Print a console banner so serial-only setups still see us.
//!   3. Open /dev/fb0 and paint the boot splash in the real DrDrTheme
//!      colors. Failures here are non-fatal — headless QEMU and
//!      serial-only boots simply skip the splash.
//!   4. **Supervise** the graphical session: *spawn* (not exec) the best
//!      available session program, wait for it, and respawn it if it ever
//!      exits. While waiting we reap every orphaned zombie the kernel
//!      reparents onto us. PID 1 never returns.
//!
//! Why a supervisor and not `exec()`?  `exec()` *replaces* PID 1 with the
//! session — if that program then crashes, there is no PID 1 left and the
//! kernel panics ("Attempted to kill init!"). A supervisor keeps PID 1
//! resident, so a desktop crash is just a flicker-and-redraw, not a dead
//! machine.

mod net;

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use drdr_fb::Framebuffer;
use drdr_font::{GLYPH_HEIGHT, GLYPH_WIDTH, draw_text};
use drdr_ui::Theme;
use nix::errno::Errno;
use nix::mount::{MsFlags, mount};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

/// Programs we will try to run as the session, best first. The graphical
/// desktop is the goal; the others are fallbacks so an early-bring-up
/// rootfs (drdr-apps not installed yet) still boots to *something*
/// interactive instead of a dead console.
const SESSION_CANDIDATES: &[&str] = &["/bin/drdr-desk", "/bin/drdr-shell", "/bin/sh"];

fn main() {
    print_banner();

    // Mount the three pseudo-filesystems userland expects. None of them
    // touch disk — they are kernel-exposed "virtual" filesystems:
    //   /proc      → information about processes and kernel state
    //   /sys       → kernel-object hierarchy (devices, drivers, classes)
    //   /dev       → device nodes (the framebuffer, ttys, disks, etc.)
    //
    // MS_NOSUID / MS_NOEXEC / MS_NODEV harden the mount: no setuid binaries,
    // no executables, no device nodes can be created on these mountpoints.
    let common = MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV;
    mount_pseudo("proc", "/proc", "proc", common);
    mount_pseudo("sysfs", "/sys", "sysfs", common);
    // /dev MUST allow device nodes (that's its whole point), so only NOSUID.
    mount_pseudo("devtmpfs", "/dev", "devtmpfs", MsFlags::MS_NOSUID);

    // Set the kernel hostname from /etc/hostname, like any real init —
    // the kernel boots with hostname "(none)" until userland sets it.
    // Non-fatal cosmetic step (the DrDrNet panel shows it).
    set_hostname();

    // Bring the loopback interface up so userland TCP works (the DrDrDesk
    // "DrDrNet" window talks to a local reactor server over 127.0.0.1).
    // Non-fatal: a desktop with no networking is still perfectly usable.
    match net::bring_up_loopback() {
        Ok(()) => println!("[drdr-init] loopback up (127.0.0.1/8)"),
        Err(e) => println!("[drdr-init] loopback bring-up failed: {e} (continuing)"),
    }

    // Paint the splash. Any failure here just logs and continues — we still
    // want the session on headless / serial-only boots where there's no fb0.
    // We log the *detected pixel format*: on real machines (efifb on a
    // Surface, say) a wrong format is the difference between a desktop and
    // a screen that merely looks frozen, so make it visible on the console.
    match draw_splash("/dev/fb0") {
        Ok(desc) => println!("[drdr-init] framebuffer splash painted — {desc}"),
        Err(e) => println!("[drdr-init] no framebuffer splash: {e} (continuing)"),
    }

    supervise();
}

/// The supervisor loop. Never returns — PID 1 must stay resident for the
/// entire life of the machine.
///
/// Each iteration:
///   1. pick the best session program that actually exists,
///   2. *spawn* it as a child (we keep our own process image),
///   3. block in [`reap_until`], which reaps every child the kernel hands
///      us and only returns when *our* session child is the one that died,
///   4. small backoff, then respawn.
fn supervise() -> ! {
    loop {
        let Some(prog) = SESSION_CANDIDATES.iter().find(|p| Path::new(p).exists()) else {
            // Nothing to run yet. Don't busy-spin: wait a few seconds and
            // re-check (a device or mount might still be settling).
            eprintln!(
                "[drdr-init] no session program found ({}); retrying in 3s",
                SESSION_CANDIDATES.join(", ")
            );
            thread::sleep(Duration::from_secs(3));
            continue;
        };

        println!("[drdr-init] starting session: {prog}");
        let child = match Command::new(prog).spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[drdr-init] could not spawn {prog}: {e}; retrying in 2s");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let session_pid = Pid::from_raw(child.id() as i32);

        // We manage the wait ourselves via waitpid(-1), so we must NOT let
        // std also try to reap this child. Leaking the handle is safe:
        // `Child`'s Drop does not kill or wait on the process.
        std::mem::forget(child);

        match reap_until(session_pid) {
            SessionEnd::Exited(code) => {
                println!("[drdr-init] session {prog} exited ({code}); respawning");
            }
            SessionEnd::Signalled(sig) => {
                println!("[drdr-init] session {prog} killed by signal {sig}; respawning");
            }
            SessionEnd::NoChildren => {
                eprintln!("[drdr-init] session {prog} vanished before we could wait; respawning");
            }
        }

        // Backoff so a session that crashes immediately can't peg the CPU
        // in a tight spawn/die loop.
        thread::sleep(Duration::from_millis(800));
    }
}

/// How the session process ended (used only for the log line).
enum SessionEnd {
    Exited(i32),
    Signalled(i32),
    NoChildren,
}

/// Block reaping children until the one identified by `session_pid` dies.
///
/// As PID 1 we are the parent-of-last-resort: when any process anywhere
/// loses its real parent, the kernel reparents it onto us, and when it
/// exits it becomes a zombie that *only we* can clear by `wait`-ing for
/// it. So we loop on `waitpid(-1)` (any child), silently reaping orphans,
/// and only return once `session_pid` itself is the process that exited.
fn reap_until(session_pid: Pid) -> SessionEnd {
    loop {
        // `None` for the second arg = blocking wait (no WNOHANG): we sleep
        // until *some* child changes state instead of busy-polling.
        match waitpid(Pid::from_raw(-1), None) {
            Ok(status) => {
                let pid = status.pid();
                let is_session = pid == Some(session_pid);
                match status {
                    WaitStatus::Exited(_, code) => {
                        if is_session {
                            return SessionEnd::Exited(code);
                        }
                        // An orphan we just cleaned up. Note it and keep going.
                        if let Some(p) = pid {
                            println!("[drdr-init] reaped orphan pid {p} (exit {code})");
                        }
                    }
                    WaitStatus::Signaled(_, sig, _) => {
                        if is_session {
                            return SessionEnd::Signalled(sig as i32);
                        }
                        if let Some(p) = pid {
                            println!("[drdr-init] reaped orphan pid {p} (signal {sig:?})");
                        }
                    }
                    // Stopped/Continued/etc. — not an exit; keep waiting.
                    _ => {}
                }
            }
            // No children at all: the session must have been reaped already
            // (or never started). Let the supervisor respawn it.
            Err(Errno::ECHILD) => return SessionEnd::NoChildren,
            // Interrupted by a signal — just retry the wait.
            Err(Errno::EINTR) => continue,
            Err(e) => {
                eprintln!("[drdr-init] waitpid error: {e}; respawning session");
                return SessionEnd::NoChildren;
            }
        }
    }
}

/// Apply the configured hostname. Buildroot writes the name into
/// `/etc/hostname`; the kernel itself starts as "(none)" until some
/// userland process calls `sethostname(2)`. On a normal distro an init
/// script does it — here PID 1 is us, so we do it.
fn set_hostname() {
    let name = match std::fs::read_to_string("/etc/hostname") {
        Ok(s) => s.trim().to_string(),
        Err(_) => return, // no config → leave the kernel default
    };
    if name.is_empty() {
        return;
    }
    match nix::unistd::sethostname(&name) {
        Ok(()) => println!("[drdr-init] hostname set to {name}"),
        Err(e) => println!("[drdr-init] sethostname({name}): {e} (continuing)"),
    }
}

/// Print the DrDrOS startup banner to the console.
///
/// At this point /dev/console exists as a static device node baked into the
/// rootfs by Buildroot, so stdout is already wired up to the screen.
fn print_banner() {
    println!();
    println!("  ╔══════════════════════════════════════════════════════════╗");
    println!("  ║                                                          ║");
    println!("  ║                  D r D r O S                             ║");
    println!("  ║                                                          ║");
    println!("  ║              drdr-init v{:<8}  PID 1 alive            ║", env!("CARGO_PKG_VERSION"));
    println!("  ║                                                          ║");
    println!("  ╚══════════════════════════════════════════════════════════╝");
    println!();
}

/// Open the framebuffer at `path` and paint the DrDrOS boot splash, in the
/// same [`Theme::DRDR`] palette the desktop uses so boot is visually
/// continuous (no jarring color change when the session takes over).
///
/// Layout (all coordinates relative to the screen's top-left):
///   - whole screen filled with the theme background
///   - centred "DrDrOS" wordmark in primary text
///   - centred "starting the desktop..." sub-line in muted text
///
/// The function is intentionally cheap (no animation, no timer): the
/// supervisor spawns the session within milliseconds, which repaints the
/// screen itself — the splash just covers the gap so the user never stares
/// at a blank/garbage framebuffer.
fn draw_splash(path: &str) -> std::io::Result<String> {
    let mut fb = Framebuffer::open(path)?;
    let desc = fb.describe();

    // Same palette the desktop boots into, so the hand-off is seamless.
    let theme = Theme::DEFAULT;
    fb.clear(theme.bg);

    // A soft vertical gradient instead of a flat fill — the boot screen
    // should already feel like the modern desktop it hands off to.
    fb.fill_rect_v(0, 0, fb.width, fb.height, theme.bg, theme.surface);

    // Centre two short lines vertically around the middle of the screen.
    let title = "DrDrOS";
    let sub = "starting the desktop...";

    let title_w = GLYPH_WIDTH * title.len() as u32;
    let sub_w = GLYPH_WIDTH * sub.len() as u32;

    // `saturating_sub` so tiny framebuffers (smaller than the text) still
    // render — they just clip to the left edge instead of underflowing.
    let title_x = fb.width.saturating_sub(title_w) / 2;
    let sub_x = fb.width.saturating_sub(sub_w) / 2;
    let title_y = fb.height / 2 - GLYPH_HEIGHT;
    let sub_y = fb.height / 2 + GLYPH_HEIGHT / 2;

    // A 2x-scaled wordmark reads as a logo, not console text.
    draw_text_scaled(&mut fb, title_x.saturating_sub(title_w / 2), title_y.saturating_sub(GLYPH_HEIGHT / 2), title, theme.accent, 2);
    draw_text(&mut fb, sub_x, sub_y, sub, theme.muted, theme.bg);

    // An indeterminate progress bar so a slow real-hardware boot looks
    // alive rather than hung.
    let bw = (fb.width / 4).clamp(80, 360);
    let bx = fb.width.saturating_sub(bw) / 2;
    let by = sub_y + GLYPH_HEIGHT * 2;
    fb.fill_rect(bx, by, bw, 4, theme.border);
    fb.fill_rect(bx, by, bw / 3, 4, theme.accent);

    // Print the detected pixel format on-screen too: if the desktop ever
    // looks frozen on real hardware, the user can read the real format
    // off the panel even with no serial cable.
    let foot_y = fb.height.saturating_sub(GLYPH_HEIGHT + 6);
    draw_text(&mut fb, 8, foot_y, &desc, theme.muted, theme.bg);

    Ok(desc)
}

/// Draw `text` with each glyph blown up `scale`x by replicating pixels —
/// cheap "big text" for the boot wordmark without a second font.
fn draw_text_scaled(fb: &mut Framebuffer, x: u32, y: u32, text: &str, fg: drdr_fb::Pixel, scale: u32) {
    let mut cx = x;
    for ch in text.bytes() {
        let glyph = drdr_font::glyph_for(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8u32 {
                if *bits & (0x80u8 >> col) != 0 {
                    fb.fill_rect(cx + col * scale, y + row as u32 * scale, scale, scale, fg);
                }
            }
        }
        cx += GLYPH_WIDTH * scale;
    }
}

/// Mount a single pseudo-filesystem; log success or failure, never panic.
///
/// We're tolerant of mount failures because some initramfs hooks may have
/// already mounted these (e.g. devtmpfs auto-mount via CONFIG_DEVTMPFS_MOUNT).
/// If a mount fails we keep going and let the rest of boot proceed; we'll
/// see the diagnostic on the console.
fn mount_pseudo(source: &str, target: &str, fstype: &str, flags: MsFlags) {
    // Ensure the mountpoint exists. Our rootfs ships these directories
    // pre-created by Buildroot's skeleton, but defensive coding is cheap.
    let _ = std::fs::create_dir_all(target);

    // `nix::mount::mount` is a SAFE wrapper around the libc::mount syscall.
    // The `unsafe` lives inside `nix`; we don't write any here ourselves.
    let result = mount(
        Some(source),
        target,
        Some(fstype),
        flags,
        Option::<&str>::None,
    );

    match result {
        Ok(()) => println!("[drdr-init] mounted {fstype:>9} on {target}"),
        Err(e) => println!("[drdr-init] mount {fstype} on {target}: {e} (continuing)"),
    }
}
