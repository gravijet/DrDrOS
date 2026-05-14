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
IMAGES="$REPO_ROOT/buildroot/upstream/output/images"
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
        *)
            echo "usage: $0 [--headless] [--kvm]" >&2
            exit 2
            ;;
    esac
done

echo "[qemu.sh] booting $KERNEL + $INITRD"
exec qemu-system-x86_64 \
    "${ACCEL[@]}" \
    -m 256M \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "$APPEND" \
    "${DISPLAY_ARGS[@]}" \
    "${SERIAL_ARGS[@]}"
