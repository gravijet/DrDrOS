# DrDrOS

> A complete, minimal, fast, fully custom **userland operating system**
> built from scratch on top of the Linux kernel — in **Rust**.

DrDrOS replaces every part of the system a human ever sees or touches.
The shell, the editor, the file manager, the GUI framework, the network
protocol — **all original**, none borrowed.
The Linux kernel underneath handles only drivers, memory, and scheduling;
everything above it is ours.

| | |
|---|---|
| **Language** | Rust (memory-safe, fast, modern) |
| **Display** | Linux framebuffer (`/dev/fb0`) — no X11, no Wayland, no DE |
| **Storage** | Runs from RAM — no required disk writes |
| **Target** | x86_64 PCs from the last 20 years · VirtualBox · QEMU · Ventoy USB |
| **Status** | All 5 phases at Tier 1+ · **boots end-to-end in QEMU** — drdr-init → framebuffer splash → DrDrShell |

---

## Philosophy

- **Linux handles** drivers, hardware, memory, kernel — we never touch it.
- **DrDrOS handles** everything the user sees and uses.
- Every component is **written from scratch**. If `bash` / `vim` / `htop`
  already exists, we build our own.
- Every component name starts with **DrDr**.
- Boot fast. Use little. Look clean.

---

## Architecture

```
                  ┌──────────────────────────────────────────────────┐
                  │                  DrDrOS USERLAND                 │
                  │                                                  │
                  │   DrDrFiles      DrDrEdit       DrDrShell        │
                  │   ─────────      ────────       ─────────        │
                  │       │              │              │            │
                  │       └──────────────┼──────────────┘            │
                  │                      ▼                           │
                  │                   DrDrUI                         │
                  │            (windows · widgets · focus)           │
                  │       ┌──────────────┬──────────────┐            │
                  │       ▼              ▼              ▼            │
                  │   DrDrFont      framebuffer      DrDrNet         │
                  │  (bitmap font)   (/dev/fb0)    (binary proto)    │
                  │                                                  │
                  │                drdr-init  (PID 1)                │
                  └────────────────────────┬─────────────────────────┘
                                           │ Linux syscalls
                  ┌────────────────────────▼─────────────────────────┐
                  │              LINUX KERNEL (minimal)              │
                  │      drivers · memory · scheduler · fbdev        │
                  └────────────────────────┬─────────────────────────┘
                                           │
                  ┌────────────────────────▼─────────────────────────┐
                  │                 HARDWARE — x86_64                │
                  └──────────────────────────────────────────────────┘
```

---

## Components

| Crate / dir | Kind | Purpose |
|---|---|---|
| **drdr-init** | binary | PID 1 — boots the userland, draws the splash, launches DrDrShell |
| **drdr-shell** | binary | DrDrShell — custom shell with pipes, redirects, quoting |
| **drdr-edit** | binary | DrDrEdit — vi-style modal text editor; RAM-resident |
| **drdr-files** | binary | DrDrFiles — batch lister + interactive TUI file browser |
| **drdr-fb** | library | DrDrFb — direct framebuffer access (`/dev/fb0`) |
| **drdr-font** | library | DrDrFont — hand-drawn 8×16 bitmap glyph renderer |
| **drdr-ui** | library | DrDrUI — widgets (Label/Button/Frame/VBox/HBox), Theme |
| **drdr-tty** | library | DrDrTty — termios raw-mode + key decoder for terminal apps |
| **drdr-net** | library | DrDrNet — custom binary network protocol (not HTTP) |
| **buildroot/** | tooling | Buildroot config + BR2_EXTERNAL recipe for drdr-init |
| **iso/** | tooling | xorriso pipeline producing the bootable `drdros.iso` |
| **scripts/qemu.sh** | tooling | Boot the bzImage + rootfs.cpio.gz under QEMU |

---

## Roadmap

- [x] **Phase 1 — Foundation**
      Cargo workspace · Buildroot 2026.02.1 (built out-of-tree at
      `$HOME/.cache/drdros-buildroot`) · drdr-init Tier 2 (mounts + framebuffer
      splash) · drdr-fb primitives · drdr-font 8×16 bitmaps · BR2_EXTERNAL
      recipe wiring drdr-init as PID 1 · `scripts/qemu.sh` runner
- [x] **First boot** — boots end-to-end under QEMU: custom kernel
      (`linux-fb.config` fragment adds bochs-drm + fbdev emulation so
      `/dev/fb0` exists) → drdr-init mounts proc/sys/dev → paints the
      framebuffer splash → execs `/bin/drdr-shell` to an interactive prompt
- [x] **Phase 2 — Core applications**
      DrDrShell Tier 2 (pipes, redirects, quoting) · DrDrFiles Tier 2 (interactive TUI)
      · DrDrEdit Tier 2 (vi-style modal) · drdr-tty shared raw-mode helper
- [x] **Phase 3 — GUI framework**
      Tier 1 widgets (Label/Button/Frame/VBox/HBox + Theme) ·
      Tier 2 input (evdev KeyReader, KeyCode, focus model) · `drdr-demo` showcase
- [x] **Phase 4 — Network & protocols**
      DrDrNet Tier 1: length-prefixed binary frames + typed Encoder/Decoder ·
      Tier 2 (TCP server/client + correlation IDs) pending
- [x] **Phase 5 — Polish & ISO**
      `iso/build.sh` (grub-mkrescue hybrid ISO) · `scripts/qemu.sh --iso` ·
      DrDrTheme customisation + polish pass still ahead

---

## Building

```sh
# Userland: compile every Rust crate in the workspace.
cargo build --workspace

# Cross-compile drdr-init for the rootfs (musl, statically linked PIE).
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p drdr-init

# Kernel + initramfs: Buildroot out-of-tree at $HOME/.cache/drdros-buildroot
# (NTFS strips exec bits; ext4 is required for the cross-toolchain).
bash scripts/build-buildroot.sh             # ~15-30 min the first time
# → buildroot/images/{bzImage, rootfs.cpio.gz} (symlinks into the cache)

# Boot just the kernel + initramfs in QEMU (development loop).
bash scripts/qemu.sh             # GTK window + serial mirrored to stdio
bash scripts/qemu.sh --headless  # serial-only
bash scripts/qemu.sh --kvm       # add KVM acceleration if /dev/kvm exists

# Or wrap everything into a bootable hybrid ISO and boot that.
bash iso/build.sh                # → iso/drdros.iso
bash scripts/qemu.sh --iso       # boots via GRUB just like real hardware
```

## Running the core apps on the host

DrDrShell / DrDrEdit / DrDrFiles are happy on a regular Linux box —
useful for trying them out before the QEMU pipeline is fully wired.

```sh
cargo run -q -p drdr-shell                     # interactive REPL
cargo run -q -p drdr-files -- -a /tmp          # list /tmp incl. dotfiles
cargo run -q -p drdr-edit  -- notes.txt        # ed-style line editor
```

---

## License

Dual-licensed under **MIT OR Apache-2.0** — pick whichever fits your project.

---

*Built by [@gravijet](https://github.com/gravijet) and Claude.*
<span style="background:#000;color:#000;cursor:pointer;" 
  onclick="this.style.color='#fff'">
  Claude did basically everything.
</span>
