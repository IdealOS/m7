#!/bin/sh
# Runs inside the idealos-build VM; see tools/check-defconfigs.
set -eu
cd "$HOME"
br="buildroot-$BUILDROOT_VERSION"
if [ ! -d "$br" ]; then
	wget -q "https://buildroot.org/downloads/$br.tar.gz"
	tar xf "$br.tar.gz"
fi

failed=0
for defconfig in idealos/image/configs/*_defconfig; do
	name=$(basename "$defconfig")
	out=$(mktemp -d)
	make -s -C "$br" O="$out" BR2_EXTERNAL="$HOME/idealos/image" "$name" >/dev/null 2>&1
	# Strings are written back with $(...) unexpanded, so kept lines match exactly.
	dropped=$(grep -E '^BR2_[A-Z0-9_]+=' "$defconfig" | while read -r line; do
		grep -qxF "$line" "$out/.config" || echo "  $line"
	done)
	legacy=$(grep -qx "BR2_LEGACY=y" "$out/.config" && echo yes || true)
	rm -rf "$out"
	if [ -n "$legacy" ]; then
		echo "$name: uses options Buildroot has renamed or removed (see Config.in.legacy)"
		failed=1
	fi
	if [ -n "$dropped" ]; then
		echo "$name: Buildroot dropped these options (unmet dependencies?):"
		echo "$dropped"
		failed=1
	elif [ -z "$legacy" ]; then
		echo "$name: ok"
	fi
done
exit "$failed"
