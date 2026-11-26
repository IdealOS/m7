#!/bin/sh
set -e
BOARD_DIR="$(dirname "$0")"
# The Pi's display and Wi-Fi drivers are modules. If depmod couldn't read them
# (missing xz support), modules.dep is empty, udev loads nothing, and the Pi boots
# to a text console with no network. Fail here rather than on the device.
for m in vc4 brcmfmac; do
	grep -qs "/$m\.ko" "$TARGET_DIR"/lib/modules/*/modules.dep || {
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
