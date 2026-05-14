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
| **Status** | Phase 2 — Core Apps (Tier 1 of each shipped) |

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
| **drdr-shell** | binary | DrDrShell — custom shell with pipes, redirects, history, autocomplete |
| **drdr-edit** | binary | DrDrEdit — text editor; RAM-resident; keyboard-driven |
| **drdr-files** | binary | DrDrFiles — file manager; keyboard navigation |
| **drdr-ui** | library | DrDrUI — windows, buttons, inputs, focus on the framebuffer |
| **drdr-font** | library | DrDrFont — hand-drawn bitmap font renderer |
| **drdr-net** | library | DrDrNet — custom binary network protocol (not HTTP) |
| **buildroot/** | tooling | Buildroot config producing the minimal Linux base |
| **iso/** | tooling | xorriso pipeline producing the bootable `drdros.iso` |

---

## Roadmap

- [x] **Phase 1 — Foundation**
      Cargo workspace · Buildroot 2026.02.1 vendored · drdr-init Tier 2 (mounts +
      framebuffer splash) · drdr-ui::fb primitives · drdr-font 8×16 bitmaps ·
      BR2_EXTERNAL package recipe · `scripts/qemu.sh` runner
      *(first QEMU boot pending — local fs mount blocks Buildroot exec bits)*
- [~] **Phase 2 — Core applications**
      DrDrShell Tier 1 · DrDrEdit Tier 1 · DrDrFiles Tier 1 (all host-runnable).
      Tier 2 next: pipes/redirects/history · raw-mode TTY navigation
- [ ] **Phase 3 — GUI framework**
      DrDrUI · DrDrFont · DrDrTheme (dark, minimal)
- [ ] **Phase 4 — Network & protocols**
      DrDrNet binary protocol · basic TCP tools on top of it
- [ ] **Phase 5 — Polish & ISO**
      Boot screen · `xorriso` ISO pipeline · screenshots · final docs

---

## Building

```sh
# Compile every crate in the workspace.
cargo build --workspace

# Cross-compile drdr-init for the rootfs (needed once you wire it into Buildroot).
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p drdr-init

# Build the Linux kernel + initramfs via the vendored Buildroot.
make -C buildroot/upstream \
     BR2_DEFCONFIG=$(pwd)/buildroot/drdros_defconfig \
     BR2_EXTERNAL=$(pwd)/buildroot/external \
     defconfig
make -C buildroot/upstream

# Boot the resulting bzImage + rootfs.cpio.gz in QEMU.
scripts/qemu.sh             # GTK window + serial mirrored to stdio
scripts/qemu.sh --headless  # serial-only
scripts/qemu.sh --kvm       # add KVM acceleration if /dev/kvm exists
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
