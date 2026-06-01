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
| **Storage** | Runs from RAM; **automatic persistence** — a removable disk is adopted on boot and apps autosave, so files survive a reboot (or pick a volume yourself in Disks) |
| **Input** | Keyboard, mouse **and touchscreen** (a Surface-class tablet is usable with no keyboard) |
| **Target** | x86_64 PCs & tablets from the last ~15 years · VirtualBox · QEMU · **Ventoy USB on real hardware** |
| **Status** | Boots to a modern desktop (rounded "Mica" windows, acrylic taskbar): Start menu, draggable windows, **anti-aliased text**, **real pictographic app icons** (also on the taskbar + title bars), ~20 apps incl. a browser, an **image viewer (real PNG/GIF/JPEG/BMP/PPM)**, a **PDF/ZIP/DOCX reader**, a **media-info panel (MP4/MKV)**, a **Wi-Fi manager (scan + connect via wpa_supplicant)**, a **menu bar with text formatting**, and automatic disk persistence |
| **File formats** | Opens **txt · md · html · source code (Rust/JS/Java/C/Py/…) · PNG · GIF · JPEG · BMP · PPM · PDF · ZIP · DOCX/XLSX/PPTX**, and reads metadata from **MP4/MOV · MKV/WebM · MP3/FLAC/WAV** — DEFLATE, PNG, GIF (LZW), baseline JPEG (Huffman+IDCT), ZIP, PDF and the MP4/Matroska container probes all written from scratch in our own `drdr-codec` |

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
- **Real, hand-drawn app icons** — every desktop tile and Start-menu
  row now shows a proper pictographic icon (a folder, a sheet of paper,
  a calculator with a keypad, a globe, a gear, a floppy disk…) drawn
  from framebuffer primitives, not a single scaled font letter. Same
  from-scratch spirit as the bitmap font: no image files, no SVG parser,
  just shapes composed at draw time (`drdr-ui/src/icon.rs`).
- **A dozen-plus windowed apps** — Files (now type-aware: it knows a
  `.png` from a `.rs` and opens each in the right viewer), **Text
  Editor** with a **Windows-style menu bar** (File / Format / Colour /
  View) for **colouring text, making it bigger/smaller, and toggling
  syntax highlighting** — all with the mouse, no shortcuts to memorise —
  plus per-language **syntax highlighting** for Rust/JS/Java/C/Python/…,
  **Notes** (persistent + autosaving), **Tasks** (a persistent to-do
  list), a **Browser** (`DrDrBrowser` — a from-scratch local renderer
  for HTML and Markdown), an **Image viewer** (real **PNG** (DEFLATE),
  **GIF** (LZW) and **baseline JPEG** (Huffman + IDCT + YCbCr) decoding,
  plus BMP/PPM), a **PDF / ZIP / DOCX reader**, a **media-info panel**
  that reads MP4 / Matroska container metadata (codec, duration,
  resolution — honestly *no* frame decoding), a
  **Network & Wi-Fi manager** (scans, lists networks with signal +
  security, takes a password and connects), **Calculator** (our own
  expression parser), **Clock &
  Calendar**, **System Monitor** (live CPU/RAM/load from `/proc`),
  **System Info** (a neofetch-style card), **DrDrConsole** (a no-PTY
  command interpreter), **DrDrChat** (LAN chat between DrDrOS machines),
  **DrDrPaint** (mouse-driven block drawing), and the games
  **DrDrSnake**, **DrDr2048** and **DrDrMines** (Minesweeper), plus
  **Disks**, **Settings**, the DrDrNet panel, About, and the power menu.
- **Smooth, anti-aliased text** — the hand-drawn 8×16 bitmap font is now
  resampled in software: letters stay crisp and full-strength while their
  diagonal staircases get a soft edge, and large logos/icons render as
  smooth rounded shapes instead of fat square pixels. No second font, no
  TTF parser — the same pixel art, de-pixelated at draw time.
- **Real persistence (DrDrStore), now automatic** — everything runs from
  RAM by default, but on boot DrDrOS quietly mounts the largest
  **removable, writable** disk it finds (the USB stick you booted from)
  and points your Documents folder at it — so Notes / Tasks / the editor
  **save for real without any setup**, and **autosave** flushes work even
  if you never press save. You can still pick a specific volume in
  **Disks** (probing ext4/vfat/exfat/ntfs3/…); either way the disk is
  auto-rediscovered next boot via a `.drdros` marker. The kernel now
  carries the USB-storage / SCSI / NVMe / MMC stack and the
  ext4/vfat/exfat/ntfs3 filesystems so a real disk actually appears.
- **DrDrNet, over the wire** — the original length-prefixed binary
  protocol now runs on every interface, not just loopback. Two DrDrOS
  machines on the same LAN find each other automatically via a tiny
  UDP-broadcast discovery protocol (`DDRN` magic, HELLO/BYE, peer
  expiry), and **DrDrChat** exchanges typed chat frames between them
  over the hand-rolled single-thread epoll reactor — no tokio, no HTTP,
  no broker.
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
| **drdr-desk** | binary | DrDrDesk — the desktop: a dozen-plus windowed apps (incl. LAN chat, paint, snake), never blocks on input, attaches keyboard/mouse/touch live |
| **drdr-shell** | binary | DrDrShell — custom shell with pipes, redirects, quoting |
| **drdr-edit** | binary | DrDrEdit — vi-style modal text editor |
| **drdr-files** | binary | DrDrFiles — batch lister + interactive TUI file browser |
| **drdr-fb** | library | DrDrFb — framebuffer access for **16/24/32bpp, any channel order**, now with anti-aliased `fill_circle` / `draw_line` primitives for the icon renderer |
| **drdr-font** | library | DrDrFont — hand-drawn 8×16 bitmap glyphs **+ a software anti-aliaser** (crisp strokes, soft edges, smooth scaled logos) |
| **drdr-ui** | library | DrDrUI — widgets, Theme (light + dark), `TextGrid`/`WindowApp` (now with per-window **text zoom**), the WM **+ taskbar/Start-menu shell**, a **pictographic icon renderer** (`icon.rs`), `InputHub` (kbd + mouse + **touchscreen**), VT takeover |
| **drdr-store** | library | DrDrStore — block-device discovery, mounting, and a `save`/`load` API so files persist beyond RAM |
| **drdr-codec** | library | DrDrCodec — from-scratch decoders: a hand-written DEFLATE/zlib ([RFC 1951/1950]), and on top of it a PNG decoder, a ZIP reader (→ DOCX/XLSX/PPTX text) and a PDF text extractor. No external crates |
| **drdr-tty** | library | DrDrTty — termios raw-mode + key decoder for terminal apps |
| **drdr-net** | library | DrDrNet — custom binary protocol + a hand-rolled epoll reactor (Tier 3 async) **+ UDP-broadcast peer discovery + a chat sub-protocol** so two DrDrOS machines on a LAN find each other and talk |
| **buildroot/** | tooling | Buildroot config + BR2_EXTERNAL recipe; `linux-fb.config` (display) + `linux-input.config` (evdev/USB-HID/xHCI for real tablets) + `linux-storage.config` (USB-storage/SCSI/NVMe/MMC + ext4/vfat/exfat/ntfs3 so disks mount + persist) + `linux-wifi.config` (cfg80211/mac80211 + Intel/Broadcom/Atheros/Realtek Wi-Fi + Ethernet so a real radio enumerates) |
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
| Rust source | **18683 lines** across **28 files** |
| Workspace crates | **13** (every `drdr-*`) |
| Tests | **119** (`cargo test`, all green) |
| Git commits | **51** |
| Tracked files (excl. `buildroot/`) | **57** |
| Development window | 2026-05-14
? → 2026-06-01 |

Lines of Rust per crate (largest first):

| Crate | Lines | Purpose |
|---|--:|---|
| drdr-desk  |  6610 | window manager + apps |
| drdr-ui    |  4186 | GUI framework + WM + shell |
| drdr-codec |  2446 | — |
| drdr-net   |  1911 | binary proto + reactor |
| drdr-fb    |   976 | framebuffer (all bpp) |
| drdr-font  |   886 | 8x16 glyphs |
| drdr-store |   562 | persistent storage |
| drdr-shell |   562 | shell |
| drdr-init  |   541 | PID 1 / supervisor |
| drdr-files |   498 | file browser |
| drdr-edit  |   463 | modal editor |
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
- [x] **Phase 7.7 — Modern desktop + persistence**
      Windows-style **taskbar + Start menu**, light "Fluent" theme with a
      dark toggle, window shadows + minimise/maximise/close · **DrDrStore**
      (mount a disk, save for real) · new apps: Notes, Calculator, Clock &
      Calendar, System Monitor, DrDrConsole, Disks, Settings · README
      numbers auto-regenerate on commit/push
- [x] **Phase 8 — DrDrNet over the wire + more windowed apps**
      *(this release)*
      The DrDrNet reactor binds to **every interface**, not just
      loopback, and a tiny UDP-broadcast **peer-discovery** sub-protocol
      (`DDRN` magic, HELLO/BYE, TTL expiry) lets two DrDrOS machines on
      the same LAN find each other with no config · the same reactor
      now multiplexes the existing `status` protocol with a new typed
      **chat** sub-protocol (`KIND_CHAT_SAY`, fire-and-forget
      `ChatMsg`) · the WM gains a real `on_drag` hook on the
      `WindowApp` trait so drawing apps see motion-while-held, not just
      clicks · three new windowed apps wire it together: **DrDrChat**
      (peer list + log + composer; broadcasts to every known peer),
      **DrDrPaint** (palette + click/drag block painting), and
      **DrDrSnake** (tick-driven game).
- [x] **Phase 9 — Smoother, more, and it actually keeps your files**
      *(this release)*
      **Anti-aliased text** everywhere (`drdr-font` resamples its own
      pixel art at draw time — crisp strokes, soft edges, smooth scaled
      logos/icons) · **storage that just works**: DrDrStore auto-adopts
      a removable writable disk on boot and Notes/Tasks/editor
      **autosave**, so files persist with no setup · a kernel
      `linux-storage.config` fragment (USB-storage · SCSI · NVMe · MMC ·
      ext4/vfat/exfat/ntfs3 + NLS) so a real USB stick / SD card / disk
      enumerates and mounts at all · **four new apps**: **Tasks**
      (persistent to-do), **DrDr2048**, **DrDrMines** (Minesweeper),
      **System Info** (neofetch-style card) · a hardened bootable ISO —
      `grub.cfg` uses `search --file` + `gfxpayload=keep` + a
      "safe graphics" entry, and `iso/build.sh` fails fast on a missing
      UEFI GRUB and prints exact **Secure-Boot-off** steps for the
      Surface Go 2 / ThinkPad T14 (the real reason a custom ISO won't
      boot on them).
- [x] **Phase 10 — Real icons, a formatting menu bar, more apps, more
      formats, Wi-Fi** *(this release)*
      The desktop stops looking like a debug build: every app tile and
      Start-menu row gets a **real pictographic icon** — a folder, a
      sheet of paper, a calculator keypad, a globe, a gear, a floppy —
      hand-drawn from new anti-aliased `fill_circle`/`draw_line`
      framebuffer primitives (`drdr-ui/src/icon.rs`), no font letters,
      no image files · the **Text Editor gains a Windows-style menu bar**
      (File / Format / Colour / View) so you **colour text, size it
      up/down and toggle highlighting with the mouse** — backed by a real
      per-character colour buffer kept in lock-step with edits and
      per-window **text zoom** in the WM · **syntax highlighting** for
      Rust/JS/Java/C/Python/Shell/JSON · the file manager is now
      **type-aware** (it knows `.png` from `.rs` from `.pdf`) and routes
      each file to the right viewer · **three new apps**: **DrDrBrowser**
      (a from-scratch local HTML/Markdown renderer), an **Image viewer**
      (decodes PPM + BMP into full-colour cells, reports PNG/JPEG
      dimensions) and a **Network & Wi-Fi** panel (enumerates
      `/sys/class/net`, flags wireless radios, shows DrDrNet peers) · a
      **`linux-wifi.config`** kernel fragment brings up the 802.11 stack
      and the Wi-Fi/Ethernet drivers the target machines use · a third,
      maximally-conservative GRUB boot entry for stubborn firmware.
- [x] **Phase 11 — Real file formats, a working Wi-Fi manager, a rounder
      Win11 look** *(this release)*
      A new **`drdr-codec`** crate decodes the formats a desktop must
      open, all from scratch (no crates): a hand-written **DEFLATE/zlib**
      (RFC 1951/1950) is the keystone, and on top of it a **PNG** decoder
      (filters + all colour types), a **ZIP** reader and a **PDF** text
      extractor — so the image viewer shows **real PNGs** in colour, and
      the file manager opens **PDF, ZIP and DOCX/XLSX/PPTX** (Office docs
      are ZIPs of XML; we pull the text out) · the **Network panel
      becomes a real Wi-Fi manager**: it scans, lists networks with
      signal + security, takes a password and connects by driving
      `wpa_supplicant` over `wpa_cli` + `udhcpc` (the WPA2/WPA3 crypto is
      plumbing we don't hand-roll, like the kernel; the UI is ours), with
      `wpa_supplicant` + `iw` + Wi-Fi firmware added to the rootfs · the
      desktop gets a **Windows-11 refresh**: rounded "Mica" title bars
      that share the window surface (no more heavy coloured strips), a
      larger corner radius, squircle icon tiles, softer shadows, an
      **acrylic (translucent) taskbar** and a soft radial **bloom**
      wallpaper. Backed by new anti-aliased `fill_circle`/`draw_line`
      framebuffer primitives.

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
sudo bash scripts/qemu.sh               # GTK window + serial on stdio
sudo bash scripts/qemu.sh --kvm         # KVM if /dev/kvm exists

# Or a bootable hybrid ISO (works under Ventoy on real hardware).
sudo bash iso/build.sh                  # → iso/drdros.iso
sudo bash scripts/qemu.sh --iso --uefi  # boot it via UEFI/OVMF
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
