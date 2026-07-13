#!/usr/bin/env bash
# Runs the e2e suite against the persistent VM started by scripts/vm/e2e-vm-up.sh
# (`just e2e-vm-up`). Re-syncs the repo (see lib.sh's vm_sync_repo — the guest has its own copy,
# not a live mount) and re-runs guest-provision.sh first (idempotent — see its header for why:
# this is what makes a future phase's new e2e prerequisites "just work" without any version
# tracking) and then `cargo test --test e2e`, both over SSH, streaming output. Both of those run
# on every call, not just once at `e2e-vm-up`, so this always tests current code against current
# prerequisites.
#
# Exit codes: 2 if the persistent VM isn't up (distinct from a test failure, so callers — see
# `just ci` in the justfile — can tell "nothing to run against" from "ran and failed"); otherwise
# the e2e suite's own exit code.
#
# Status: not boot-verified since the switch from a live 9p share to copying the repo in — see
# scripts/vm/e2e-vm-up.sh's header and docs/DESIGN.md §8c.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
# shellcheck source=./lib.sh
source "$SCRIPT_DIR/lib.sh"

STATE_DIR="$(vm_state_dir)"

if [ ! -d "$STATE_DIR" ] || ! vm_persistent_is_up "$STATE_DIR"; then
	echo "no persistent e2e VM is up." >&2
	echo "  run: just e2e-vm-up" >&2
	exit 2
fi

echo "== syncing repo =="
vm_sync_repo "$STATE_DIR" "$VM_PORT"

echo "== provisioning (idempotent — picks up any new prerequisites) =="
vm_ssh "$STATE_DIR" "$VM_PORT" "sudo bash /repo/scripts/vm/guest-provision.sh"
touch "$STATE_DIR/provisioned"

echo "== e2e suite =="
vm_ssh "$STATE_DIR" "$VM_PORT" \
	"source \$HOME/.cargo/env && cd /repo && CARGO_TARGET_DIR=/tmp/target cargo test --test e2e -- --test-threads=1"
