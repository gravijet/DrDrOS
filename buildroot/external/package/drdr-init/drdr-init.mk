################################################################################
#
# drdr-init — DrDrOS custom PID 1
#
# Builds the binary OUTSIDE Buildroot using the host's rustup toolchain
# targeting x86_64-unknown-linux-musl (musl is bundled with the rustup
# target, so we don't need to wire it to Buildroot's libc). The compiled
# binary is then installed into TARGET_DIR as both /sbin/drdr-init and
# /init — the kernel runs /init at boot, and our binary is happy with
# either path.
#
################################################################################

DRDR_INIT_VERSION  = 0.1.0
DRDR_INIT_SITE     = $(BR2_EXTERNAL_DRDROS_PATH)/../..
DRDR_INIT_SITE_METHOD = local
DRDR_INIT_LICENSE  = MIT OR Apache-2.0
DRDR_INIT_LICENSE_FILES = LICENSE-MIT LICENSE-APACHE

# Where cargo drops the cross-compiled binary. .cargo/config.toml at the
# repo root redirects target-dir to $HOME/.cache/drdros-target (the
# in-tree target/ dir doesn't work on NTFS — build scripts lose exec
# bits). Override DRDR_INIT_TARGET_DIR if you've moved the cache.
DRDR_INIT_TARGET_DIR ?= $(HOME)/.cache/drdros-target
DRDR_INIT_BIN = $(DRDR_INIT_TARGET_DIR)/x86_64-unknown-linux-musl/release/drdr-init

define DRDR_INIT_BUILD_CMDS
	cd $(DRDR_INIT_SITE) && \
		cargo build --release \
			--target x86_64-unknown-linux-musl \
			-p drdr-init
endef

define DRDR_INIT_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(DRDR_INIT_BIN) $(TARGET_DIR)/sbin/drdr-init
	ln -sf sbin/drdr-init $(TARGET_DIR)/init
endef

$(eval $(generic-package))
