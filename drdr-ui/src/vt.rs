//! drdr-ui::vt — take the Linux virtual terminal away from the kernel
//! so a graphics app can own the screen and the keyboard.
//!
//! The problem this solves
//! ───────────────────────
//! When Linux boots with `console=tty0` and the kernel is built with
//! `CONFIG_FRAMEBUFFER_CONSOLE`, the kernel runs its *own* text terminal
//! ("fbcon") directly on `/dev/fb0` — the same framebuffer DrDrDesk
//! paints to. Nobody told the kernel to stop, so two programs scribble
//! on the same pixels at once: every frame flickers, the kernel's text
//! cursor blinks through our windows, and there's an "always-open
//! terminal" underneath everything. Worse, the keys you press are eaten
//! by that terminal's input handling (the kernel echoes them to the
//! console) instead of reaching the app.
//!
//! The fix every framebuffer GUI uses
//! ──────────────────────────────────
//! The Linux console is controlled through two `ioctl(2)` calls on a
//! terminal device (`/dev/tty0` = whichever VT is on screen now):
//!
//!   - **KDSETMODE → KD_GRAPHICS**: "a graphics program owns this
//!     screen; stop drawing the text console on it." The kernel stops
//!     repainting fbcon, so we're the only writer to the framebuffer.
//!   - **KDSKBMODE → K_OFF**: "stop turning key presses into terminal
//!     input on this VT." The keystrokes no longer get echoed to the
//!     dead console. We don't lose input — DrDrDesk reads the keyboard
//!     straight from `/dev/input/eventN` (evdev), which taps the kernel
//!     *before* this terminal layer, so it is unaffected.
//!
//! Both settings are global to that VT, so we must put them back when we
//! exit — otherwise a crash leaves you staring at a black screen with a
//! dead keyboard. [`VtGuard`] is an RAII handle: acquire it once at
//! startup, hold it for the life of the program, and its `Drop` restores
//! `KD_TEXT` + the previous keyboard mode no matter how we leave `main`
//! (clean return, `?`, or panic-unwind).
//!
//! This is the same approach kmscon, fbterm and weston-on-fbdev take.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

// ─── Constants from linux/kd.h ───────────────────────────────────────
// These are stable kernel UAPI numbers (a console ioctl ABI that has
// not changed in decades), so it is correct to hard-code them.

// nix re-exports libc, so we don't need a direct libc dependency just
// for the one integer type the ioctl macros hand us.
type CInt = nix::libc::c_int;

/// `KDSETMODE` arg: ordinary text console (the boot-time default).
const KD_TEXT: CInt = 0x00;
/// `KDSETMODE` arg: a graphics program owns the screen — stop fbcon.
const KD_GRAPHICS: CInt = 0x01;

/// `KDSKBMODE` arg: the kernel keymap/ASCII default. We restore to this
/// if we could not read the real previous mode for some reason — it is
/// what a normal text console expects, so the worst case is still sane.
const K_XLATE: CInt = 0x01;
/// `KDSKBMODE` arg: this VT's keyboard produces *no* terminal input at
/// all. We read keys via evdev instead, so "off" is exactly right.
const K_OFF: CInt = 0x04;

// nix's `ioctl_*_bad!` macros build safe-signature wrappers around the
// raw `ioctl(2)` syscall for these legacy (pre-`_IOC`) request numbers —
// the same mechanism drdr-fb uses for the framebuffer ioctls.
//
//   *_write_int_bad → ioctl(fd, REQUEST, <int by value>)
//   *_read_bad      → ioctl(fd, REQUEST, <*mut T>) — fills T
nix::ioctl_write_int_bad!(kd_set_mode, 0x4B3A); // KDSETMODE
nix::ioctl_read_bad!(kd_get_kbmode, 0x4B44, CInt); // KDGKBMODE
nix::ioctl_write_int_bad!(kd_set_kbmode, 0x4B45); // KDSKBMODE
// KDGKBTYPE: "what kind of keyboard does this console have". It only
// succeeds on a real VT; on a serial line or a pty it fails with
// ENOTTY. That makes it the standard, cheap "is this actually a virtual
// terminal" probe (systemd and libvterm use exactly this) — we run it
// first so a serial-only boot fails *fast and cleanly* instead of us
// blindly issuing KD_GRAPHICS at something that will never show pixels.
nix::ioctl_read_bad!(kd_get_kbtype, 0x4B33, u8); // KDGKBTYPE

/// Owns the VT takeover for as long as it is alive.
///
/// Construct with [`VtGuard::acquire`] at startup and keep the value
/// bound (e.g. `let _vt = VtGuard::acquire();`) until the program ends.
/// Dropping it restores the console — see the module docs.
pub struct VtGuard {
    /// Kept open for the guard's whole life: the fd must stay valid so
    /// `Drop` can issue the restoring ioctls on it.
    tty: File,
    /// The keyboard mode the console had before us, to restore on drop.
    prev_kb_mode: CInt,
}

impl VtGuard {
    /// Switch the active console into graphics mode and silence its
    /// keyboard. Returns an error if there is no usable console device
    /// (e.g. a headless / serial-only boot) — callers should treat that
    /// as non-fatal, exactly like the boot splash: a desktop with no
    /// local screen to grab is unusual but not a reason to abort.
    pub fn acquire() -> io::Result<Self> {
        let tty = open_console()?;
        let fd = tty.as_raw_fd();

        // Confirm this is a real VT before we touch its mode. On a
        // serial-only / headless boot the "console" is not a VT and
        // KDGKBTYPE returns ENOTTY — bail now (non-fatal: the caller
        // just runs without a VT) instead of leaving the keyboard
        // silenced on a console that can't draw anyway.
        //
        // SAFETY: `fd` is the freshly opened console fd, valid for this
        // call; the ioctl writes exactly one byte into `kbtype`.
        let mut kbtype: u8 = 0;
        unsafe { kd_get_kbtype(fd, &mut kbtype) }
            .map_err(|e| io::Error::other(format!("not a virtual terminal: {e}")))?;

        // Read the current keyboard mode so we can put it back exactly.
        // If the call fails (unlikely on a real VT) we still restore to
        // a working default rather than leave the keyboard dead.
        let mut prev: CInt = K_XLATE;

        // SAFETY: `fd` is a live console device fd we just opened and
        // hold for the guard's whole life. Each ioctl gets an argument
        // of exactly the width linux/kd.h specifies (`c_int` by value,
        // or `*mut c_int` for the GET). We never expose the pointer and
        // `prev` outlives the call. These console ioctls only read/write
        // that one int and the kernel's per-VT mode — no other memory.
        unsafe {
            let _ = kd_get_kbmode(fd, &mut prev);
            // Order matters: silence the keyboard first, then blank the
            // text console. If the second call failed we'd still want
            // the keyboard restored, which Drop handles either way.
            kd_set_kbmode(fd, K_OFF).map_err(io::Error::from)?;
            kd_set_mode(fd, KD_GRAPHICS).map_err(io::Error::from)?;
        }

        Ok(Self { tty, prev_kb_mode: prev })
    }
}

impl Drop for VtGuard {
    fn drop(&mut self) {
        let fd = self.tty.as_raw_fd();
        // Best-effort restore: we're tearing down (possibly mid-panic),
        // so there's nothing useful to do with an error here — but
        // leaving the console in graphics mode with a dead keyboard
        // would strand the user, so we always try both.
        //
        // SAFETY: same invariants as `acquire` — `fd` is still the live
        // console fd owned by `self.tty`, valid until this `Drop` ends.
        unsafe {
            let _ = kd_set_kbmode(fd, self.prev_kb_mode);
            let _ = kd_set_mode(fd, KD_TEXT);
        }
    }
}

/// Open the controlling console, most-correct device first.
///
/// `/dev/tty0` is special: the kernel always maps it to *whichever* VT
/// is currently on screen, which is exactly the one fbcon is drawing on
/// and the one we need to silence. `/dev/console` and `/dev/tty1` are
/// fallbacks for unusual setups. We need read+write because the console
/// ioctls require a writable fd.
fn open_console() -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut last_err =
        io::Error::new(io::ErrorKind::NotFound, "no console device found");
    for path in ["/dev/tty0", "/dev/console", "/dev/tty1", "/dev/tty"] {
        if !Path::new(path).exists() {
            continue;
        }
        // O_NONBLOCK: opening certain tty devices can otherwise *block*
        // until a carrier/handshake — on real hardware that is a silent
        // hang with the screen frozen on the splash. The console ioctls
        // we issue don't care about the blocking flag, so non-blocking
        // open is free insurance. O_NOCTTY: never let this become our
        // controlling terminal (we are not a shell).
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOCTTY)
            .open(path)
        {
            Ok(f) => return Ok(f),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}
