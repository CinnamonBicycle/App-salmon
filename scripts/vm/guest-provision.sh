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

echo "== e2e prerequisites (client accounts, sudoers rule, images) =="
# setup-e2e-env.sh is itself written to be safe to re-run (every step checks current state), so
# calling it on every provision pass rather than only on first boot is deliberate, not wasteful.
APP_SALMON_USER="$GUEST_USER" /repo/scripts/setup-e2e-env.sh

echo "== kata containers =="
# Pinned, not "latest": empirically verified working at this exact version (2026-07-13, real KVM
# host) after finding two real bugs in the "standard" install path — see docs/DESIGN.md for the
# full story. Bump deliberately, re-verifying, not by drifting.
KATA_VERSION="3.32.0"
if [ -x /opt/kata/bin/kata-runtime ] && docker info 2>/dev/null | grep -q ' kata'; then
	echo "  already installed and registered"
else
	# kata-manager.sh (the project's own documented installer) is broken against this release:
	# it verifies /opt/kata/bin exists post-extraction, but 3.32.0's tarball only ships the (now
	# Rust-only) shim under runtime-rs/bin/, not bin/ — the installer's own check predates that
	# layout change and fails every time, even with an explicit older-version pin (tried 3.30.0
	# too). Confirmed by extracting the exact same release tarball by hand: the real content
	# (including /opt/kata/bin/kata-runtime) is all there and correct — only the installer
	# script's completion check is stale. Downloading and extracting the official static release
	# tarball directly sidesteps the broken check entirely.
	if [ ! -d /opt/kata ]; then
		echo "  downloading kata-static-${KATA_VERSION}"
		curl -fsSL -o /tmp/kata-static.tar.zst \
			"https://github.com/kata-containers/kata-containers/releases/download/${KATA_VERSION}/kata-static-${KATA_VERSION}-amd64.tar.zst"
		tar -I zstd -xf /tmp/kata-static.tar.zst -C /
		rm -f /tmp/kata-static.tar.zst
	fi
	ln -sf /opt/kata/runtime-rs/bin/containerd-shim-kata-v2 /usr/bin/containerd-shim-kata-v2
	ln -sf /opt/kata/bin/kata-runtime /usr/bin/kata-runtime

	# Docker's own runtime-name validation rejects any name not in its "runtimes" map (unlike
	# containerd, which will resolve any name to a containerd-shim-<name>-v2 binary on PATH) —
	# this map entry is what makes `docker run --runtime kata` accepted at all. The key is
	# "runtimeType", not "path": "path" is Docker's *legacy* mechanism for a raw runc-CLI-style
	# binary (create/start/--bundle/--root args) and produces a real but misleading runtime
	# error ("flag provided but not defined: -root") when pointed at a shim-v2 binary, which
	# speaks a completely different (GRPC task-management) protocol. "runtimeType" tells Docker
	# to hand the name straight to containerd's native runtime-v2 shim resolution instead, which
	# is what containerd-shim-kata-v2 actually implements. Confirmed both ways empirically —
	# "path" reproduces the "-root" failure every time, "runtimeType" works.
	python3 - <<'PYEOF'
import json
path = "/etc/docker/daemon.json"
try:
    with open(path) as f:
        config = json.load(f)
except FileNotFoundError:
    config = {}
config.setdefault("runtimes", {})["kata"] = {"runtimeType": "io.containerd.kata.v2"}
with open(path, "w") as f:
    json.dump(config, f, indent=2)
PYEOF
	systemctl restart docker
fi

echo "== provisioning complete =="
