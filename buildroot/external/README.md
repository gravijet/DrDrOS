# DrDrOS Buildroot packages

The `BR2_EXTERNAL` tree adds `drdr-init` and `drdr-apps` to Buildroot. `scripts/build-buildroot.sh` supplies this tree and the DrDrOS configuration.

The packages use the host's Rust toolchain with the musl target:

```sh
rustup target add x86_64-unknown-linux-musl
```

For a direct build from the repository root:

```sh
make -C buildroot/upstream \
    BR2_EXTERNAL=$(pwd)/buildroot/external \
    BR2_DEFCONFIG=$(pwd)/buildroot/drdros_defconfig defconfig
make -C buildroot/upstream
```

`drdr-init` is installed as `/init` and `/sbin/drdr-init`. Package recipes resolve workspace sources relative to `BR2_EXTERNAL_DRDROS_PATH`.
