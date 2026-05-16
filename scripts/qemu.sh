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
UEFI=0
# KVM is auto-enabled below when /dev/kvm exists. Without hardware
# virtualisation QEMU emulates every instruction in software, and
# DrDrDesk's full-scene repaint is heavy enough that the mouse lags
# badly — so KVM is the default, not an opt-in. `--no-kvm` forces the
# slow pure-emulation path (e.g. to reproduce a TCG-only bug).
NO_KVM=0
# QEMU's default `pc` machine exposes only a PS/2 keyboard via i8042 and
# (in this kernel/QEMU combo) no pointer device at all — DrDrDesk then
# runs keyboard-only and the mouse is dead. Give the VM a USB mouse on
# an explicit xHCI controller (the legacy `-usb` PIIX UHCI proved
# flaky here). `usb-mouse` is a *relative* HID pointer (EV_REL),
# exactly what PointerReader decodes. The kernel has USB_XHCI_HCD +
# USB_HID + HID_GENERIC, so usbhid binds it as a normal
# /dev/input/eventN.
INPUT_ARGS=(-device qemu-xhci,id=xhci -device usb-mouse,bus=xhci.0)

# Parse flags.
for arg in "$@"; do
    case "$arg" in
        --headless)
            DISPLAY_ARGS=(-nographic -vga std)
            SERIAL_ARGS=()  # -nographic already routes serial to stdio
            APPEND="console=ttyS0 loglevel=4"
            ;;
        --kvm)
            # Kept for compatibility / explicitness; KVM is now the
            # default whenever /dev/kvm exists (see below).
            ;;
        --no-kvm)
            NO_KVM=1
            ;;
        --iso)
            ISO_PATH="$REPO_ROOT/iso/drdros.iso"
            ;;
        --uefi)
            # Boot the ISO under UEFI (OVMF) instead of the default
            # SeaBIOS. Required when the ISO has only a UEFI El Torito
            # image — i.e. iso/build.sh ran without grub-pc-bin, so no
            # legacy-BIOS boot image was embedded.
            UEFI=1
            ;;
        *)
            echo "usage: $0 [--headless] [--kvm] [--no-kvm] [--iso] [--uefi]" >&2
            exit 2
            ;;
    esac
done

# Enable KVM by default when the host can (massive speed-up; DrDrDesk's
# repaint is unusably laggy under pure TCG emulation). --no-kvm opts out.
if [[ $NO_KVM -eq 0 && -e /dev/kvm && -r /dev/kvm && -w /dev/kvm ]]; then
    ACCEL=(-enable-kvm -cpu host)
    echo "[qemu.sh] KVM acceleration enabled"
elif [[ $NO_KVM -eq 1 ]]; then
    echo "[qemu.sh] KVM disabled (--no-kvm) — expect slow software emulation" >&2
else
    echo "[qemu.sh] /dev/kvm not usable — falling back to slow software emulation" >&2
fi

# --iso boots from the GRUB ISO instead of -kernel/-initrd, so we
# exercise the full boot chain like real hardware does.
if [[ -n $ISO_PATH ]]; then
    if [[ ! -f $ISO_PATH ]]; then
        echo "error: $ISO_PATH not found — run iso/build.sh first" >&2
        exit 1
    fi
    FIRMWARE_ARGS=()
    if [[ $UEFI -eq 1 ]]; then
        # Locate an OVMF build. The split CODE/VARS layout is the modern
        # one; OVMF.fd is the older single-file fallback. VARS is NVRAM,
        # so QEMU must be able to write it — copy to a private temp file.
        OVMF_CODE=""
        for c in /usr/share/OVMF/OVMF_CODE_4M.fd \
                 /usr/share/OVMF/OVMF_CODE.fd \
                 /usr/share/ovmf/OVMF.fd; do
            [[ -f $c ]] && { OVMF_CODE="$c"; break; }
        done
        if [[ -z $OVMF_CODE ]]; then
            echo "error: --uefi needs OVMF — install the 'ovmf' package" >&2
            exit 1
        fi
        OVMF_VARS_SRC="${OVMF_CODE%CODE*}VARS${OVMF_CODE##*CODE}"
        [[ -f $OVMF_VARS_SRC ]] || OVMF_VARS_SRC="$OVMF_CODE"
        VARS_TMP="$(mktemp -t drdros-ovmf-vars.XXXXXX.fd)"
        trap 'rm -f "$VARS_TMP"' EXIT
        cp "$OVMF_VARS_SRC" "$VARS_TMP"
        FIRMWARE_ARGS=(
            -drive "if=pflash,format=raw,unit=0,readonly=on,file=$OVMF_CODE"
            -drive "if=pflash,format=raw,unit=1,file=$VARS_TMP"
        )
        echo "[qemu.sh] UEFI firmware: $OVMF_CODE"
    fi
    echo "[qemu.sh] booting from ISO $ISO_PATH"
    # No `exec`: the EXIT trap that cleans up the OVMF vars copy must run.
    qemu-system-x86_64 \
        "${ACCEL[@]}" \
        "${FIRMWARE_ARGS[@]}" \
        -m 256M \
        -cdrom "$ISO_PATH" \
        -boot d \
        "${INPUT_ARGS[@]}" \
        "${DISPLAY_ARGS[@]}" \
        "${SERIAL_ARGS[@]}"
    exit $?
fi

echo "[qemu.sh] booting $KERNEL + $INITRD"
exec qemu-system-x86_64 \
    "${ACCEL[@]}" \
    -m 256M \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "$APPEND" \
    "${INPUT_ARGS[@]}" \
    "${DISPLAY_ARGS[@]}" \
    "${SERIAL_ARGS[@]}"
