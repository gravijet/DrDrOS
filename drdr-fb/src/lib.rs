//! drdr-fb — direct framebuffer access for DrDrOS.
//!
//! Linux exposes the screen as a character device at /dev/fb0. We:
//!   1. open() the device to get a file descriptor.
//!   2. ioctl() the driver to learn the resolution, bit depth, pitch,
//!      **and the exact bit layout of a pixel** (red/green/blue offsets
//!      and lengths).
//!   3. mmap() the pixel memory into our process so we can paint by
//!      writing bytes to a slice — no read()/write() round-trips needed.
//!
//! Why the bit layout matters (the real-hardware lesson)
//! ─────────────────────────────────────────────────────
//! For a long time this file assumed every framebuffer was 32 bits per
//! pixel in `B G R A` byte order — true under QEMU's bochs-drm, and the
//! only path `put_pixel` would take (anything else silently drew
//! *nothing*). On real machines that assumption breaks: a UEFI machine
//! such as a Microsoft Surface boots on **efifb / simpledrm**, whose
//! pixel format comes straight from the firmware's GOP — frequently
//! 32bpp but `X R G B`, sometimes 16bpp `5-6-5`, sometimes 24bpp packed.
//! Drawing with the wrong layout means a blank or garbage screen that
//! looks exactly like a hang.
//!
//! The fix is to stop hard-coding the layout and instead *read it* from
//! `FBIOGET_VSCREENINFO` (the `red`/`green`/`blue` [`FbBitfield`]s) and
//! encode every pixel for whatever the panel actually wants. The
//! in-memory back buffer stays a fixed canonical 32bpp `BGRA` so the
//! renderer is simple; [`Framebuffer::present`] converts to the device
//! format once per frame (with a `memcpy` fast path when they already
//! match, which is the QEMU case so nothing there got slower).

use std::fs::OpenOptions;
use std::io;
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};

// ─── Linux framebuffer structs (mirror linux/fb.h) ───────────────────
// `#[repr(C)]` forces the same field layout a C compiler would produce
// on this platform, so when the kernel writes through our pointer via
// ioctl() the bytes land in the fields we expect.

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct FbFixScreeninfo {
    id: [u8; 16],
    smem_start: u64,       // physical address (kernel use)
    smem_len: u32,         // total framebuffer bytes
    fb_type: u32,          // packed pixels, planes, etc.
    type_aux: u32,
    visual: u32,           // truecolor, pseudocolor, ...
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    // 2 bytes of automatic alignment padding here
    line_length: u32,      // bytes per row — THE field we care about
    // 4 bytes of automatic alignment padding here on 64-bit
    mmio_start: u64,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

// ─── ioctl wrappers ──────────────────────────────────────────────────
// The framebuffer driver predates Linux's newer ioctl numbering scheme,
// so we hard-code the request numbers via the `*_bad!` macro variants
// rather than computing them from (type, nr, data).
nix::ioctl_read_bad!(fb_get_vinfo, 0x4600, FbVarScreeninfo); // FBIOGET_VSCREENINFO
nix::ioctl_read_bad!(fb_get_finfo, 0x4602, FbFixScreeninfo); // FBIOGET_FSCREENINFO

// ─── Public API ──────────────────────────────────────────────────────

/// A 32-bit RGBA color. Alpha is carried through but only blended by
/// [`Framebuffer::blend_pixel`]; `put_pixel` writes opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pixel {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Pixel {
    /// Opaque RGB pixel.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// RGBA pixel with custom alpha.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Mix `self` over `bg` by `self.a` (0 = bg, 255 = self). Used by the
    /// drop-shadow / translucency passes in the window manager.
    pub fn over(self, bg: Pixel) -> Pixel {
        let a = self.a as u32;
        let ia = 255 - a;
        let mix = |f: u8, b: u8| ((f as u32 * a + b as u32 * ia) / 255) as u8;
        Pixel::rgb(mix(self.r, bg.r), mix(self.g, bg.g), mix(self.b, bg.b))
    }

    /// Linear interpolate toward `other` by `t` in 0..=255 (for gradients).
    pub fn lerp(self, other: Pixel, t: u8) -> Pixel {
        let t = t as u32;
        let it = 255 - t;
        let m = |a: u8, b: u8| ((a as u32 * it + b as u32 * t) / 255) as u8;
        Pixel::rgb(m(self.r, other.r), m(self.g, other.g), m(self.b, other.b))
    }

    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
    pub const RED:   Self = Self::rgb(255, 0, 0);
    pub const GREEN: Self = Self::rgb(0, 255, 0);
    pub const BLUE:  Self = Self::rgb(0, 0, 255);
}

/// How one pixel is laid out in device memory, learned from the driver.
///
/// `bytes` is the stride of a single pixel (2, 3, or 4). The three
/// `(offset, length)` pairs say which bits of the little-endian value
/// hold each channel — exactly the `FBIOGET_VSCREENINFO` bitfields. We
/// derive a shift+mask once and reuse it for every pixel so the hot path
/// is a couple of shifts and an OR, no per-pixel branching on the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFmt {
    pub bpp: u32,
    pub bytes: usize,
    r_off: u32,
    r_len: u32,
    g_off: u32,
    g_len: u32,
    b_off: u32,
    b_len: u32,
    a_off: u32,
    a_len: u32,
}

impl PixelFmt {
    /// The canonical layout of the heap back buffer and of QEMU's
    /// bochs-drm: 32bpp, little-endian `0xAARRGGBB` (bytes `B G R A`).
    pub const CANONICAL: PixelFmt = PixelFmt {
        bpp: 32,
        bytes: 4,
        r_off: 16, r_len: 8,
        g_off: 8,  g_len: 8,
        b_off: 0,  b_len: 8,
        a_off: 24, a_len: 8,
    };

    fn from_vinfo(v: &FbVarScreeninfo) -> Self {
        // A correctly-populated truecolor driver fills the bitfields.
        // Some minimal firmwares leave them zeroed; fall back to a sane
        // default for the reported depth so we still draw *something*.
        let zeroed = v.red.length == 0 && v.green.length == 0 && v.blue.length == 0;
        if zeroed {
            return match v.bits_per_pixel {
                16 => PixelFmt { bpp: 16, bytes: 2, r_off: 11, r_len: 5, g_off: 5, g_len: 6, b_off: 0, b_len: 5, a_off: 0, a_len: 0 },
                24 => PixelFmt { bpp: 24, bytes: 3, r_off: 16, r_len: 8, g_off: 8, g_len: 8, b_off: 0, b_len: 8, a_off: 0, a_len: 0 },
                _  => PixelFmt::CANONICAL,
            };
        }
        PixelFmt {
            bpp: v.bits_per_pixel,
            bytes: (v.bits_per_pixel.max(8) / 8) as usize,
            r_off: v.red.offset,   r_len: v.red.length,
            g_off: v.green.offset, g_len: v.green.length,
            b_off: v.blue.offset,  b_len: v.blue.length,
            a_off: v.transp.offset, a_len: v.transp.length,
        }
    }

    /// Pack an RGBA pixel into this device's little-endian word. Channels
    /// are scaled from 8-bit down to the field width (e.g. 8→5 for 565).
    #[inline]
    fn encode(&self, p: Pixel) -> u32 {
        #[inline]
        fn chan(v: u8, len: u32, off: u32) -> u32 {
            if len == 0 {
                return 0;
            }
            // Scale 8-bit channel to `len` bits (round by truncation).
            let scaled = (v as u32) >> (8 - len.min(8));
            scaled << off
        }
        chan(p.r, self.r_len, self.r_off)
            | chan(p.g, self.g_len, self.g_off)
            | chan(p.b, self.b_len, self.b_off)
            | chan(p.a, self.a_len, self.a_off)
    }

    /// True when this layout is byte-identical to the heap back buffer,
    /// so a whole row can be `memcpy`'d instead of converted per pixel.
    fn is_canonical(&self) -> bool {
        self.bpp == 32
            && self.r_off == 16 && self.g_off == 8 && self.b_off == 0
            && self.r_len == 8 && self.g_len == 8 && self.b_len == 8
    }
}

/// Where the pixel bytes live. `Mmap` is the production case; `Heap` is a
/// Vec-backed buffer used by host tests, the `write_ppm` snapshot path,
/// and as the double-buffer back surface.
enum Backend {
    Mmap {
        _fd: OwnedFd,
        ptr: NonNull<u8>,
        len: usize,
    },
    Heap(Vec<u8>),
}

/// A pixel surface. [`Framebuffer::open`] mmaps a real device and learns
/// its [`PixelFmt`]; [`Framebuffer::in_memory`] is a canonical 32bpp
/// heap buffer that the renderer always targets.
pub struct Framebuffer {
    backend: Backend,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bits per pixel as reported by the driver.
    pub bpp: u32,
    /// Bytes per row — may exceed `width * bytes/px` due to padding.
    pub pitch: u32,
    /// Byte offset of pixel (0,0): `yoffset*pitch + xoffset*bytes`. Zero
    /// on efifb/bochs but non-zero if the driver pans the visible window.
    origin: usize,
    /// The device's exact pixel layout (canonical for heap buffers).
    fmt: PixelFmt,
}

impl Framebuffer {
    /// Open a framebuffer device (usually `/dev/fb0`).
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let fd: OwnedFd = file.into();

        let mut vinfo = FbVarScreeninfo::default();
        let mut finfo = FbFixScreeninfo::default();

        // SAFETY: valid, properly aligned mutable pointers to structs
        // whose `#[repr(C)]` layout matches the kernel's. ioctl() fills
        // them and returns 0, or returns a negative errno.
        unsafe {
            fb_get_vinfo(fd.as_raw_fd(), &mut vinfo).map_err(io::Error::from)?;
            fb_get_finfo(fd.as_raw_fd(), &mut finfo).map_err(io::Error::from)?;
        }

        let fmt = PixelFmt::from_vinfo(&vinfo);
        // mmap the *whole* device (smem_len) when the driver reports it —
        // efifb's visible yres can be shorter than the allocated memory,
        // and panning relies on the extra rows being mapped.
        let by_line = (finfo.line_length as usize)
            .checked_mul(vinfo.yres_virtual.max(vinfo.yres) as usize)
            .ok_or_else(|| io::Error::other("framebuffer size overflow"))?;
        let map_len = (finfo.smem_len as usize).max(by_line).max(1);
        let map_len_nz = NonZeroUsize::new(map_len)
            .ok_or_else(|| io::Error::other("framebuffer size is zero"))?;

        // SAFETY: a shared read+write mapping of the device, offset 0,
        // `map_len` bytes, valid until munmap in Drop. `fd` is held in
        // `_fd` so it can't be closed early.
        let map_ptr = unsafe {
            mmap(
                None,
                map_len_nz,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )
        }
        .map_err(io::Error::from)?;

        let origin = (vinfo.yoffset as usize) * (finfo.line_length as usize)
            + (vinfo.xoffset as usize) * fmt.bytes;

        Ok(Self {
            backend: Backend::Mmap {
                _fd: fd,
                ptr: map_ptr.cast(),
                len: map_len,
            },
            width: vinfo.xres,
            height: vinfo.yres,
            bpp: vinfo.bits_per_pixel,
            pitch: finfo.line_length,
            origin,
            fmt,
        })
    }

    /// Create a heap-backed canonical 32bpp `width × height` surface.
    /// Same API as a real `/dev/fb0` — used for unit tests, snapshots,
    /// and as the double-buffer back surface.
    pub fn in_memory(width: u32, height: u32) -> Self {
        let pitch = width.saturating_mul(4).max(4);
        let bytes = (pitch as usize).saturating_mul(height.max(1) as usize);
        Self {
            backend: Backend::Heap(vec![0u8; bytes]),
            width,
            height,
            bpp: 32,
            pitch,
            origin: 0,
            fmt: PixelFmt::CANONICAL,
        }
    }

    /// A human-readable one-liner for boot diagnostics — printed by
    /// drdr-init so a misdetected pixel format is visible on the console
    /// instead of looking like a freeze.
    pub fn describe(&self) -> String {
        format!(
            "{}x{} {}bpp pitch={} R{}@{} G{}@{} B{}@{} A{}@{}{}",
            self.width, self.height, self.bpp, self.pitch,
            self.fmt.r_len, self.fmt.r_off,
            self.fmt.g_len, self.fmt.g_off,
            self.fmt.b_len, self.fmt.b_off,
            self.fmt.a_len, self.fmt.a_off,
            if self.fmt.is_canonical() { " (canonical)" } else { " (converted)" },
        )
    }

    /// Write the framebuffer's contents to `path` as a binary PPM (P6).
    /// Reads the canonical heap layout, so it is only meaningful for an
    /// [`in_memory`](Self::in_memory) surface (the snapshot path).
    pub fn write_ppm(&self, path: impl AsRef<Path>) -> io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(path)?;
        writeln!(file, "P6")?;
        writeln!(file, "{} {}", self.width, self.height)?;
        writeln!(file, "255")?;

        let buf = self.buffer_ro();
        let pitch = self.pitch as usize;
        let mut row = Vec::with_capacity(self.width as usize * 3);
        for y in 0..self.height as usize {
            row.clear();
            for x in 0..self.width as usize {
                let i = y * pitch + x * 4;
                let (b, g, r) = (buf[i], buf[i + 1], buf[i + 2]);
                row.extend_from_slice(&[r, g, b]);
            }
            file.write_all(&row)?;
        }
        Ok(())
    }

    /// Set a single pixel. Out-of-bounds coordinates are silently ignored.
    /// Encoded for the device's real layout (works on 16/24/32bpp,
    /// RGB or BGR, scaled channels for 565).
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Pixel) {
        if x >= self.width || y >= self.height {
            return;
        }
        let (pitch, fmt, origin) = (self.pitch as usize, self.fmt, self.origin);
        let off = origin + (y as usize) * pitch + (x as usize) * fmt.bytes;
        let word = fmt.encode(color);
        let bytes = word.to_le_bytes();
        let buf = self.buffer();
        if off + fmt.bytes <= buf.len() {
            buf[off..off + fmt.bytes].copy_from_slice(&bytes[..fmt.bytes]);
        }
    }

    /// Read one pixel back as canonical RGBA. Only correct for the
    /// canonical heap surface (used by translucency passes that composite
    /// onto the back buffer); on a device surface it best-effort decodes
    /// the low bytes and is not used in hot paths.
    pub fn get_pixel(&self, x: u32, y: u32) -> Pixel {
        if x >= self.width || y >= self.height {
            return Pixel::BLACK;
        }
        let pitch = self.pitch as usize;
        let buf = self.buffer_ro();
        let i = self.origin + (y as usize) * pitch + (x as usize) * self.fmt.bytes;
        if i + 4 <= buf.len() && self.fmt.is_canonical() {
            return Pixel::rgb(buf[i + 2], buf[i + 1], buf[i]);
        }
        Pixel::BLACK
    }

    /// Alpha-composite `color` over whatever is already there. Used for
    /// soft shadows; only meaningful on the canonical heap back buffer.
    pub fn blend_pixel(&mut self, x: u32, y: u32, color: Pixel) {
        if color.a == 255 {
            self.put_pixel(x, y, color);
            return;
        }
        if color.a == 0 {
            return;
        }
        let bg = self.get_pixel(x, y);
        self.put_pixel(x, y, color.over(bg));
    }

    /// Fill an axis-aligned rectangle. Coordinates outside the screen
    /// are clipped, not rejected.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Pixel) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        for py in y..y_end {
            for px in x..x_end {
                self.put_pixel(px, py, color);
            }
        }
    }

    /// Fill a rectangle with a vertical gradient from `top` to `bottom`
    /// (used for the modern title bars and the desktop wallpaper).
    pub fn fill_rect_v(&mut self, x: u32, y: u32, w: u32, h: u32, top: Pixel, bottom: Pixel) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        if y_end <= y {
            return;
        }
        let span = (y_end - y).max(1);
        for py in y..y_end {
            let t = (((py - y) as u32 * 255) / span) as u8;
            let c = top.lerp(bottom, t);
            for px in x..x_end {
                self.put_pixel(px, py, c);
            }
        }
    }

    /// Translucent rectangle (soft shadows / scrims) — alpha-composited
    /// per pixel onto the back buffer.
    pub fn shade_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Pixel) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        for py in y..y_end {
            for px in x..x_end {
                self.blend_pixel(px, py, color);
            }
        }
    }

    /// Fill the entire screen with a single color.
    pub fn clear(&mut self, color: Pixel) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Copy a whole frame from a canonical `src` onto this surface — the
    /// *present* step of double buffering.
    ///
    /// When the device format is byte-identical to the back buffer (QEMU,
    /// and many efifb panels) each row is a single `copy_from_slice`. When
    /// it differs (16/24bpp, RGB-order efifb on real machines like a
    /// Surface) we convert pixel by pixel into the device layout — slower
    /// but *correct*, which is the difference between a desktop and a
    /// screen that looks frozen.
    pub fn present(&mut self, src: &Framebuffer) {
        let w = self.width.min(src.width) as usize;
        let h = self.height.min(src.height) as usize;
        let src_pitch = src.pitch as usize;
        let dst_pitch = self.pitch as usize;
        let origin = self.origin;
        let fmt = self.fmt;

        if fmt.is_canonical() {
            let row_bytes = w * 4;
            let src_buf = src.buffer_ro();
            let dst_buf = self.buffer();
            for y in 0..h {
                let s = y * src_pitch;
                let d = origin + y * dst_pitch;
                if s + row_bytes <= src_buf.len() && d + row_bytes <= dst_buf.len() {
                    dst_buf[d..d + row_bytes].copy_from_slice(&src_buf[s..s + row_bytes]);
                }
            }
            return;
        }

        // Format-converting present. `src` and `self` are distinct
        // objects, so an immutable borrow of one and a mutable borrow of
        // the other coexist fine. Re-encode each canonical BGRA pixel
        // into the device layout.
        let bytes = fmt.bytes;
        let src_buf = src.buffer_ro();
        let dst_buf = self.buffer();
        for y in 0..h {
            let s = y * src_pitch;
            let d = origin + y * dst_pitch;
            for x in 0..w {
                let i = s + x * 4;
                let o = d + x * bytes;
                if i + 4 > src_buf.len() || o + bytes > dst_buf.len() {
                    break;
                }
                let px = Pixel::rgb(src_buf[i + 2], src_buf[i + 1], src_buf[i]);
                let word = fmt.encode(px).to_le_bytes();
                dst_buf[o..o + bytes].copy_from_slice(&word[..bytes]);
            }
        }
    }

    /// Mutable view over the framebuffer bytes — mmap or heap.
    fn buffer(&mut self) -> &mut [u8] {
        match &mut self.backend {
            // SAFETY: `ptr` came from mmap with `len` bytes of valid,
            // writable, kernel-shared memory; the mapping outlives this
            // borrow (tied to `self`); `&mut self` rules out aliasing.
            Backend::Mmap { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(ptr.as_ptr(), *len)
            },
            Backend::Heap(v) => v.as_mut_slice(),
        }
    }

    /// Read-only view, for snapshot / encode / present paths.
    fn buffer_ro(&self) -> &[u8] {
        match &self.backend {
            // SAFETY: same invariants as `buffer`; `&self` suffices for a
            // shared slice.
            Backend::Mmap { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts(ptr.as_ptr(), *len)
            },
            Backend::Heap(v) => v.as_slice(),
        }
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        if let Backend::Mmap { ptr, len, .. } = &self.backend {
            // SAFETY: we unmap exactly the region created in `open`. No
            // method runs on `self` afterwards, so the now-dangling
            // pointer is never read again.
            unsafe {
                let _ = munmap(ptr.cast(), *len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_roundtrips_through_ppm_bytes() {
        let mut fb = Framebuffer::in_memory(3, 2);
        fb.put_pixel(0, 0, Pixel::rgb(10, 20, 30));
        fb.put_pixel(2, 1, Pixel::rgb(200, 100, 50));
        assert_eq!(fb.get_pixel(0, 0), Pixel::rgb(10, 20, 30));
        assert_eq!(fb.get_pixel(2, 1), Pixel::rgb(200, 100, 50));
        assert_eq!(fb.get_pixel(1, 0), Pixel::BLACK);
    }

    #[test]
    fn encode_565_scales_channels() {
        let f = PixelFmt {
            bpp: 16, bytes: 2,
            r_off: 11, r_len: 5, g_off: 5, g_len: 6, b_off: 0, b_len: 5,
            a_off: 0, a_len: 0,
        };
        // Pure white → all field bits set: 0b11111_111111_11111 = 0xFFFF.
        assert_eq!(f.encode(Pixel::WHITE), 0xFFFF);
        // Pure red → top 5 bits only.
        assert_eq!(f.encode(Pixel::RED), 0b11111 << 11);
        // Pure blue → low 5 bits only.
        assert_eq!(f.encode(Pixel::BLUE), 0b11111);
    }

    #[test]
    fn rgb_order_efifb_is_not_canonical_but_encodes() {
        // efifb on some firmware: 32bpp but R at 0, B at 16 (X B G R is
        // wrong; this is R G B X). Must NOT take the memcpy fast path.
        let f = PixelFmt {
            bpp: 32, bytes: 4,
            r_off: 0, r_len: 8, g_off: 8, g_len: 8, b_off: 16, b_len: 8,
            a_off: 24, a_len: 8,
        };
        assert!(!f.is_canonical());
        // Red lands in the low byte for this layout.
        assert_eq!(f.encode(Pixel::RED) & 0xFF, 0xFF);
        assert_eq!(f.encode(Pixel::BLUE) >> 16 & 0xFF, 0xFF);
    }

    #[test]
    fn from_vinfo_falls_back_when_bitfields_are_zeroed() {
        let mut v = FbVarScreeninfo::default();
        v.bits_per_pixel = 16;
        let f = PixelFmt::from_vinfo(&v);
        assert_eq!((f.bpp, f.bytes), (16, 2));
        assert_eq!((f.g_off, f.g_len), (5, 6)); // assumed 565
    }

    #[test]
    fn present_converts_when_formats_differ() {
        let mut src = Framebuffer::in_memory(2, 1);
        src.put_pixel(0, 0, Pixel::RED);
        src.put_pixel(1, 0, Pixel::BLUE);

        // A fake 16bpp 565 device backed by heap so we can inspect bytes.
        let mut dev = Framebuffer::in_memory(2, 1);
        dev.fmt = PixelFmt {
            bpp: 16, bytes: 2,
            r_off: 11, r_len: 5, g_off: 5, g_len: 6, b_off: 0, b_len: 5,
            a_off: 0, a_len: 0,
        };
        dev.bpp = 16;
        dev.present(&src);
        let buf = dev.buffer_ro();
        // pixel 0 = red = 0xF800, pixel 1 = blue = 0x001F (LE bytes).
        assert_eq!(&buf[0..2], &0xF800u16.to_le_bytes());
        assert_eq!(&buf[2..4], &0x001Fu16.to_le_bytes());
    }

    #[test]
    fn out_of_bounds_is_ignored() {
        let mut fb = Framebuffer::in_memory(4, 4);
        fb.put_pixel(99, 99, Pixel::WHITE); // must not panic
        assert_eq!(fb.get_pixel(0, 0), Pixel::BLACK);
    }
}
