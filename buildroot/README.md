# buildroot/

This directory will hold the **Buildroot** configuration that produces the
minimal Linux kernel + tiny rootfs underneath DrDrOS.

Buildroot is a build system that takes a config file and emits:

- `bzImage` — the Linux kernel
- `rootfs.cpio.gz` — the minimal root filesystem we drop our DrDr binaries into

Phase 1 will add `drdros_defconfig` and instructions for invoking it:

```sh
make -C buildroot drdros_defconfig
make -C buildroot
```

Nothing here yet — scaffold placeholder.
