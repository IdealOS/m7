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
# backend. `session`, `busd` and `osctl` have no features. kms-probe, db-bench and
# js-bench (Spikes A/B, C and D) and store-bench (the db crate's benchmark) are dev
# tools that hw-report runs to measure new hardware.
IDEALOS_USERSPACE_BINS = session busd dbd permd secretsd ptyd netd backupd osctl app-host compositor kms-probe db-bench js-bench store-bench
IDEALOS_USERSPACE_CARGO_BUILD_OPTS = \
	-p session -p busd -p dbd -p permd -p secretsd -p ptyd -p netd -p backupd -p osctl -p db -p app-host \
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

# The apps that ship with the system, bundled on the host before the build
# (tools/build-image runs tools/build-app.ts; the VM has no Node). At boot,
# `app-host --install` puts them in the database and launches the autostart ones.
define IDEALOS_USERSPACE_INSTALL_TARGET_CMDS
	$(foreach bin,$(IDEALOS_USERSPACE_BINS),
		$(INSTALL) -D -m 0755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/$(bin) \
			$(TARGET_DIR)/usr/bin/$(bin)
	)
	for d in $(@D)/apps/*/dist; do \
		[ -f $$d/manifest.json ] && [ -f $$d/app.js ] || continue; \
		name=$$(basename $$(dirname $$d)); \
		$(INSTALL) -D -m 0644 $$d/app.js $(TARGET_DIR)/usr/share/idealos/apps/$$name/app.js; \
		$(INSTALL) -D -m 0644 $$d/manifest.json $(TARGET_DIR)/usr/share/idealos/apps/$$name/manifest.json; \
	done
endef

$(eval $(cargo-package))
