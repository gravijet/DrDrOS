# iso/

This directory will hold the **bootable ISO build pipeline**.

Phase 5 brings:

- `build.sh` — runs Buildroot, packages kernel + rootfs, wraps it in an
  ISOLINUX / `grub-mkrescue` bootable image via `xorriso`.
- `boot/` — boot loader assets (GRUB config, splash image).

The output is `drdros.iso`, bootable on any x86_64 PC, VirtualBox, QEMU,
or a Ventoy USB stick.

Nothing here yet — scaffold placeholder.
