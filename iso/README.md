# iso/ — DrDrOS bootable ISO pipeline

`iso/build.sh` wraps the Buildroot output (`bzImage` + `rootfs.cpio.gz`)
into a hybrid ISO that boots:

- from a real CD/DVD,
- from a USB stick (`dd if=drdros.iso of=/dev/sdX`),
- as a Ventoy payload,
- under any VM that takes `-cdrom` (QEMU, VirtualBox, VMware, ...).

The bootloader is **GRUB**, packaged via `grub-mkrescue` — no isolinux
dependency. EFI works out of the box (needs `grub-efi-amd64-bin`); legacy
BIOS works too as long as `grub-pc-bin` is installed alongside
`grub-common`.

The generated `grub.cfg` is hardened for real UEFI hardware:
`search --file` re-finds the volume holding the kernel (so Ventoy / a
USB stick whose layout the firmware remapped still boots), `all_video` +
`gfxpayload=keep` hand the live GOP framebuffer to the kernel (no black
screen on efifb/simpledrm), and a **"safe graphics"** menu entry
(`nomodeset video=efifb`) is there for panels where the KMS driver
misbehaves.

## ⚠ It boots in QEMU but not on my Surface / ThinkPad

Ninety-nine times out of a hundred this is **Secure Boot**, not the ISO.
`grub-mkrescue` produces an *unsigned* GRUB binary, and UEFI firmware
refuses to launch unsigned bootloaders while Secure Boot is on — the
machine shows "No bootable device" or jumps straight back to Windows,
looking exactly like the media isn't bootable.

Disable Secure Boot, then pick the USB stick from the firmware boot menu:

- **Surface Go 2** — power off. Hold **Volume-Up**, tap **Power**, keep
  holding Volume-Up → UEFI menu → **Security → Secure Boot → Disabled**
  (or "Microsoft & 3rd party CA"). Then hold **Volume-Down** + tap
  **Power** to boot from USB.
- **ThinkPad T14** — tap **Enter** (then **F1**) at the logo → **Security
  → Secure Boot → Disabled**; make sure **Boot Mode = UEFI** (not
  Legacy/CSM). **F12** at boot to pick the USB stick.

Other things that look like "won't boot":

- **Ventoy:** if normal mode chainloads to a blank screen, press
  **Ctrl-r** on the Ventoy menu to use **GRUB2 Mode**, or just `dd` the
  ISO straight to a dedicated stick.
- **BIOS-only ISO on a UEFI machine:** if you built without
  `grub-efi-amd64-bin` the ISO has no EFI image and a UEFI-only laptop
  won't see it. `iso/build.sh` now errors out up front if that package
  is missing.

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
    └── grub.cfg         ← three entries: desktop · safe-graphics · verbose
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

For UEFI booting (Surface Go 2, ThinkPad T14, every modern PC):

- `grub-efi-amd64-bin` — the x86_64-efi GRUB modules. Without it
  `grub-mkrescue` builds a BIOS-only ISO that a UEFI-only machine cannot
  boot at all; `iso/build.sh` errors out if it is missing.

One-liner for Debian/Ubuntu:

```sh
sudo apt-get install -y grub-common grub-efi-amd64-bin xorriso mtools grub-pc-bin
```
