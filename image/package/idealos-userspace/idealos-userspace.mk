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
# backend. `session` has no features. kms-probe (Spikes A and B) is a dev tool for
# checking scanout on real hardware.
IDEALOS_USERSPACE_CARGO_BUILD_OPTS = \
	-p session \
	-p compositor --no-default-features --features kms \
	-p kms-probe

# Buildroot vendors crates in its download step, which local packages skip, and
# then builds with --offline. Vendor here instead, using Cargo.lock, before the
# build runs. cargo vendor prints the source replacement config that makes the
# offline build use ./vendor.
define IDEALOS_USERSPACE_VENDOR_CRATES
	mkdir -p $(@D)/.cargo
	cd $(@D) && PATH=$(BR_PATH) CARGO_HOME=$(BR_CARGO_HOME) \
		cargo vendor --locked --versioned-dirs vendor > $(@D)/.cargo/config.toml
endef
IDEALOS_USERSPACE_PRE_BUILD_HOOKS += IDEALOS_USERSPACE_VENDOR_CRATES

define IDEALOS_USERSPACE_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/session \
		$(TARGET_DIR)/usr/bin/session
	$(INSTALL) -D -m 0755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/compositor \
		$(TARGET_DIR)/usr/bin/compositor
	$(INSTALL) -D -m 0755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/kms-probe \
		$(TARGET_DIR)/usr/bin/kms-probe
endef

$(eval $(cargo-package))
