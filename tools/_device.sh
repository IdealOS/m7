# Shared by scripts that log in to a running dev image as root. Source, do not run.
# Set REPO, HOST and PORT first. KNOWN_HOSTS may override where host keys are kept.
KEY="$REPO/image/output/dev-key"
KNOWN_HOSTS="${KNOWN_HOSTS:-$REPO/image/output/known_hosts}"

[ -f "$KEY" ] || { echo "No $KEY. tools/build-image makes it; rebuild and reflash the image."; exit 1; }

remote() {
	ssh -i "$KEY" -p "$PORT" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 \
		-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$KNOWN_HOSTS" \
		"root@$HOST" "$@"
}
