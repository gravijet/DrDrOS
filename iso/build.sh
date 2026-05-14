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
# Edit timeout=0 if you want zero-pause autoboot — useful in CI / Ventoy.
set timeout=3
set default=0

menuentry "DrDrOS — boot to userland" {
    linux  /boot/bzImage console=tty0 quiet
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
echo "  Boot in QEMU: scripts/qemu.sh --iso"
echo "  Write to USB: sudo dd if=$OUTPUT of=/dev/sdX bs=4M status=progress oflag=sync"
