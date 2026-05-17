# DrDrOS

> A complete, minimal, fast, fully custom **userland operating system**
> built from scratch on top of the Linux kernel — in **Rust**.

DrDrOS replaces every part of the system a human ever sees or touches.
The shell, the editor, the file manager, the GUI framework, the window
manager, the network protocol, the storage layer — **all original**,
none borrowed. The Linux kernel underneath handles only drivers, memory,
and scheduling; everything above it is ours.

| | |
|---|---|
| **Language** | Rust (memory-safe, fast, modern) |
| **Display** | Linux framebuffer (`/dev/fb0`) — no X11, no Wayland, no DE |
| **Pixel formats** | 16 / 24 / 32 bpp, any RGB/BGR channel order (real efifb/simpledrm, not just QEMU) |
| **Storage** | Runs from RAM; **opt-in persistence** — mount a disk and your files survive a reboot |
| **Input** | Keyboard, mouse **and touchscreen** (a Surface-class tablet is usable with no keyboard) |
| **Target** | x86_64 PCs & tablets from the last ~15 years · VirtualBox · QEMU · **Ventoy USB on real hardware** |
| **Status** | Boots to a modern desktop: taskbar + Start menu, draggable windows, a dozen apps, optional disk storage |

### What you actually get when it boots

Power on a PC, a tablet, or a VM and a few seconds later you are in a
graphical desktop — no login, no shell, no X11:

- **A modern desktop shell** — a bottom **taskbar** with a Start button,
  a live clock + date, and one button per window (click to
  focus / minimise / restore). A **Start menu** lists every app. Windows
  have soft drop shadows, a light "Fluent" theme (dark theme one toggle
  away), and **minimise / maximise / close** controls. Double-click a
  title bar to maximise; drag it to move.
- **A real window manager** — overlapping, titled windows, Alt-Tab to
  cycle, a hand-drawn cursor, a Launcher that returns if you close
  everything, so the desktop is never a dead end.
- **A dozen windowed apps** — Files, Text Editor, **Notes** (persistent),
  **Calculator** (our own expression parser), **Clock & Calendar**,
  **System Monitor** (live CPU/RAM/load from `/proc`), **DrDrConsole**
  (a no-PTY command interpreter), **Disks**, **Settings**, the DrDrNet
  panel, About, and the power menu.
- **Real, opt-in persistence (DrDrStore)** — everything runs from RAM by
  default. Open **Disks**, pick a partition, and DrDrOS mounts it
  (probing ext4/vfat/exfat/ntfs/…) and makes it your data directory.
  Notes and saved files now survive a reboot, and the disk is
  auto-rediscovered next time via a `.drdros` marker.
- **DrDrNet** — a live status panel speaking an original length-prefixed
  binary protocol over a hand-rolled single-thread epoll reactor (no
  tokio, no HTTP) across loopback TCP.
- **Owns the screen properly, on real hardware** — takes the Linux VT
  into graphics mode, double-buffers every frame, coalesces input, and
  **encodes pixels for the panel's true format** so an efifb/simpledrm
  framebuffer on a real machine shows a desktop instead of looking
  frozen. It never blocks waiting for a device: input attaches live, so
  a keyboardless tablet still comes up and is driven by touch.

Every pixel and keystroke above is handled by code in this repository.

---

## Philosophy

- **Linux handles** drivers, hardware, memory, kernel — we never touch it.
- **DrDrOS handles** everything the user sees and uses.
- Every component is **written from scratch**. If `bash` / `vim` /
  `htop` / a date library already exists, we build our own.
- Every component name starts with **DrDr**.
- Boot fast. Use little. Look clean. Never hang in front of the user.

---

## Architecture

```
                  ┌──────────────────────────────────────────────────┐
                  │                  DrDrOS USERLAND                 │
                  │                                                  │
                  │   DrDrDesk  —  taskbar · Start menu · windows    │
                  │   ┌────────┬────────┬────────┬────────┬───────┐  │
                  │  Files  Editor   Notes    Calc   SysMon  …apps  │
                  │   └────────┴────────┴────────┴────────┴───────┘  │
                  │                      │                           │
                  │                      ▼                           │
                  │                   DrDrUI                         │
                  │        (windows · widgets · WM · shell)          │
                  │   ┌──────────┬──────────┬──────────┬──────────┐  │
                  │   ▼          ▼          ▼          ▼          ▼  │
                  │ DrDrFont  framebuffer DrDrNet   DrDrStore  input │
                  │ (glyphs) (16/24/32bpp)(proto)  (persist) (kbd/   │
                  │                                          mouse/  │
                  │                                          touch)  │
                  │                drdr-init  (PID 1)                │
                  └────────────────────────┬─────────────────────────┘
                                           │ Linux syscalls
                  ┌────────────────────────▼─────────────────────────┐
                  │              LINUX KERNEL (minimal)              │
                  │  drivers · memory · scheduler · fbdev · evdev    │
                  └────────────────────────┬─────────────────────────┘
                                           │
                  ┌────────────────────────▼─────────────────────────┐
                  │          HARDWARE — x86_64 PC / tablet           │
                  └──────────────────────────────────────────────────┘
```

---

## Components

| Crate / dir | Kind | Purpose |
|---|---|---|
| **drdr-init** | binary | PID 1 — mounts, hostname, brings `lo` up, paints the splash (logging the real pixel format), then *supervises* the session |
| **drdr-desk** | binary | DrDrDesk — the desktop: a dozen windowed apps, never blocks on input, attaches keyboard/mouse/touch live |
| **drdr-shell** | binary | DrDrShell — custom shell with pipes, redirects, quoting |
| **drdr-edit** | binary | DrDrEdit — vi-style modal text editor |
| **drdr-files** | binary | DrDrFiles — batch lister + interactive TUI file browser |
| **drdr-fb** | library | DrDrFb — framebuffer access for **16/24/32bpp, any channel order** |
| **drdr-font** | library | DrDrFont — hand-drawn 8×16 bitmap glyph renderer |
| **drdr-ui** | library | DrDrUI — widgets, Theme (light + dark), `TextGrid`/`WindowApp`, the WM **+ taskbar/Start-menu shell**, `InputHub` (kbd + mouse + **touchscreen**), VT takeover |
| **drdr-store** | library | DrDrStore — block-device discovery, mounting, and a `save`/`load` API so files persist beyond RAM |
| **drdr-tty** | library | DrDrTty — termios raw-mode + key decoder for terminal apps |
| **drdr-net** | library | DrDrNet — custom binary protocol + a hand-rolled epoll reactor (Tier 3 async) |
| **buildroot/** | tooling | Buildroot config + BR2_EXTERNAL recipe; `linux-fb.config` (display) + `linux-input.config` (evdev/USB-HID/xHCI for real tablets) |
| **iso/** | tooling | xorriso pipeline producing the bootable `drdros.iso` |
| **scripts/** | tooling | `qemu.sh` runner · `stats.sh` (auto-updates the numbers below) |

---

## Project stats

A snapshot of the **from-scratch userland only** — the Linux kernel and
the Buildroot tree are *not* counted, just code in this repo. **These
numbers regenerate themselves** on every commit (a versioned
`.githooks/pre-commit`) and every push (a GitHub Action), so they are
never stale — see [Keeping the numbers honest](#keeping-the-numbers-honest).

<!-- STATS:START -->
<!-- Generated by scripts/stats.sh — do not edit by hand.
     Refreshed automatically on every commit (.githooks/pre-commit)
     and every push (.github/workflows/stats.yml). -->

| Metric | Value |
|---|---|
| Rust source | **11120 lines** across **20 files** |
| Workspace crates | **12** (every `drdr-*`) |
| Tests | **62** (`cargo test`, all green) |
| Git commits | **38** |
| Tracked files (excl. `buildroot/`) | **48** |
| Development window | 2026-05-14
? → 2026-05-17 |

Lines of Rust per crate (largest first):

| Crate | Lines | Purpose |
|---|--:|---|
| drdr-ui    |  2902 | GUI framework + WM + shell |
| drdr-desk  |  2425 | window manager + apps |
| drdr-net   |  1518 | binary proto + reactor |
| drdr-fb    |   664 | framebuffer (all bpp) |
| drdr-font  |   659 | 8x16 glyphs |
| drdr-shell |   562 | shell |
| drdr-store |   513 | persistent storage |
| drdr-files |   498 | file browser |
| drdr-edit  |   463 | modal editor |
| drdr-init  |   462 | PID 1 / supervisor |
| drdr-demo  |   268 | widget showcase |
| drdr-tty   |   186 | raw-mode helper |
<!-- STATS:END -->

> Numbers count *our* userland; the kernel underneath is stock Linux
> built by Buildroot and deliberately excluded. Regenerate or verify any
> time with `scripts/stats.sh` (add `--check` for a non-mutating CI gate).

---

## Roadmap

- [x] **Phase 1 — Foundation** · Cargo workspace · Buildroot · drdr-init
      Tier 2 · drdr-fb · drdr-font · BR2_EXTERNAL wiring · `qemu.sh`
- [x] **First boot** — end-to-end under QEMU (custom kernel → drdr-init →
      session)
- [x] **Phase 2 — Core apps** · DrDrShell · DrDrFiles · DrDrEdit · drdr-tty
- [x] **Phase 3 — GUI framework** · widgets · evdev input · `drdr-demo`
- [x] **Phase 4 — Network** · DrDrNet framing/codecs/transport
- [x] **Phase 5 — Polish & ISO** · hybrid ISO · UEFI boot verified ·
      WCAG-AA theme (now enforced for **both** light and dark)
- [x] **Phase 6 — Graphical session** · full font · DrDrDesk · supervisor
- [x] **Phase 7 — Window manager + DrDrNet Tier 3** · stacking WM ·
      `InputHub` · epoll reactor · live DrDrNet window
- [x] **Phase 7.5 — Desktop made usable** · VT takeover · double buffer ·
      capability input detect · live device attach · create/edit/delete
- [x] **Phase 7.6 — Runs on real hardware** *(this release)*
      drdr-fb encodes pixels for the panel's **true** format (16/24/32bpp,
      RGB or BGR) so efifb/simpledrm on a real machine (a Surface Go 2 via
      Ventoy) shows a desktop, not a frozen splash · the session **never
      blocks waiting for input** and attaches keyboard/mouse/**touch**
      live · touchscreens (`EV_ABS`) drive the cursor so a keyboardless
      tablet is usable · VT open is non-blocking and VT-probed · the
      kernel gains an input fragment (evdev/USB-HID/xHCI/hid-multitouch)
- [x] **Phase 7.7 — Modern desktop + persistence** *(this release)*
      Windows-style **taskbar + Start menu**, light "Fluent" theme with a
      dark toggle, window shadows + minimise/maximise/close · **DrDrStore**
      (mount a disk, save for real) · new apps: Notes, Calculator, Clock &
      Calendar, System Monitor, DrDrConsole, Disks, Settings · README
      numbers auto-regenerate on commit/push
- [ ] **Phase 8 — DrDrNet over the wire + more windowed apps** (ahead)

---

## Building

```sh
# Userland: compile every Rust crate in the workspace.
cargo build --workspace
cargo test  --workspace            # all green

# Cross-compile drdr-init for the rootfs (musl, static PIE).
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p drdr-init

# Kernel + initramfs via Buildroot (out-of-tree cache).
bash scripts/build-buildroot.sh    # ~15-30 min the first time

# Boot kernel + initramfs in QEMU (dev loop).
bash scripts/qemu.sh               # GTK window + serial on stdio
bash scripts/qemu.sh --kvm         # KVM if /dev/kvm exists

# Or a bootable hybrid ISO (works under Ventoy on real hardware).
bash iso/build.sh                  # → iso/drdros.iso
bash scripts/qemu.sh --iso --uefi  # boot it via UEFI/OVMF
```

## Running the core apps on the host

DrDrShell / DrDrEdit / DrDrFiles run on a regular Linux box too:

```sh
cargo run -q -p drdr-shell                 # interactive REPL
cargo run -q -p drdr-files -- -a /tmp      # list /tmp incl. dotfiles
cargo run -q -p drdr-edit  -- notes.txt    # line editor
cargo run -q -p drdr-desk  -- --ppm out.ppm  # render one desktop frame
```

## Keeping the numbers honest

The **Project stats** block above is generated, never hand-typed:

```sh
scripts/stats.sh           # rewrite the block from the repo
scripts/stats.sh --check   # CI gate: fail if out of date, change nothing
bash scripts/install-hooks.sh   # point git at the versioned .githooks/
```

`.githooks/pre-commit` regenerates and re-stages `README.md` on every
commit; `.github/workflows/stats.yml` regenerates on every push and
verifies it on PRs — so the numbers track `HEAD` forever, automatically.

---

*Built by [@gravijet](https://github.com/gravijet) and Claude.*
<span style="background:#000;color:#000;cursor:pointer;"
  onclick="this.style.color='#fff'">
  Claude did basically everything.
</span>
