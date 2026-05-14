//! drdr-init — PID 1 for DrDrOS (Tier 1: console boot, no framebuffer yet).
//!
//! When the Linux kernel finishes booting it unpacks our initramfs into RAM
//! and runs `/init` as process ID 1 — that's us. As PID 1 we are responsible
//! for everything userland: mounting pseudo-filesystems, reaping orphans,
//! and launching the program the human actually interacts with.
//!
//! This tier does the minimum useful work:
//!   1. Mount /proc, /sys, /dev (three virtual filesystems the rest of
//!      userland expects to find).
//!   2. Print a banner so we can confirm we ran.
//!   3. exec() into /bin/sh, replacing ourselves with the shell — the
//!      shell now lives as PID 1 and we no longer need to reap zombies.
//!
//! Tier 2 (Phase 1 finale): draw the framebuffer splash before exec.
//! Tier 3 (Phase 2):        exec drdr-shell instead of /bin/sh.

use std::os::unix::process::CommandExt;
use std::process::Command;

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
