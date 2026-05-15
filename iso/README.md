# iso/ — DrDrOS bootable ISO pipeline

`iso/build.sh` wraps the Buildroot output (`bzImage` + `rootfs.cpio.gz`)
into a hybrid ISO that boots:

- from a real CD/DVD,
- from a USB stick (`dd if=drdros.iso of=/dev/sdX`),
- as a Ventoy payload,
- under any VM that takes `-cdrom` (QEMU, VirtualBox, VMware, ...).

The bootloader is **GRUB**, packaged via `grub-mkrescue` — no isolinux
dependency. EFI works out of the box; legacy BIOS works too as long as
`grub-pc-bin` is installed alongside `grub-common`.

## One-shot

```sh
scripts/build-buildroot.sh   # bzImage + rootfs.cpio.gz → buildroot/images/
bash iso/build.sh            # → iso/drdros.iso
scripts/qemu.sh --iso        # boot the ISO under QEMU (legacy BIOS)
scripts/qemu.sh --iso --uefi # ...or under UEFI (OVMF); needed if the
                             #    ISO was built without grub-pc-bin
```

> **BIOS vs UEFI:** `grub-mkrescue` embeds a legacy-BIOS boot image
> only when `grub-pc-bin` is installed. Without it the ISO is
> **UEFI-only** — `scripts/qemu.sh --iso` (SeaBIOS) will say "No
> bootable device"; use `--uefi` (needs the `ovmf` package), or
> install `grub-pc-bin` and rebuild for a dual-firmware ISO.

## Custom paths

```sh
bash iso/build.sh \
    --bzimage path/to/bzImage \
    --rootfs  path/to/rootfs.cpio.gz \
    --output  /tmp/drdros.iso
```

## Layout that GRUB sees inside the ISO

```
/boot/
├── bzImage              ← Linux kernel
├── rootfs.cpio.gz       ← DrDrOS initramfs (runs entirely in RAM)
└── grub/
    └── grub.cfg         ← two boot entries: quiet + verbose (serial)
```

## ISO requirements

Installed by default on most Ubuntu / Debian desktop installs:

- `grub-common` (provides `grub-mkrescue`)
- `xorriso`
- `mtools` — provides `mformat`/`mcopy`. `grub-mkrescue` uses these to
  build the embedded **EFI System Partition** (a FAT image holding the
  EFI bootloader). Missing it fails with `mformat invocation failed`
  and *no ISO is written*. Headless server images often omit it.

For legacy BIOS booting also install:

- `grub-pc-bin`

One-liner for Debian/Ubuntu:

```sh
sudo apt-get install -y grub-common xorriso mtools grub-pc-bin
```
