#!/usr/bin/env bash
# iso/build.sh — package the DrDrOS kernel + initramfs into a bootable
# hybrid ISO using GRUB's rescue-image generator.
#
# Input:
#   $REPO/buildroot/images/bzImage        (symlink set by build-buildroot.sh)
#   $REPO/buildroot/images/rootfs.cpio.gz (likewise)
#
# Output:
#   $REPO/iso/drdros.iso  — boot it in QEMU (-cdrom) or `dd` to a USB stick.
#
# Usage:
#   iso/build.sh              # build with defaults
#   iso/build.sh --bzimage X --rootfs Y --output Z
#
# Tooling required: grub-mkrescue (grub-common) + xorriso. EFI boot works
# out of the box; BIOS boot also works if grub-pc-bin is installed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BZIMAGE="$REPO_ROOT/buildroot/images/bzImage"
ROOTFS="$REPO_ROOT/buildroot/images/rootfs.cpio.gz"
OUTPUT="$REPO_ROOT/iso/drdros.iso"
STAGE="$REPO_ROOT/iso/build"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bzimage) BZIMAGE="$2"; shift 2 ;;
        --rootfs)  ROOTFS="$2";  shift 2 ;;
        --output)  OUTPUT="$2";  shift 2 ;;
        -h|--help)
            sed -n '2,16p' "$0"
            exit 0
            ;;
        *)
            echo "iso/build.sh: unknown arg '$1'" >&2
            exit 2
            ;;
    esac
done

if ! command -v grub-mkrescue >/dev/null; then
    echo "iso/build.sh: grub-mkrescue not found — install grub-common + xorriso" >&2
    exit 1
fi
# grub-mkrescue shells out to mtools (mformat/mcopy) to build the EFI
# System Partition FAT image. Without it the run dies late with a cryptic
# "mformat invocation failed" and writes no ISO — catch it up front.
if ! command -v mformat >/dev/null; then
    echo "iso/build.sh: mformat not found — install mtools (grub-mkrescue" >&2
    echo "  needs it to build the embedded EFI boot image)" >&2
    echo "  → sudo apt-get install -y mtools" >&2
    exit 1
fi
# grub-mkrescue only embeds a legacy-BIOS El Torito boot image if the
# BIOS GRUB modules (package grub-pc-bin) are present. Without them the
# ISO is UEFI-only: it boots fine on modern machines and under
# `scripts/qemu.sh --iso --uefi`, but SeaBIOS (plain `--iso`) and older
# BIOS-only PCs will report "No bootable device". Warn, don't fail —
# a UEFI-only ISO is still a valid, useful artifact.
if [[ ! -d /usr/lib/grub/i386-pc ]]; then
    echo "iso/build.sh: WARNING — grub-pc-bin not found" >&2
    echo "  → the ISO will be UEFI-only (no legacy-BIOS boot)." >&2
    echo "  → boot it with: scripts/qemu.sh --iso --uefi" >&2
    echo "  → for BIOS boot too: sudo apt-get install -y grub-pc-bin" >&2
fi
# The UEFI El Torito image needs the x86_64-efi GRUB modules
# (grub-efi-amd64-bin). Without them grub-mkrescue produces a BIOS-only
# ISO that a Surface Go 2 / ThinkPad T14 (both UEFI) will NOT boot at
# all — the single most common "it doesn't start" cause after Secure
# Boot. Fail loudly: a UEFI machine with a BIOS-only ISO is dead media.
if [[ ! -d /usr/lib/grub/x86_64-efi ]]; then
    echo "iso/build.sh: ERROR — grub-efi-amd64-bin not found" >&2
    echo "  /usr/lib/grub/x86_64-efi is missing, so grub-mkrescue cannot" >&2
    echo "  build a UEFI boot image. The resulting ISO would not boot on" >&2
    echo "  a Surface Go 2 / ThinkPad T14 (both UEFI-only)." >&2
    echo "  → sudo apt-get install -y grub-efi-amd64-bin" >&2
    exit 1
fi

if [[ ! -f $BZIMAGE ]]; then
    echo "iso/build.sh: kernel image not found: $BZIMAGE" >&2
    echo "  → run scripts/build-buildroot.sh first" >&2
    exit 1
fi
if [[ ! -f $ROOTFS ]]; then
    echo "iso/build.sh: initramfs not found: $ROOTFS" >&2
    exit 1
fi

echo "[iso/build.sh] staging $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/boot/grub"

cp "$BZIMAGE" "$STAGE/boot/bzImage"
cp "$ROOTFS"  "$STAGE/boot/rootfs.cpio.gz"

cat > "$STAGE/boot/grub/grub.cfg" <<'EOF'
# DrDrOS GRUB boot configuration.
#
# Robustness notes (why each line is here — learned booting real UEFI
# hardware: Surface Go 2, ThinkPad T14, not just QEMU):
#
#  * `search --file` re-finds the partition that actually holds our
#    kernel and sets $root to it. Without this, GRUB's idea of $root can
#    be wrong when the ISO is launched indirectly (Ventoy, a USB stick
#    whose layout the firmware mapped differently), and `linux
#    /boot/bzImage` then fails with "file not found" — a silent non-boot.
#  * `all_video` + `efi_gop`/`efi_uga` load every framebuffer backend GRUB
#    has, and `gfxpayload=keep` hands the *live* GOP framebuffer straight
#    to the kernel. On efifb/simpledrm machines this is the difference
#    between a desktop and a black screen after the GRUB menu.
#  * We keep `console=tty0` on every entry so the screen always shows the
#    boot, and drop `quiet` from the safe/verbose entries for diagnosis.

insmod part_gpt
insmod part_msdos
insmod fat
insmod iso9660
insmod all_video
insmod efi_gop
insmod efi_uga
insmod gfxterm
insmod video_bochs
insmod video_cirrus

# Find the volume that carries our kernel and make it $root, wherever the
# firmware placed it. The `|| true` keeps going if search isn't needed.
search --no-floppy --file --set=root /boot/bzImage

set gfxpayload=keep
terminal_output gfxterm

# Edit timeout=0 for zero-pause autoboot (CI / Ventoy). 5s gives a user on
# a slow-to-init panel time to see the menu and pick "safe graphics".
set timeout=5
set default=0

menuentry "DrDrOS — boot to desktop" {
    set gfxpayload=keep
    linux  /boot/bzImage console=tty0 loglevel=4
    initrd /boot/rootfs.cpio.gz
}

menuentry "DrDrOS — safe graphics (force EFI framebuffer)" {
    # nomodeset disables the DRM KMS drivers and falls back to the plain
    # firmware framebuffer (efifb). Use this if the desktop boots to a
    # black/garbled screen with the default entry on real hardware.
    set gfxpayload=keep
    linux  /boot/bzImage console=tty0 nomodeset video=efifb loglevel=4
    initrd /boot/rootfs.cpio.gz
}

menuentry "DrDrOS — verbose boot (serial + tty0)" {
    linux  /boot/bzImage console=tty0 console=ttyS0 loglevel=7
    initrd /boot/rootfs.cpio.gz
}
EOF

echo "[iso/build.sh] running grub-mkrescue"
# `--compress=xz` shaves a few MiB off the ISO; xorriso must support it
# (Ubuntu 22.04+ does). The redirect quiets GRUB's chatty status output.
grub-mkrescue \
    --compress=xz \
    -o "$OUTPUT" \
    "$STAGE" 2>&1 | grep -vE '^(xorriso|Drive current|Media current|Media status|Drive size|Media size|Media blocks)' || true

ls -la "$OUTPUT"
echo
echo "[iso/build.sh] success → $OUTPUT"
echo "  Boot in QEMU (UEFI): scripts/qemu.sh --iso --uefi"
echo "  Write to USB:        sudo dd if=$OUTPUT of=/dev/sdX bs=4M status=progress oflag=sync"
echo
echo "  ┌─ REAL HARDWARE (Surface Go 2 / ThinkPad T14) ─────────────────┐"
echo "  │ If the machine shows 'No bootable device' or jumps straight   │"
echo "  │ back to Windows, it is almost always SECURE BOOT, not the     │"
echo "  │ ISO: this GRUB image is unsigned, so UEFI firmware refuses it │"
echo "  │ until Secure Boot is OFF.                                     │"
echo "  │   Surface Go 2: hold Volume-Up + tap Power → UEFI → Security  │"
echo "  │     → Secure Boot → Disabled (or 'Microsoft & 3rd party').    │"
echo "  │   ThinkPad T14: tap Enter/F1 at boot → Security → Secure Boot │"
echo "  │     → Disabled. Set Boot Mode = UEFI (not Legacy).            │"
echo "  │ Then F12 (T14) / Volume-Down (Surface) to pick the USB stick.│"
echo "  │ Ventoy users: use 'GRUB2 Mode' (press Ctrl-r) if normal mode │"
echo "  │ chainloads the ISO into a blank screen.                      │"
echo "  └───────────────────────────────────────────────────────────────┘"
