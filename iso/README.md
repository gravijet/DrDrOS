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
scripts/qemu.sh --iso        # boot the ISO under QEMU
```

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

For legacy BIOS booting also install:

- `grub-pc-bin`
