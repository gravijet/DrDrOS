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
| **Status** | Phase 1 — Foundation |

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

- [ ] **Phase 1 — Foundation**
      Cargo workspace · Buildroot config · drdr-init · framebuffer primitives
- [ ] **Phase 2 — Core applications**
      DrDrShell · DrDrEdit · DrDrFiles
- [ ] **Phase 3 — GUI framework**
      DrDrUI · DrDrFont · DrDrTheme (dark, minimal)
- [ ] **Phase 4 — Network & protocols**
      DrDrNet binary protocol · basic TCP tools on top of it
- [ ] **Phase 5 — Polish & ISO**
      Boot screen · `xorriso` ISO pipeline · screenshots · final docs

---

## Building (preview)

> Real instructions land at the end of Phase 1. For now, just sanity-check
> that the workspace compiles:

```sh
cargo build --workspace
```

Later phases will add:

```sh
# Build the kernel + rootfs (Phase 1)
make -C buildroot drdros_defconfig
make -C buildroot

# Package the bootable ISO (Phase 5)
./iso/build.sh

# Test in QEMU (any phase)
qemu-system-x86_64 -accel kvm -m 512M -cdrom drdros.iso
```

---

## License

Dual-licensed under **MIT OR Apache-2.0** — pick whichever fits your project.

---

*Built by [@gravijet](https://github.com/gravijet).*
