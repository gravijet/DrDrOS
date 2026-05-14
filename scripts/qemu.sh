#!/usr/bin/env bash
# scripts/qemu.sh — boot the DrDrOS build artefacts under QEMU.
#
# Run from anywhere; paths resolve relative to the script's location.
# Defaults:
#   - 256 MiB RAM
#   - stdvga (gives us /dev/fb0 at 1024x768 32bpp by default)
#   - GTK window for the framebuffer
#   - kernel log + init console mirrored to BOTH the GTK window (tty0)
#     and our terminal stdio (ttyS0). Ctrl-A X exits the serial side.
#
# Usage:
#   scripts/qemu.sh            # boot with framebuffer window
#   scripts/qemu.sh --headless # serial-only, no GTK window
#   scripts/qemu.sh --kvm      # enable KVM acceleration if /dev/kvm exists

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# scripts/build-buildroot.sh symlinks the latest build artefacts here.
IMAGES="$REPO_ROOT/buildroot/images"
KERNEL="$IMAGES/bzImage"
INITRD="$IMAGES/rootfs.cpio.gz"

if [[ ! -f $KERNEL ]]; then
    echo "error: $KERNEL not found — run buildroot first" >&2
    exit 1
fi
if [[ ! -f $INITRD ]]; then
    echo "error: $INITRD not found — run buildroot first" >&2
    exit 1
fi

# Defaults.
DISPLAY_ARGS=(-display gtk -vga std)
SERIAL_ARGS=(-serial stdio)
APPEND="console=tty0 console=ttyS0 loglevel=4"
ACCEL=()
ISO_PATH=""

# Parse flags.
for arg in "$@"; do
    case "$arg" in
        --headless)
            DISPLAY_ARGS=(-nographic -vga std)
            SERIAL_ARGS=()  # -nographic already routes serial to stdio
            APPEND="console=ttyS0 loglevel=4"
            ;;
        --kvm)
            if [[ -e /dev/kvm ]]; then
                ACCEL=(-enable-kvm -cpu host)
            else
                echo "warning: /dev/kvm not present, skipping --kvm" >&2
            fi
            ;;
        --iso)
            ISO_PATH="$REPO_ROOT/iso/drdros.iso"
            ;;
        *)
            echo "usage: $0 [--headless] [--kvm] [--iso]" >&2
            exit 2
            ;;
    esac
done

# --iso boots from the GRUB ISO instead of -kernel/-initrd, so we
# exercise the full boot chain like real hardware does.
if [[ -n $ISO_PATH ]]; then
    if [[ ! -f $ISO_PATH ]]; then
        echo "error: $ISO_PATH not found — run iso/build.sh first" >&2
        exit 1
    fi
    echo "[qemu.sh] booting from ISO $ISO_PATH"
    exec qemu-system-x86_64 \
        "${ACCEL[@]}" \
        -m 256M \
        -cdrom "$ISO_PATH" \
        -boot d \
        "${DISPLAY_ARGS[@]}" \
        "${SERIAL_ARGS[@]}"
fi

echo "[qemu.sh] booting $KERNEL + $INITRD"
exec qemu-system-x86_64 \
    "${ACCEL[@]}" \
    -m 256M \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "$APPEND" \
    "${DISPLAY_ARGS[@]}" \
    "${SERIAL_ARGS[@]}"
