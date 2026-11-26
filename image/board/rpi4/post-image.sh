#!/bin/sh
set -e
BOARD_DIR="$(dirname "$0")"
# The Pi's display and Wi-Fi drivers are modules. If depmod couldn't read them
# (missing xz support), modules.dep is empty, udev loads nothing, and the Pi boots
# to a text console with no network. Fail here rather than on the device. The Pi 4
# also needs v3d, its GPU: the image has no software renderer to fall back on.
# A driver built into the kernel (the Pi kernel builds v3d in) is in modules.builtin.
for m in vc4 v3d brcmfmac; do
	grep -qs "/$m\.ko" "$TARGET_DIR"/lib/modules/*/modules.dep ||
		grep -qs "/$m\.ko" "$TARGET_DIR"/lib/modules/*/modules.builtin || {
		echo "post-image: $m.ko is missing from modules.dep (depmod couldn't index the modules)"
		exit 1
	}
done
# The kernel runs /sbin/modprobe to load some modules itself (brcmfmac loads
# brcmfmac-wcc that way): it must be kmod's, since BusyBox's can't read xz modules.
case "$(readlink "$TARGET_DIR/sbin/modprobe")" in
	*busybox*) echo "post-image: /sbin/modprobe is BusyBox's, which can't load xz modules"; exit 1 ;;
esac
support/scripts/genimage.sh -c "$BOARD_DIR/genimage.cfg"
