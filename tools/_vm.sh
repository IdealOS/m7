# Shared helpers for scripts that drive the idealos-build VM. Source, do not run.
VM=idealos-build
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SSH_CONFIG="$HOME/.lima/$VM/ssh.config"
# 2026.08 or newer: the 2026.02 LTS branch ships Rust 1.88, older than the workspace needs.
BUILDROOT_VERSION="${BUILDROOT_VERSION:-2026.08.2}"

vm_require() {
	command -v limactl >/dev/null || { echo "Lima is not installed. Run: brew install lima"; exit 1; }
	limactl list -q 2>/dev/null | grep -qx "$VM" || { echo "No build VM. Run: tools/setup-build-vm"; exit 1; }
	limactl start "$VM" >/dev/null 2>&1 || true
}

# Copy the repo to ~/idealos inside the VM (a path without spaces).
vm_sync() {
	rsync -a --delete -e "ssh -F $SSH_CONFIG" \
		--exclude target --exclude node_modules --exclude image/output --exclude .git \
		"$REPO/" "lima-$VM:idealos/"
}

vm_run() {
	limactl shell "$VM" bash -lc "$1"
}

vm_fetch() { # vm_fetch REMOTE_DIR LOCAL_DIR
	mkdir -p "$2"
	rsync -a -e "ssh -F $SSH_CONFIG" "lima-$VM:$1/" "$2/"
}
