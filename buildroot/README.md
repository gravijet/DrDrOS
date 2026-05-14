# buildroot/

This directory holds everything DrDrOS needs to produce the minimal Linux
kernel and root filesystem that sit underneath our userland.

## Layout

| Path | What |
|---|---|
| `upstream/` | Buildroot source, **git submodule pinned to release `2026.02.1`** |
| `drdros_defconfig` | Our Buildroot configuration (target arch, kernel, packages, rootfs format) |

`upstream/` is a git submodule. When you clone DrDrOS, fetch it with:

```sh
git clone --recursive https://github.com/gravijet/DrDrOS
# or, if you already cloned:
git submodule update --init --recursive
```

## What Buildroot produces

Buildroot is a build system that takes our `drdros_defconfig` and emits:

- `bzImage` — the Linux kernel
- `rootfs.cpio.gz` — a gzipped cpio archive of the root filesystem,
  unpacked directly into RAM by the kernel at boot (an initramfs)

These two files are everything we need to boot DrDrOS.

## Building (once the defconfig is complete)

```sh
# Apply our defconfig (writes buildroot/upstream/.config).
make -C buildroot/upstream BR2_DEFCONFIG=$(pwd)/buildroot/drdros_defconfig defconfig

# Cross-compile everything. First run: 20–60 minutes (downloads + builds gcc).
make -C buildroot/upstream
```

Output lands in `buildroot/upstream/output/images/`.

## Updating the pinned Buildroot version

```sh
git -C buildroot/upstream fetch --tags
git -C buildroot/upstream checkout 2026.02.2     # for example
git add buildroot/upstream
git commit -m "buildroot: bump submodule to 2026.02.2"
```
