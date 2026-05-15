//! drdr-init::net — bring the loopback interface up at boot.
//!
//! Why PID 1 has to do this
//! ────────────────────────
//! The Linux kernel always *creates* the loopback device `lo`, but it
//! leaves it administratively **DOWN** with no address. On a normal
//! distro some early service (ifupdown, systemd-networkd, `ip link set
//! lo up`) flips it on. DrDrOS has none of those — we are the whole
//! userland — so until *we* bring `lo` up, `127.0.0.1` is unreachable
//! and every DrDrNet TCP call fails with `ENETUNREACH` /
//! `EADDRNOTAVAIL`. The DrDrDesk "DrDrNet" status window talks to a
//! local reactor server over loopback, so this has to happen at boot.
//!
//! How an interface is configured without `ip`/`ifconfig`
//! ─────────────────────────────────────────────────────
//! Those tools are just thin wrappers around `ioctl(2)` on a socket.
//! Configuring an interface = three ioctls, each handed a `struct
//! ifreq` (interface name + one payload field):
//!
//!   - `SIOCSIFADDR`    — set the IPv4 address      (127.0.0.1)
//!   - `SIOCSIFNETMASK` — set the netmask           (255.0.0.0)
//!   - `SIOCSIFFLAGS`   — set the flags             (add IFF_UP)
//!
//! We don't copy `ifconfig`; we make the same kernel calls it would.
//!
//! The single block of `unsafe` in DrDrOS-the-init lives here: `nix`
//! exposes no safe wrapper for these `SIOC*` requests, so we call
//! `libc::socket`/`libc::ioctl` directly. Each call's invariant is
//! documented inline. Everything is best-effort: if it fails we log and
//! continue booting (a machine with no network is still a usable
//! desktop), exactly like the framebuffer splash.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// `struct ifreq` is `char ifr_name[16]` followed by a union. On x86_64
// `sizeof(struct ifreq) == 40`. We never use `libc::ifreq` directly:
// its union is expressed through version-specific accessor methods that
// differ between libc releases and between glibc/musl. A fixed 40-byte
// block with the fields written at their stable offsets (name at 0, the
// union at 16) is simpler, target-agnostic, and easy to audit.
const IFREQ_LEN: usize = 40;
const IFNAMSIZ: usize = 16;

// Request numbers from <linux/sockios.h>. These are ABI constants —
// identical on every Linux arch/libc — so we spell them out rather than
// depend on whether the `libc` crate re-exports them for this target.
const SIOCSIFADDR: libc::Ioctl = 0x8916;
const SIOCSIFFLAGS: libc::Ioctl = 0x8914;
const SIOCGIFFLAGS: libc::Ioctl = 0x8913;
const SIOCSIFNETMASK: libc::Ioctl = 0x891C;

// Interface flag bits from <net/if.h>.
const IFF_UP: i16 = 0x1;
const IFF_RUNNING: i16 = 0x40;

/// Build a zeroed `ifreq` block with `ifr_name` filled in.
fn ifreq(name: &str) -> [u8; IFREQ_LEN] {
    let mut b = [0u8; IFREQ_LEN];
    let n = name.as_bytes();
    let len = n.len().min(IFNAMSIZ - 1); // leave room for the NUL
    b[..len].copy_from_slice(&n[..len]);
    b
}

/// Overlay a `struct sockaddr_in { family, port, addr, zero[8] }` onto
/// the `ifreq` union (offset [`IFNAMSIZ`]). `ip` is the dotted-quad in
/// network byte order, which for a `[u8;4]` is just the bytes in order.
fn put_addr(buf: &mut [u8; IFREQ_LEN], ip: [u8; 4]) {
    let fam = (libc::AF_INET as u16).to_ne_bytes(); // sa_family_t: host order
    buf[16..18].copy_from_slice(&fam);
    buf[18..20].copy_from_slice(&0u16.to_be_bytes()); // sin_port = 0
    buf[20..24].copy_from_slice(&ip); // sin_addr (already net order)
    // sin_zero[8] stays zeroed.
}

/// One ioctl against `fd` with the `ifreq` block.
fn ioctl(fd: &OwnedFd, request: libc::Ioctl, buf: &mut [u8; IFREQ_LEN]) -> io::Result<()> {
    // SAFETY: `fd` is a live AF_INET socket we own. `buf` is exactly
    // IFREQ_LEN (40) bytes — the size of `struct ifreq` on x86_64 — and
    // for every SIOC* request used here the kernel only reads/writes
    // within that block. `ioctl` returns <0 on error, leaving errno set,
    // which `last_os_error` reads.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), request, buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Configure `lo` with 127.0.0.1/8 and mark it UP+RUNNING. Returns the
/// first ioctl error encountered; callers treat failure as non-fatal.
pub fn bring_up_loopback() -> io::Result<()> {
    // A datagram socket is just the handle ioctl needs — we never send
    // on it; it's the conventional way to reach the interface ioctls.
    // SAFETY: plain socket(2); we check the return and immediately wrap
    // the fd in OwnedFd so it is closed on every exit path.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh, valid fd we just created and own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    let mut req = ifreq("lo");
    put_addr(&mut req, [127, 0, 0, 1]);
    ioctl(&fd, SIOCSIFADDR, &mut req)?;

    let mut req = ifreq("lo");
    put_addr(&mut req, [255, 0, 0, 0]);
    ioctl(&fd, SIOCSIFNETMASK, &mut req)?;

    // Read-modify-write the flags so we only *add* UP/RUNNING and don't
    // clobber anything the kernel already set on the device.
    let mut req = ifreq("lo");
    ioctl(&fd, SIOCGIFFLAGS, &mut req)?;
    let mut flags = i16::from_ne_bytes([req[16], req[17]]);
    flags |= IFF_UP | IFF_RUNNING;
    req[16..18].copy_from_slice(&flags.to_ne_bytes());
    ioctl(&fd, SIOCSIFFLAGS, &mut req)?;

    Ok(())
}
