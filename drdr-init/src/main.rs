//! drdr-init — PID 1 for DrDrOS.
//!
//! This is the very first program the Linux kernel hands control to.
//! Once Phase 1 implementation lands, this binary will:
//!   1. Mount the essential pseudo-filesystems (/proc, /sys, /dev).
//!   2. Open /dev/fb0 and draw the DrDrOS boot splash.
//!   3. Launch DrDrShell on the console as its child process.
//!
//! For now this is a stub so the workspace compiles end-to-end.

fn main() {
    println!("drdr-init (stub) — Phase 1 implementation pending");
}
