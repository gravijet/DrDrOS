//! drdr-init — PID 1 for DrDrOS (Tier 2: framebuffer splash + shell).
//!
//! When the Linux kernel finishes booting it unpacks our initramfs into RAM
//! and runs `/init` as process ID 1 — that's us. As PID 1 we are responsible
//! for everything userland: mounting pseudo-filesystems, reaping orphans,
//! and launching the program the human actually interacts with.
//!
//! Tier 2 work:
//!   1. Mount /proc, /sys, /dev (three virtual filesystems the rest of
//!      userland expects to find).
//!   2. Print a console banner so serial-only setups still see us.
//!   3. Open /dev/fb0 and paint the boot splash with DrDrFont. Failures
//!      here are non-fatal — headless QEMU and serial-only boots simply
//!      skip the splash.
//!   4. exec() into /bin/sh, replacing ourselves with the shell.
//!
//! Tier 3 (Phase 2): exec drdr-shell instead of /bin/sh, keep PID 1
//! around as a supervisor that reaps orphans and respawns the shell.

use std::os::unix::process::CommandExt;
use std::process::Command;

use drdr_font::{GLYPH_HEIGHT, GLYPH_WIDTH, draw_text};
use drdr_ui::{Framebuffer, Pixel};
use nix::mount::{MsFlags, mount};

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

    // Paint the splash. Any failure here just logs and continues — we still
    // want the shell on headless / serial-only boots where there's no fb0.
    match draw_splash("/dev/fb0") {
        Ok(()) => println!("[drdr-init] framebuffer splash painted"),
        Err(e) => println!("[drdr-init] no framebuffer splash: {e} (continuing)"),
    }

    println!("[drdr-init] handing control to /bin/sh...");

    // exec() replaces THIS process image with /bin/sh. On success it never
    // returns — the calling process ceases to exist. The only way we reach
    // the line after is if exec failed (e.g. /bin/sh missing).
    let err = Command::new("/bin/sh").exec();
    eprintln!("[drdr-init] FATAL: could not exec /bin/sh: {err}");

    // PID 1 must NEVER exit (the kernel panics if it does), so if we get
    // here, park the thread forever instead of returning from main().
    loop {
        std::thread::park();
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

/// Open the framebuffer at `path` and paint the DrDrOS splash on it.
///
/// Layout (all coordinates relative to the screen's top-left):
///   - whole screen filled with a deep blue background
///   - one centred line: "DrDrOS booting..."  (white on blue, 2× scale via
///     extra padding cells — actually drawn at 1× for Phase 1)
///   - one centred sub-line below: "Phase 1"
///
/// The function is intentionally cheap (no animation, no timer): PID 1
/// hits exec() within milliseconds of returning, so the splash is the
/// *last* thing the framebuffer shows before the shell takes the console.
fn draw_splash(path: &str) -> std::io::Result<()> {
    let mut fb = Framebuffer::open(path)?;

    let bg = Pixel::rgb(0x10, 0x18, 0x40); // deep blue, the DrDrOS background
    let fg = Pixel::WHITE;
    fb.clear(bg);

    // Centre two short lines vertically around the middle of the screen.
    let title = "DrDrOS booting...";
    let sub = "Phase 1";

    let title_w = GLYPH_WIDTH * title.len() as u32;
    let sub_w = GLYPH_WIDTH * sub.len() as u32;

    // `saturating_sub` so tiny framebuffers (smaller than the text) still
    // render — they just clip to the left edge instead of underflowing.
    let title_x = fb.width.saturating_sub(title_w) / 2;
    let sub_x = fb.width.saturating_sub(sub_w) / 2;
    let title_y = fb.height / 2 - GLYPH_HEIGHT;
    let sub_y = fb.height / 2 + GLYPH_HEIGHT / 2;

    draw_text(&mut fb, title_x, title_y, title, fg, bg);
    draw_text(&mut fb, sub_x, sub_y, sub, fg, bg);

    Ok(())
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
