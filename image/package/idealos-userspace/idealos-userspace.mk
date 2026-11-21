################################################################################
#
# idealos-userspace: build the Rust workspace at the root of this repository.
#
################################################################################

IDEALOS_USERSPACE_VERSION = 0.1.0
IDEALOS_USERSPACE_SITE = $(BR2_EXTERNAL_IDEALOS_PATH)/..
IDEALOS_USERSPACE_SITE_METHOD = local
IDEALOS_USERSPACE_LICENSE = MIT or Apache-2.0

# The device has no window system, so build the compositor with only the KMS
# backend. `session` has no features.
IDEALOS_USERSPACE_CARGO_BUILD_OPTS = \
	-p session \
	-p compositor --no-default-features --features kms

define IDEALOS_USERSPACE_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/session \
		$(TARGET_DIR)/usr/bin/session
	$(INSTALL) -D -m 0755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/compositor \
		$(TARGET_DIR)/usr/bin/compositor
endef

$(eval $(cargo-package))
