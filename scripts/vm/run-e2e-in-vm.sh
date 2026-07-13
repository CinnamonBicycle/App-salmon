#!/usr/bin/env bash
# Runs App Salmon's e2e suite (`just test-e2e`) inside an ephemeral, disposable QEMU VM, so
# the suite's real requirements — root, system user accounts, /etc/sudoers.d writes, a real
# Docker daemon — land on a throwaway guest instead of this host. See docs/DESIGN.md, "VM e2e
# testing" section.
#
# Host requirements: qemu-system-x86_64 + qemu-img, a cloud-init seed-image tool (cloud-localds,
# genisoimage, mkisofs, or xorriso), this user able to read/write /dev/kvm, and network access to
# fetch the base Ubuntu cloud image once. Run `./scripts/vm/setup-vm-host.sh` once per machine to
# get all of that — it's the *only* place sudo is needed for this path, for two ordinary,
# App-Salmon-agnostic, one-time things (installing qemu, adding you to the `kvm` group), not
# anything specific to this project or repeated per run.
#
# Explicitly NOT required on this host, at all: root, a Docker daemon, or any of the
# useradd/sudoers changes scripts/setup-e2e-env.sh makes — all of that happens only inside the
# disposable guest, which is discarded when this script exits. This is the whole point of this
# script over running `scripts/setup-e2e-env.sh` + `just test-e2e` directly.
#
# Usage: scripts/vm/run-e2e-in-vm.sh [--keep] [--timeout SECONDS]
#   --keep      don't delete the overlay disk / seed image on exit (inspect after a failure)
#   --timeout   max seconds to wait for the VM to finish (default 1800)
#
# Status: written and reviewed (`bash -n` clean; the qemu flags and cloud-init schema used
# here were checked against this machine's installed `qemu-system-x86_64 --help`/`-device
# help`/`-accel help` output and validated with a YAML parser), but NOT boot-verified in any
# session so far — this sandbox has no /dev/kvm access (confirmed: this user isn't in the
# `kvm` group, direct open of /dev/kvm is denied, and there's no passwordless sudo to fix
# either of those). The first session with real KVM access should run this end to end against
# a scratch checkout and fix whatever it finds, the same way §8a of docs/DESIGN.md already
# flags for the e2e suite itself.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/app-salmon-e2e-vm"
IMAGE_NAME="noble-server-cloudimg-amd64.img"
IMAGE_URL="https://cloud-images.ubuntu.com/noble/current/${IMAGE_NAME}"
SHA_URL="https://cloud-images.ubuntu.com/noble/current/SHA256SUMS"

KEEP=0
TIMEOUT=1800
while [ $# -gt 0 ]; do
	case "$1" in
	--keep)
		KEEP=1
		shift
		;;
	--timeout)
		TIMEOUT="$2"
		shift 2
		;;
	*)
		echo "unknown argument: $1" >&2
		exit 1
		;;
	esac
done

echo "== checking host prerequisites =="
command -v qemu-system-x86_64 >/dev/null 2>&1 || {
	echo "qemu-system-x86_64 not found." >&2
	echo "  run: ./scripts/vm/setup-vm-host.sh  (one-time; installs qemu, no other host changes)" >&2
	exit 1
}
command -v qemu-img >/dev/null 2>&1 || {
	echo "qemu-img not found." >&2
	echo "  run: ./scripts/vm/setup-vm-host.sh  (one-time; installs qemu, no other host changes)" >&2
	exit 1
}
[ -r /dev/kvm ] && [ -w /dev/kvm ] || {
	echo "/dev/kvm not accessible by this user." >&2
	echo "  run: ./scripts/vm/setup-vm-host.sh  (one-time; adds you to the kvm group, nothing" >&2
	echo "  else — you'll need to log out and back in afterwards for it to take effect)." >&2
	exit 1
}

SEED_TOOL=""
for candidate in cloud-localds genisoimage mkisofs xorriso; do
	if command -v "$candidate" >/dev/null 2>&1; then
		SEED_TOOL="$candidate"
		break
	fi
done
[ -n "$SEED_TOOL" ] || {
	echo "none of cloud-localds/genisoimage/mkisofs/xorriso found." >&2
	echo "  run: ./scripts/vm/setup-vm-host.sh  (one-time; installs cloud-image-utils)" >&2
	exit 1
}
echo "  ok (seed tool: $SEED_TOOL)"

mkdir -p "$CACHE_DIR"

echo "== base image =="
if [ ! -f "$CACHE_DIR/$IMAGE_NAME" ]; then
	echo "  downloading $IMAGE_URL"
	curl -fL --progress-bar -o "$CACHE_DIR/$IMAGE_NAME.part" "$IMAGE_URL"
	mv "$CACHE_DIR/$IMAGE_NAME.part" "$CACHE_DIR/$IMAGE_NAME"
fi
echo "  verifying checksum against $SHA_URL"
expected="$(curl -fsL "$SHA_URL" | grep " \*${IMAGE_NAME}\$" | cut -d' ' -f1)"
[ -n "$expected" ] || {
	echo "could not find $IMAGE_NAME in SHA256SUMS" >&2
	exit 1
}
actual="$(sha256sum "$CACHE_DIR/$IMAGE_NAME" | cut -d' ' -f1)"
if [ "$expected" != "$actual" ]; then
	echo "  cached image checksum mismatch (expected $expected, got $actual); redownloading" >&2
	rm -f "$CACHE_DIR/$IMAGE_NAME"
	curl -fL --progress-bar -o "$CACHE_DIR/$IMAGE_NAME" "$IMAGE_URL"
	actual="$(sha256sum "$CACHE_DIR/$IMAGE_NAME" | cut -d' ' -f1)"
	[ "$expected" = "$actual" ] || {
		echo "checksum still mismatched after redownload; aborting" >&2
		exit 1
	}
fi
echo "  ok ($actual)"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/app-salmon-e2e-vm.XXXXXX")"
cleanup() {
	if [ "$KEEP" -eq 1 ]; then
		echo "== --keep set, leaving $WORKDIR =="
	else
		rm -rf "$WORKDIR"
	fi
}
trap cleanup EXIT

echo "== overlay disk (base image itself is never modified) =="
qemu-img create -f qcow2 -F qcow2 -b "$CACHE_DIR/$IMAGE_NAME" "$WORKDIR/overlay.qcow2" 20G >/dev/null

echo "== cloud-init seed =="
GUEST_INIT_B64="$(base64 -w0 "$SCRIPT_DIR/guest-init.sh")"
cat >"$WORKDIR/user-data" <<EOF
#cloud-config
hostname: app-salmon-e2e
write_files:
  - path: /root/guest-init.sh
    encoding: b64
    permissions: '0755'
    content: ${GUEST_INIT_B64}
runcmd:
  - [bash, /root/guest-init.sh]
EOF
cat >"$WORKDIR/meta-data" <<EOF
instance-id: app-salmon-e2e-$(date +%s)
local-hostname: app-salmon-e2e
EOF

case "$SEED_TOOL" in
cloud-localds)
	cloud-localds "$WORKDIR/seed.iso" "$WORKDIR/user-data" "$WORKDIR/meta-data"
	;;
*)
	# genisoimage, mkisofs, and `xorriso -as genisoimage` all accept this same argument shape.
	iso_cmd=("$SEED_TOOL")
	[ "$SEED_TOOL" = "xorriso" ] && iso_cmd+=(-as genisoimage)
	"${iso_cmd[@]}" -output "$WORKDIR/seed.iso" -volid cidata -joliet -rock \
		"$WORKDIR/user-data" "$WORKDIR/meta-data" >/dev/null
	;;
esac

RESULT_DIR="$REPO_ROOT/.e2e-vm-result"
rm -rf "$RESULT_DIR"

echo "== booting VM (timeout ${TIMEOUT}s; console log: $WORKDIR/console.log) =="
if ! timeout "$TIMEOUT" qemu-system-x86_64 \
	-machine q35,accel=kvm \
	-cpu host \
	-smp 4 \
	-m 4096 \
	-no-reboot \
	-display none \
	-serial "file:$WORKDIR/console.log" \
	-monitor none \
	-drive file="$WORKDIR/overlay.qcow2",if=virtio,format=qcow2 \
	-drive file="$WORKDIR/seed.iso",if=virtio,format=raw,media=cdrom,readonly=on \
	-netdev user,id=net0 -device virtio-net-pci,netdev=net0 \
	-virtfs local,path="$REPO_ROOT",mount_tag=repo,security_model=none,id=repo0; then
	echo "qemu exited non-zero or timed out; console log at $WORKDIR/console.log" >&2
	[ "$KEEP" -eq 1 ] || echo "  (rerun with --keep to inspect the disk/console after a failure)" >&2
	exit 1
fi

if [ ! -f "$RESULT_DIR/exit_code" ]; then
	echo "VM shut down but $RESULT_DIR/exit_code was never written." >&2
	echo "  guest-init.sh likely failed before reaching 'just test-e2e' — see" >&2
	echo "  $RESULT_DIR/guest-init.log (if present) and $WORKDIR/console.log" >&2
	exit 1
fi

CODE="$(cat "$RESULT_DIR/exit_code")"
echo "== e2e suite exit code: $CODE (log: $RESULT_DIR/test-e2e.log) =="
exit "$CODE"
