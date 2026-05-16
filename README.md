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
| **Status** | All phases at Tier 2+ · **boots to a real window manager** — drdr-init (PID 1, sets hostname + brings up `lo`) → DrDrDesk: overlapping windows, mouse, a live DrDrNet panel backed by a Tier 3 async reactor |

### What you actually get when it boots

Power on a PC (or a VM) and a few seconds later you are in a graphical
desktop — no login, no shell, no X11:

- **A real window manager** — overlapping, draggable, titled windows
  with a hand-drawn mouse cursor. Alt-Tab to cycle, double-click to
  open, `[x]` to close, drag the title bar to move.
- **DrDrFiles** — browse the live filesystem; double-click a folder to
  enter it or a file to open it; create files (`n`) and delete them
  (`d`) with confirmation.
- **DrDrEdit** — a genuine in-window text editor: type, navigate, edit,
  save (no PTY, no terminal emulator — apps draw characters into a
  grid the WM blits).
- **Launcher** — reopen any window you closed; it comes back on its own
  if you close *everything*, so the desktop is never a dead end.
- **DrDrNet** — a live status panel speaking an original length-prefixed
  binary protocol over a hand-rolled single-thread epoll reactor (no
  tokio, no HTTP) across loopback TCP.
- **Owns the screen properly** — takes the Linux VT into graphics mode
  so the kernel text console can't fight it, double-buffers every
  frame (no flicker), and coalesces input so the cursor stays smooth.

Every pixel and keystroke above is handled by code in this repository.

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
                  │                  DrDrDesk                       │
                  │           (graphical session / launcher)         │
                  │       ┌──────────────┼──────────────┐            │
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
| **drdr-init** | binary | PID 1 — mounts, sets the hostname, brings `lo` up, draws the splash, then *supervises* (spawns + respawns) the graphical session |
| **drdr-desk** | binary | DrDrDesk — framebuffer **window manager**: overlapping windows, mouse + keyboard, in-window apps (About / DrDrFiles / System / DrDrNet) |
| **drdr-shell** | binary | DrDrShell — custom shell with pipes, redirects, quoting |
| **drdr-edit** | binary | DrDrEdit — vi-style modal text editor; RAM-resident |
| **drdr-files** | binary | DrDrFiles — batch lister + interactive TUI file browser |
| **drdr-fb** | library | DrDrFb — direct framebuffer access (`/dev/fb0`) |
| **drdr-font** | library | DrDrFont — hand-drawn 8×16 bitmap glyph renderer |
| **drdr-ui** | library | DrDrUI — widgets + Theme, the `TextGrid`/`WindowApp` surface, a stacking `WindowManager`, and an `InputHub` (poll over keyboard + mouse) |
| **drdr-tty** | library | DrDrTty — termios raw-mode + key decoder for terminal apps |
| **drdr-net** | library | DrDrNet — custom binary protocol (not HTTP): framing, codecs, sync TCP, **and a hand-rolled epoll reactor** (Tier 3 async) |
| **buildroot/** | tooling | Buildroot config + BR2_EXTERNAL recipe for drdr-init |
| **iso/** | tooling | xorriso pipeline producing the bootable `drdros.iso` |
| **scripts/qemu.sh** | tooling | Boot the bzImage + rootfs.cpio.gz under QEMU |

---

## Project stats

A snapshot of the **from-scratch userland only** — the Linux kernel and
the Buildroot tree are *not* counted, just code in this repo:

| Metric | Value |
|---|---|
| Rust source | **8,221 lines** across **19 files** |
| Workspace crates | **11** (every `drdr-*`) |
| Tests | **41** (unit + integration, `cargo test`) |
| Git commits | **36** |
| Tracked files (excl. `buildroot/`) | **42** |
| Development window | 2026-05-14 → 2026-05-16 |

Lines of Rust per crate:

| Crate | Lines | Crate | Lines |
|---|--:|---|--:|
| drdr-ui (GUI framework, WM)     | 2172 | drdr-init (PID 1)        | 421 |
| drdr-net (binary proto + reactor)| 1518 | drdr-fb (framebuffer)    | 374 |
| drdr-desk (window manager + apps)| 1100 | drdr-demo (showcase)     | 268 |
| drdr-font (8×16 glyphs)         |  659 | drdr-tty (raw-mode help) | 186 |
| drdr-shell (shell)              |  562 | drdr-edit (modal editor) | 463 |
| drdr-files (file browser)       |  498 |                          |     |

Regenerate these numbers any time:

```sh
git ls-files '*.rs' | grep -v '^buildroot/' | xargs wc -l | tail -1   # LOC
git rev-list --count HEAD                                              # commits
grep -rl '#\[test\]' --include='*.rs' . | grep -v buildroot \
  | xargs grep -ch '#\[test\]' | paste -sd+ | bc                       # tests
```

> Numbers are a point-in-time snapshot (the commands above are the
> source of truth). They count *our* userland; the kernel underneath is
> stock Linux built by Buildroot and deliberately excluded.

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
      framebuffer splash → hands off to the graphical session
- [x] **Phase 2 — Core applications**
      DrDrShell Tier 2 (pipes, redirects, quoting) · DrDrFiles Tier 2 (interactive TUI)
      · DrDrEdit Tier 2 (vi-style modal) · drdr-tty shared raw-mode helper
- [x] **Phase 3 — GUI framework**
      Tier 1 widgets (Label/Button/Frame/VBox/HBox + Theme) ·
      Tier 2 input (evdev KeyReader, KeyCode, focus model) · `drdr-demo` showcase
- [x] **Phase 4 — Network & protocols**
      DrDrNet Tier 1: length-prefixed binary frames + typed Encoder/Decoder ·
      Tier 2: correlation IDs, Codec trait, Conn request/reply, real
      `tcp` transport (std::net, thread-per-conn) · **Tier 3 done** (see Phase 7)
- [x] **Phase 5 — Polish & ISO**
      `iso/build.sh` (grub-mkrescue hybrid ISO) · `scripts/qemu.sh --iso`
      (+ `--uefi`) · DrDrTheme polish pass done (semantic roles +
      WCAG-AA contrast, enforced by test) · **ISO boot test passed**:
      `iso/drdros.iso` boots end-to-end under UEFI (GRUB → kernel →
      drdr-init splash → DrDrDesk). Legacy-BIOS boot needs
      `grub-pc-bin` at ISO-build time (build warns if absent)
- [x] **Phase 6 — Graphical session**
      DrDrFont completed to the full printable ASCII set (≈95 hand-authored
      8×16 glyphs via a compact `const fn` pixel-art DSL) · **DrDrDesk**:
      framebuffer desktop with a keyboard-driven launcher (↑/↓/Tab, Enter)
      for DrDrShell / DrDrFiles / DrDrEdit + Reboot / Power off, themed by
      DrDrTheme · **drdr-init is now a supervisor**: it *spawns* (not
      `exec`s) the session, reaps every orphaned child as PID 1 must, and
      respawns the desktop if it ever exits — a session crash is a
      flicker, not a kernel panic · **verified**: `iso/drdros.iso` boots
      under UEFI straight into the DrDrDesk desktop (headless QMP
      screendump)
- [x] **Phase 7 — Window manager + DrDrNet Tier 3 (async)**
      **DrDrUI Tier 2**: a `TextGrid` + `WindowApp` surface (apps draw
      characters, not pixels — no PTY, no terminal emulator), a stacking
      `WindowManager` (overlapping windows, title bars, drag-to-move,
      Alt-Tab, click-`[x]`-to-close), a hand-drawn cursor, and an
      `InputHub` that `poll(2)`s the auto-detected keyboard **and** mouse
      (evdev `REL_*`/`BTN_LEFT`) at once · **DrDrDesk Tier 2** is now that
      WM, hosting About / DrDrFiles / System / DrDrNet windows (`--ppm`
      still works) · **DrDrNet Tier 3**: an incremental `FrameParser`
      (re-frames a TCP byte stream without blocking) + a hand-rolled
      single-thread **epoll reactor** (`nix`, no tokio, many connections
      one thread) keeping the Tier 2 wire format + correlation IDs · the
      DrDrNet window is a *live client* of that reactor (DrDrDesk runs
      the server in a background thread; drdr-init brings `lo` up so the
      loopback TCP works) · **verified** end-to-end on the headless UEFI
      ISO boot: the panel shows the reactor serving ~4 req/s
- [x] **Phase 7.5 — Desktop made usable** (field-hardening)
      Killed the kernel `fbcon` fighting us for `/dev/fb0` (VT taken into
      `KD_GRAPHICS`, keyboard silenced, restored on exit) · double-buffered
      every frame (no flicker) · capability-based input detection (the ACPI
      Power Button no longer steals the keyboard) · wait out async USB
      enumeration for keyboard *and* mouse · input coalescing + repaint
      only on change + KVM-by-default → smooth cursor · the desktop now
      paints **before** input is attached (never a stuck splash) ·
      **DrDrFiles** opens folders/files on double-click and can
      create/delete · **DrDrEdit** runs as an in-window editor · a
      **Launcher** reopens closed windows (and auto-returns when the
      desktop is emptied)
- [ ] **Phase 8 — DrDrNet over the wire + more windowed apps** (ahead)

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
bash scripts/qemu.sh --iso       # boots via GRUB (legacy BIOS)
bash scripts/qemu.sh --iso --uefi # ...via UEFI/OVMF (UEFI-only ISOs)
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


*Built by [@gravijet](https://github.com/gravijet) and Claude.*
<span style="background:#000;color:#000;cursor:pointer;" 
  onclick="this.style.color='#fff'">
  Claude did basically everything.
</span>