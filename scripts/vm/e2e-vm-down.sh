#!/usr/bin/env bash
# Tears down the persistent VM started by scripts/vm/e2e-vm-up.sh: shuts the guest down (SSH
# poweroff if reachable, SIGTERM/SIGKILL the qemu process otherwise), then wipes the state
# directory — overlay disk, SSH keys, logs, everything. The next `e2e-vm-up.sh` starts from a
# fresh base-image overlay and reprovisions from scratch. Idempotent: fine to run when nothing is
# up.
#
# Status: not boot-verified — see scripts/vm/e2e-vm-up.sh's header and docs/DESIGN.md §8b.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
# shellcheck source=./lib.sh
source "$SCRIPT_DIR/lib.sh"

STATE_DIR="$(vm_state_dir)"

if [ ! -d "$STATE_DIR" ]; then
	echo "no persistent e2e VM state found; nothing to do"
	exit 0
fi

if vm_persistent_is_up "$STATE_DIR"; then
	echo "== shutting down guest =="
	vm_ssh "$STATE_DIR" "$VM_PORT" "sudo poweroff" || true
	pid="$(cat "$STATE_DIR/qemu.pid" 2>/dev/null || true)"
	if [ -n "$pid" ]; then
		for _ in $(seq 1 30); do
			kill -0 "$pid" 2>/dev/null || break
			sleep 1
		done
	fi
fi

echo "== removing state (overlay disk, keys, logs) =="
vm_reap_stale "$STATE_DIR"
echo "== down =="
