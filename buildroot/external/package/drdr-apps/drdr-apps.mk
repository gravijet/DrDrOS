################################################################################
#
# drdr-apps — DrDrDesk + DrDrShell + DrDrFiles + DrDrEdit
#
# One Buildroot package that fans out into the userland Rust binaries.
# Building them as a single package keeps the host-side cargo invocation
# efficient (one workspace, one resolver pass) and the Buildroot config
# tidy. DrDrDesk is the graphical session drdr-init supervises; the other
# three are the apps it launches.
#
################################################################################

DRDR_APPS_VERSION  = 0.1.0
DRDR_SRC_MIRROR   ?= $(HOME)/.cache/drdros-src
DRDR_APPS_SITE     = $(DRDR_SRC_MIRROR)
DRDR_APPS_SITE_METHOD = local
DRDR_APPS_LICENSE  = MIT OR Apache-2.0
DRDR_APPS_LICENSE_FILES = LICENSE-MIT LICENSE-APACHE

DRDR_APPS_TARGET_DIR ?= $(HOME)/.cache/drdros-target
DRDR_APPS_OUT = $(DRDR_APPS_TARGET_DIR)/x86_64-unknown-linux-musl/release

define DRDR_APPS_BUILD_CMDS
	cd $(DRDR_APPS_SITE) && \
		cargo build --release \
			--target x86_64-unknown-linux-musl \
			-p drdr-desk -p drdr-shell -p drdr-files -p drdr-edit
endef

define DRDR_APPS_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(DRDR_APPS_OUT)/drdr-desk  $(TARGET_DIR)/bin/drdr-desk
	$(INSTALL) -D -m 0755 $(DRDR_APPS_OUT)/drdr-shell $(TARGET_DIR)/bin/drdr-shell
	$(INSTALL) -D -m 0755 $(DRDR_APPS_OUT)/drdr-files $(TARGET_DIR)/bin/drdr-files
	$(INSTALL) -D -m 0755 $(DRDR_APPS_OUT)/drdr-edit  $(TARGET_DIR)/bin/drdr-edit
endef

$(eval $(generic-package))
