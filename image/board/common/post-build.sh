#!/bin/sh
# Runs after packages are installed, before the root filesystem image is made.
set -e
TARGET_DIR="$1"
BOARD_COMMON="$(dirname "$0")"
mkdir -p "$TARGET_DIR/data" "$TARGET_DIR/opt/idealos/bin"
# Phase 0 only builds dev images. A release defconfig will drop this marker.
touch "$TARGET_DIR/etc/idealos/dev-image"

# The root filesystem is read-only, so Buildroot points /etc/dropbear at /var/run
# and dropbear makes new host keys on every boot. Keep them on /data instead (S05data
# creates the directory) so the host key, and ssh's known_hosts entry, stay valid.
ln -snf /data/etc/dropbear "$TARGET_DIR/etc/dropbear"

# tools/build-image puts the public half of image/output/dev-key here, so
# tools/deploy-to-pi can log in as root. It is not in git.
if [ -f "$BOARD_COMMON/dev-key.pub" ]; then
	install -d -m 0700 "$TARGET_DIR/root/.ssh"
	install -m 0600 "$BOARD_COMMON/dev-key.pub" "$TARGET_DIR/root/.ssh/authorized_keys"
else
	rm -f "$TARGET_DIR/root/.ssh/authorized_keys"
fi
