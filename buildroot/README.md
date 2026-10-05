# Buildroot

Buildroot produces the Linux kernel and root filesystem used by DrDrOS. `upstream/` is a pinned submodule; `drdros_defconfig` contains the build configuration.

```sh
git submodule update --init --recursive
scripts/build-buildroot.sh
```

Run the build as your regular user. The script uses a cache directory on a filesystem with Unix permissions and links `bzImage` and `rootfs.cpio.gz` into `buildroot/images/`.

Use `scripts/build-buildroot.sh menuconfig` to inspect settings, or `scripts/build-buildroot.sh kernel` to rebuild after changing kernel fragments.

See [external packages](external/README.md) and [ISO generation](../iso/README.md).
