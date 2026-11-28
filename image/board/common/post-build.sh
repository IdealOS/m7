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

# NetworkManager keeps saved networks (with their passwords) and its state on
# /data too (S05data creates the directories).
if [ -d "$TARGET_DIR/etc/NetworkManager" ]; then
	rm -rf "$TARGET_DIR/etc/NetworkManager/system-connections" "$TARGET_DIR/var/lib/NetworkManager"
	mkdir -p "$TARGET_DIR/var/lib"
	ln -snf /data/etc/NetworkManager/system-connections "$TARGET_DIR/etc/NetworkManager/system-connections"
	ln -snf /data/var/lib/NetworkManager "$TARGET_DIR/var/lib/NetworkManager"
fi

# Name lookup: NetworkManager writes the resolver config it got from DHCP to
# /run/NetworkManager/resolv.conf (the root filesystem is read-only), so point
# /etc/resolv.conf there. Buildroot's skeleton points it at /run/resolv.conf,
# which nothing writes.
ln -snf ../run/NetworkManager/resolv.conf "$TARGET_DIR/etc/resolv.conf"

# browser-host (the web service: Servo) builds natively in the VM, not as a
# Buildroot package; tools/build-browser-host leaves it here.
if [ -x "$HOME/browser-host/browser-host" ]; then
	install -D -m 0755 "$HOME/browser-host/browser-host" "$TARGET_DIR/usr/bin/browser-host"
else
	echo "post-build: no ~/browser-host/browser-host (run tools/build-browser-host); the image has no web service" >&2
	rm -f "$TARGET_DIR/usr/bin/browser-host"
fi

# tools/build-image puts the public half of image/output/dev-key here, so
# tools/deploy-to-pi can log in as root. It is not in git.
if [ -f "$BOARD_COMMON/dev-key.pub" ]; then
	install -d -m 0700 "$TARGET_DIR/root/.ssh"
	install -m 0600 "$BOARD_COMMON/dev-key.pub" "$TARGET_DIR/root/.ssh/authorized_keys"
else
	rm -f "$TARGET_DIR/root/.ssh/authorized_keys"
fi
