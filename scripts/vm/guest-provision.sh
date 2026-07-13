#!/usr/bin/env bash
# Runs INSIDE the persistent e2e VM, as root (via `sudo bash /repo/scripts/vm/guest-provision.sh`
# over SSH), to make sure the guest has everything the e2e suite needs. Not meant to be run
# anywhere else.
#
# Deliberately idempotent and cheap to re-run: e2e-vm-up.sh runs this once after first boot, and
# e2e-vm-run-tests.sh runs it again before every test run. That second call is what makes a
# future phase's new e2e prerequisites (a new apt package, a new setup-e2e-env.sh step) "just
# work" the next time someone runs the e2e suite against an already-up VM, without any version
# tracking: every step below checks current state before doing anything, so a fully-provisioned
# guest re-running this pays only the cost of those checks (a few seconds), not a full reinstall.
#
# Status: written and reviewed (`bash -n` clean; shellcheck was not available in the sandbox this
# was written in), but not boot-verified — see scripts/vm/e2e-vm-up.sh's header and
# docs/DESIGN.md for why.

set -euo pipefail

GUEST_USER=ubuntu # cloud image's default user; also used as scripts/setup-e2e-env.sh's APP_SALMON_USER

echo "== apt packages =="
if dpkg -s docker.io git build-essential pkg-config libssl-dev curl ca-certificates \
	>/dev/null 2>&1; then
	echo "  already installed"
else
	export DEBIAN_FRONTEND=noninteractive
	apt-get update
	apt-get install -y --no-install-recommends \
		docker.io git build-essential pkg-config libssl-dev curl ca-certificates
fi

echo "== docker =="
systemctl enable --now docker
usermod -aG docker "$GUEST_USER"

echo "== rust toolchain (as $GUEST_USER) =="
if [ -x "/home/$GUEST_USER/.cargo/bin/cargo" ]; then
	echo "  already installed"
else
	su - "$GUEST_USER" -c \
		'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
fi

echo "== e2e prerequisites (client accounts, sudoers rule, postgres image) =="
# setup-e2e-env.sh is itself written to be safe to re-run (every step checks current state), so
# calling it on every provision pass rather than only on first boot is deliberate, not wasteful.
APP_SALMON_USER="$GUEST_USER" /repo/scripts/setup-e2e-env.sh

echo "== provisioning complete =="
