#!/bin/sh
# Runs after packages are installed, before the root filesystem image is made.
set -e
TARGET_DIR="$1"
mkdir -p "$TARGET_DIR/data" "$TARGET_DIR/opt/idealos/bin"
# Phase 0 only builds dev images. A release defconfig will drop this marker.
touch "$TARGET_DIR/etc/idealos/dev-image"
