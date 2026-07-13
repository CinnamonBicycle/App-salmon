#!/usr/bin/env bash
# Runs INSIDE the disposable e2e VM, as root, via cloud-init's runcmd — not meant to be run
# anywhere else. Installs what the e2e suite needs, runs it against the repo shared in from
# the host over a 9p mount, and leaves a result (exit code + logs) on that same share for the
# host script to read back after the VM powers off.
#
# Status: written and reviewed (`bash -n` clean; shellcheck was not available in the sandbox
# this was written in, so that check hasn't run), but not boot-verified — see
# scripts/vm/run-e2e-in-vm.sh's header and docs/DESIGN.md for why.

set -euo pipefail

REPO=/repo
RESULT_DIR="$REPO/.e2e-vm-result"
GUEST_USER=ubuntu # cloud image's default user; also used as scripts/setup-e2e-env.sh's APP_SALMON_USER

# Always power off, however this script exits — otherwise a failure before the real test run
# (a bad apt mirror, rustup network hiccup, the 9p mount itself) leaves the VM sitting there
# and the host waiting out its full --timeout instead of failing fast with a log to read. If
# $RESULT_DIR isn't reachable yet (mount below hasn't succeeded), this best-effort mkdir/write
# just no-ops locally — the host will notice the missing exit_code file and point at
# console.log instead, which is the only record that exists at that point anyway.
on_exit() {
	code=$?
	mkdir -p "$RESULT_DIR" 2>/dev/null || true
	[ -f "$RESULT_DIR/exit_code" ] || echo "$code" >"$RESULT_DIR/exit_code" 2>/dev/null || true
	sync || true
	poweroff -f
}
trap on_exit EXIT

# cloud-init already tees runcmd output to the console (which the host captures to
# console.log), so if the mount below fails, that's where to look — nothing under $REPO is
# writable until it succeeds.
echo "== mount repo share =="
mkdir -p "$REPO"
mount -t 9p -o trans=virtio,version=9p2000.L,msize=524288 repo "$REPO"

mkdir -p "$RESULT_DIR"
exec > >(tee "$RESULT_DIR/guest-init.log") 2>&1

echo "== apt packages =="
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
	docker.io git build-essential pkg-config libssl-dev curl ca-certificates

echo "== docker =="
systemctl enable --now docker
usermod -aG docker "$GUEST_USER"

echo "== rust toolchain (as $GUEST_USER) =="
su - "$GUEST_USER" -c \
	'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'

echo "== e2e prerequisites (client accounts, sudoers rule, postgres image) =="
# The whole point of running this in a VM: setup-e2e-env.sh's useradd/sudoers.d writes land on
# this disposable guest, never on the real host running run-e2e-in-vm.sh.
APP_SALMON_USER="$GUEST_USER" "$REPO/scripts/setup-e2e-env.sh"

echo "== e2e suite (as $GUEST_USER) =="
# Calls cargo directly rather than `just test-e2e` — `just` isn't guaranteed to be in every
# Ubuntu release's default repos, and this is the one place in the whole tree where avoiding
# that dependency is worth the duplication (mirrors the justfile's test-e2e recipe exactly).
# CARGO_TARGET_DIR keeps build output off the 9p share, which is also the host's working tree.
set +e
su - "$GUEST_USER" -c \
	"source \$HOME/.cargo/env && cd $REPO && CARGO_TARGET_DIR=/tmp/target cargo test --test e2e -- --test-threads=1" \
	>"$RESULT_DIR/test-e2e.log" 2>&1
echo $? >"$RESULT_DIR/exit_code"
set -e

echo "== done =="
# on_exit (trap above) handles sync + poweroff.
