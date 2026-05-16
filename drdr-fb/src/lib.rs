//! drdr-ui::fb — direct framebuffer access for DrDrOS (Phase 1).
//!
//! Linux exposes the screen as a character device at /dev/fb0. We:
//!   1. open() the device to get a file descriptor.
//!   2. ioctl() the driver to learn the resolution, bit depth, and pitch.
//!   3. mmap() the pixel memory into our process so we can paint by
//!      writing bytes to a slice — no read()/write() round-trips needed.
//!
//! Pixel memory layout at 32 bits-per-pixel (the common QEMU + x86 case):
//! each pixel is 4 bytes ordered B G R A in little-endian. Pixel (x, y)
//! lives at byte offset
//!     y * pitch + x * (bpp / 8)
//! where `pitch` (a.k.a. `line_length`) is the bytes-per-row — sometimes
//! larger than `width * 4` if the driver pads each row for alignment.

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
//
// Each macro expands to:
//     unsafe fn name(fd: RawFd, data: *mut T) -> nix::Result<i32>
nix::ioctl_read_bad!(fb_get_vinfo, 0x4600, FbVarScreeninfo); // FBIOGET_VSCREENINFO
nix::ioctl_read_bad!(fb_get_finfo, 0x4602, FbFixScreeninfo); // FBIOGET_FSCREENINFO

// ─── Public API ──────────────────────────────────────────────────────

/// A 32-bit RGBA color. The `a` (alpha) channel is currently stored as-is
/// but not blended — `put_pixel` writes it directly into the framebuffer's
/// alpha slot. Real alpha blending lands in Phase 3.
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

    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
    pub const RED:   Self = Self::rgb(255, 0, 0);
    pub const GREEN: Self = Self::rgb(0, 255, 0);
    pub const BLUE:  Self = Self::rgb(0, 0, 255);
}

/// Where the pixel bytes live. `Mmap` is the production case — the
/// kernel exposes /dev/fb0 as memory we map directly. `Heap` is a
/// Vec-backed buffer used by host-side tests and the `write_ppm`
/// snapshot path — no kernel, no framebuffer device required.
enum Backend {
    Mmap {
        _fd: OwnedFd,
        ptr: NonNull<u8>,
        len: usize,
    },
    Heap(Vec<u8>),
}

/// A pixel surface. Production constructor [`Framebuffer::open`] mmaps
/// /dev/fb0; [`Framebuffer::in_memory`] backs the same API with a
/// heap-allocated buffer so widget tests can run anywhere.
pub struct Framebuffer {
    backend: Backend,

    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bits per pixel (typically 32 in QEMU and most real hardware).
    pub bpp: u32,
    /// Bytes per row — may exceed `width * bpp/8` due to driver padding.
    pub pitch: u32,
}

impl Framebuffer {
    /// Open a framebuffer device (usually `/dev/fb0`).
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let fd: OwnedFd = file.into();

        let mut vinfo = FbVarScreeninfo::default();
        let mut finfo = FbFixScreeninfo::default();

        // SAFETY: we pass valid, properly aligned mutable pointers to
        // structs whose `#[repr(C)]` layout matches the kernel's. ioctl()
        // either fills them and returns 0, or returns a negative errno.
        unsafe {
            fb_get_vinfo(fd.as_raw_fd(), &mut vinfo).map_err(io::Error::from)?;
            fb_get_finfo(fd.as_raw_fd(), &mut finfo).map_err(io::Error::from)?;
        }

        let map_len = (finfo.line_length as usize)
            .checked_mul(vinfo.yres as usize)
            .ok_or_else(|| io::Error::other("framebuffer size overflow"))?;
        let map_len_nz = NonZeroUsize::new(map_len)
            .ok_or_else(|| io::Error::other("framebuffer size is zero"))?;

        // SAFETY: we create a shared read+write mapping of /dev/fb0,
        // offset 0, length `map_len` bytes. mmap returns a valid pointer
        // to that range (or fails). The mapping lives until munmap in
        // Drop. We hold `fd` in `_fd` so it can't be closed early from
        // outside.
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
        })
    }

    /// Create a heap-backed framebuffer of `width × height` pixels at
    /// 32 bits-per-pixel. Same API as a real /dev/fb0 — useful for unit
    /// tests, golden-image snapshots, and rendering previews on systems
    /// that don't have a framebuffer device.
    pub fn in_memory(width: u32, height: u32) -> Self {
        let pitch = width.saturating_mul(4);
        let bytes = (pitch as usize).saturating_mul(height as usize);
        Self {
            backend: Backend::Heap(vec![0u8; bytes]),
            width,
            height,
            bpp: 32,
            pitch,
        }
    }

    /// Write the framebuffer's contents to `path` as a binary PPM (P6)
    /// image. PPM is the simplest image format that any viewer (GIMP,
    /// ImageMagick, browsers via extension, Eye of GNOME) opens out of
    /// the box — handy for diffing renders without spinning up QEMU.
    pub fn write_ppm(&self, path: impl AsRef<Path>) -> io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(path)?;
        // Header: magic + WxH + max channel value.
        writeln!(file, "P6")?;
        writeln!(file, "{} {}", self.width, self.height)?;
        writeln!(file, "255")?;

        // Body: one RGB triplet per pixel, top-left to bottom-right.
        // Source layout is 32bpp BGRA; we transpose to RGB on the fly.
        let buf = self.buffer_ro();
        let pitch = self.pitch as usize;
        let mut row = Vec::with_capacity(self.width as usize * 3);
        for y in 0..self.height as usize {
            row.clear();
            for x in 0..self.width as usize {
                let i = y * pitch + x * 4;
                let b = buf[i];
                let g = buf[i + 1];
                let r = buf[i + 2];
                row.extend_from_slice(&[r, g, b]);
            }
            file.write_all(&row)?;
        }
        Ok(())
    }

    /// Set a single pixel. Out-of-bounds coordinates are silently ignored.
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Pixel) {
        if x >= self.width || y >= self.height {
            return;
        }
        // For Phase 1 we only support 32bpp BGRA, which covers QEMU
        // and virtually every modern x86 display. 16/24bpp paths land later.
        if self.bpp != 32 {
            return;
        }
        let offset = (y as usize) * (self.pitch as usize) + (x as usize) * 4;
        let buf = self.buffer();
        // Little-endian 32bpp framebuffer byte order: B G R A.
        buf[offset]     = color.b;
        buf[offset + 1] = color.g;
        buf[offset + 2] = color.r;
        buf[offset + 3] = color.a;
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

    /// Fill the entire screen with a single color.
    pub fn clear(&mut self, color: Pixel) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Copy a whole frame from `src` onto this framebuffer in one pass —
    /// the *present* step of double buffering.
    ///
    /// Why this exists: painting straight onto the mmap'd `/dev/fb0` lets
    /// the monitor scan out a half-drawn frame (cleared background, then
    /// windows appearing one by one) — that's the flicker. The cure is
    /// to render the whole scene into an off-screen [`in_memory`] buffer
    /// and then blit it here in a single tight copy, so the screen only
    /// ever shows complete frames.
    ///
    /// Both surfaces must be 32bpp (the only depth we render). The copy
    /// is row-by-row because the on-screen framebuffer's `pitch` (bytes
    /// per row) is often padded wider than `width * 4`, while the heap
    /// back buffer is exactly `width * 4` — the byte offsets don't line
    /// up, so a single flat `copy_from_slice` would shear the image.
    /// Mismatched sizes are clipped to the overlapping region.
    pub fn present(&mut self, src: &Framebuffer) {
        if self.bpp != 32 || src.bpp != 32 {
            return;
        }
        let w = self.width.min(src.width) as usize;
        let h = self.height.min(src.height) as usize;
        let row_bytes = w * 4;
        let dst_pitch = self.pitch as usize;
        let src_pitch = src.pitch as usize;
        let src_buf = src.buffer_ro();
        let dst_buf = self.buffer();
        for y in 0..h {
            let s = y * src_pitch;
            let d = y * dst_pitch;
            dst_buf[d..d + row_bytes].copy_from_slice(&src_buf[s..s + row_bytes]);
        }
    }

    /// Mutable view over the framebuffer bytes — mmap or heap.
    fn buffer(&mut self) -> &mut [u8] {
        match &mut self.backend {
            // SAFETY: `ptr` was returned by mmap with `len` bytes of
            // valid, writable memory shared with the kernel. The mapping
            // outlives this borrow because it's tied to `self`. `&mut
            // self` rules out aliased slices.
            Backend::Mmap { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(ptr.as_ptr(), *len)
            },
            Backend::Heap(v) => v.as_mut_slice(),
        }
    }

    /// Read-only view, for snapshot / encode paths.
    fn buffer_ro(&self) -> &[u8] {
        match &self.backend {
            // SAFETY: same invariants as `buffer` — but `&self` is enough
            // since we only hand out a `&[u8]`.
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
            // SAFETY: we unmap exactly the region we created in `open`.
            // After this point no method runs on `self`, so the now-
            // dangling pointer is never read again.
            unsafe {
                let _ = munmap(ptr.cast(), *len);
            }
            // _fd's own Drop closes the file descriptor.
        }
        // Heap backend has nothing to release — Vec drops itself.
    }
}
