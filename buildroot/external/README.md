# buildroot/external — DrDrOS BR2_EXTERNAL tree

This directory is Buildroot's [BR2_EXTERNAL][br-ext] mechanism: a sibling
tree that adds DrDrOS-specific packages on top of the upstream Buildroot
in `../upstream/`. Buildroot resolves it via the `BR2_EXTERNAL=` variable
passed at `make` time.

[br-ext]: https://buildroot.org/downloads/manual/manual.html#outside-br-custom

## Layout

```
external/
├── external.desc            ← name + one-line description (required)
├── external.mk              ← include-glob for all package .mk files
├── Config.in                ← Kconfig menu entry point
└── package/
    └── drdr-init/
        ├── Config.in        ← menuconfig entry for drdr-init
        └── drdr-init.mk     ← cross-compile + install recipe
```

## Packages

| Package      | Purpose                                       |
| ------------ | --------------------------------------------- |
| `drdr-init`  | DrDrOS custom PID 1 (cross-compiled to musl). |

## Build recipe

```sh
make -C buildroot/upstream \
     BR2_EXTERNAL=$(pwd)/buildroot/external \
     BR2_DEFCONFIG=$(pwd)/buildroot/drdros_defconfig \
     defconfig
make -C buildroot/upstream
```

The first `defconfig` invocation also stores `BR2_EXTERNAL` in
`output/.br-external.mk`, so later `make` calls (incremental rebuilds)
don't need the variable repeated.

## Notes

- `drdr-init` is compiled with the host's `rustup` toolchain targeting
  `x86_64-unknown-linux-musl`. The rustup target bundles its own musl
  libc, so we deliberately bypass Buildroot's musl. Run
  `rustup target add x86_64-unknown-linux-musl` once on the host before
  building.
- The package recipe references the workspace via
  `$(BR2_EXTERNAL_DRDROS_PATH)/../..`, which resolves to the repo root.
- Buildroot installs `drdr-init` at both `/sbin/drdr-init` and `/init`
  (the kernel runs `/init` at boot from the initramfs).
