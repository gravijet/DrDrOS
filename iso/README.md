# Boot image

`iso/build.sh` packages the Linux kernel and initramfs into a GRUB ISO for virtual machines or USB booting.

## Build and run

On Debian or Ubuntu, install:

```sh
sudo apt-get install grub-common grub-efi-amd64-bin grub-pc-bin xorriso mtools
```

Then run from the repository root:

```sh
scripts/build-buildroot.sh
bash iso/build.sh
scripts/qemu.sh --iso
```

The output is `iso/drdros.iso`. Use `scripts/qemu.sh --iso --uefi` for UEFI testing; this requires OVMF. Without `grub-pc-bin`, the ISO supports UEFI only.

## Custom input files

```sh
bash iso/build.sh \
    --bzimage path/to/bzImage \
    --rootfs path/to/rootfs.cpio.gz \
    --output /tmp/drdros.iso
```

## Boot problems

The GRUB image is unsigned, so firmware configured to require signed bootloaders will reject it. If the desktop reaches a black screen, try the **safe graphics** entry in the GRUB menu. The verbose entry shows additional boot logs.

Hardware support is still limited. Test images in a virtual machine first.
