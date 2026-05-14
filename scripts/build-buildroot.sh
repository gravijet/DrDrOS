#!/usr/bin/env bash
# scripts/build-buildroot.sh — full Buildroot build for DrDrOS.
#
# Buildroot's source tree must live on a filesystem that preserves Unix
# exec bits (so its build scripts and freshly compiled toolchain binaries
# can run). The NTFS partition this repo lives on does NOT preserve those
# bits, so we keep the build directory at $HOME/.cache/drdros-buildroot
# (ext4) and pull `BR2_DEFCONFIG` + `BR2_EXTERNAL` from this repo.
#
# Usage:
#   scripts/build-buildroot.sh defconfig    # generate .config from drdros_defconfig
#   scripts/build-buildroot.sh              # full build → bzImage + rootfs.cpio.gz
#   scripts/build-buildroot.sh menuconfig   # interactive Kconfig
#   scripts/build-buildroot.sh clean        # wipe output/
#
# After a successful build:
#   - Kernel image:  $BR/output/images/bzImage
#   - Initramfs:     $BR/output/images/rootfs.cpio.gz
#   Both are symlinked into ./buildroot/images/ for `scripts/qemu.sh`.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BR="${BR:-$HOME/.cache/drdros-buildroot}"
DEFCONFIG="$REPO_ROOT/buildroot/drdros_defconfig"
EXTERNAL="$REPO_ROOT/buildroot/external"
# Buildroot's `local` site method rsyncs DRDR_*_SITE every build. The
# real repo lives on NTFS and may contain corrupted output/ trees from
# killed builds, so we maintain a clean ext4 mirror and point the
# recipes at it (DRDR_*_SITE in buildroot/external/package/*.mk).
SRC_MIRROR="${SRC_MIRROR:-$HOME/.cache/drdros-src}"

# Clone Buildroot at the pinned tag if the build dir doesn't exist.
if [[ ! -d $BR ]]; then
    echo "[build-buildroot] cloning Buildroot 2026.02.1 into $BR"
    git clone --depth=1 --branch 2026.02.1 \
        https://git.buildroot.net/buildroot "$BR"
fi

# Sync the Rust workspace into the ext4 mirror, excluding things that
# would balloon the rsync (buildroot/upstream's own output, the cargo
# target dir, git metadata). Each Buildroot run will rsync this mirror
# into output/build/drdr-*; keeping it slim makes that step fast.
sync_mirror() {
    echo "[build-buildroot] syncing $REPO_ROOT → $SRC_MIRROR"
    mkdir -p "$SRC_MIRROR"
    rsync -a --delete \
        --exclude '.git' \
        --exclude 'target' \
        --exclude 'buildroot/upstream/output' \
        --exclude 'buildroot/upstream/dl' \
        --exclude 'buildroot/images' \
        --exclude 'iso/build' \
        --exclude 'iso/*.iso' \
        "$REPO_ROOT/" "$SRC_MIRROR/"
}

cd "$BR"

target="${1:-all}"
case "$target" in
    defconfig|menuconfig|savedefconfig|clean|distclean)
        make BR2_DEFCONFIG="$DEFCONFIG" BR2_EXTERNAL="$EXTERNAL" "$target"
        ;;
    all|"")
        sync_mirror
        # Defconfig first if .config is missing.
        if [[ ! -f .config ]]; then
            make BR2_DEFCONFIG="$DEFCONFIG" BR2_EXTERNAL="$EXTERNAL" defconfig
        fi
        # Use one less than all cores so the laptop stays responsive.
        jobs=$(($(nproc) - 1))
        [[ $jobs -lt 1 ]] && jobs=1
        echo "[build-buildroot] building with -j$jobs"
        make BR2_EXTERNAL="$EXTERNAL" -j"$jobs"
        ;;
    *)
        echo "usage: $0 [defconfig|menuconfig|all|clean]" >&2
        exit 2
        ;;
esac

# Link the images into the repo so qemu.sh can find them.
if [[ -f $BR/output/images/bzImage && -f $BR/output/images/rootfs.cpio.gz ]]; then
    mkdir -p "$REPO_ROOT/buildroot/images"
    ln -sf "$BR/output/images/bzImage"      "$REPO_ROOT/buildroot/images/bzImage"
    ln -sf "$BR/output/images/rootfs.cpio.gz" "$REPO_ROOT/buildroot/images/rootfs.cpio.gz"
    echo "[build-buildroot] images:"
    ls -la "$REPO_ROOT/buildroot/images/"
fi
