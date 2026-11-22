################################################################################
#
# idealos-userspace: build the Rust workspace at the root of this repository.
#
################################################################################

IDEALOS_USERSPACE_VERSION = 0.1.0
IDEALOS_USERSPACE_SITE = $(BR2_EXTERNAL_IDEALOS_PATH)/..
IDEALOS_USERSPACE_SITE_METHOD = local
IDEALOS_USERSPACE_LICENSE = MIT or Apache-2.0

# The compositor's KMS backend links libinput (which finds devices through udev)
# and libxkbcommon; the udev crate finds libudev with pkg-config.
IDEALOS_USERSPACE_DEPENDENCIES = host-pkgconf libinput libxkbcommon libudev
# The pkg-config crate refuses to cross-compile unless told to. Buildroot's
# pkg-config (PKG_CONFIG, set by TARGET_CONFIGURE_OPTS) already points at staging.
IDEALOS_USERSPACE_CARGO_ENV = PKG_CONFIG_ALLOW_CROSS=1

# The device has no window system, so build the compositor with only the KMS
# backend. `session` has no features. kms-probe, db-bench and js-bench (Spikes A/B,
# C and D) are dev tools that hw-report runs to measure new hardware.
IDEALOS_USERSPACE_BINS = session compositor kms-probe db-bench js-bench
IDEALOS_USERSPACE_CARGO_BUILD_OPTS = \
	-p session \
	-p compositor --no-default-features --features kms \
	-p kms-probe -p db-bench -p js-bench

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
	$(foreach bin,$(IDEALOS_USERSPACE_BINS),
		$(INSTALL) -D -m 0755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/$(bin) \
			$(TARGET_DIR)/usr/bin/$(bin)
	)
endef

$(eval $(cargo-package))
