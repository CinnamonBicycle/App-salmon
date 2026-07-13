#!/usr/bin/env bash
# Prints whether the persistent e2e VM (scripts/vm/e2e-vm-up.sh) is up, and if so, whether it's
# provisioned and ready for scripts/vm/e2e-vm-run-tests.sh. Exit code: 0 if up, 2 if down — same
# convention as e2e-vm-run-tests.sh, so `just ci` can use this to decide which e2e path to run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
# shellcheck source=./lib.sh
source "$SCRIPT_DIR/lib.sh"

STATE_DIR="$(vm_state_dir)"

if [ -d "$STATE_DIR" ] && vm_persistent_is_up "$STATE_DIR"; then
	if vm_persistent_is_provisioned "$STATE_DIR"; then
		echo "up and provisioned on 127.0.0.1:$VM_PORT"
	else
		echo "up on 127.0.0.1:$VM_PORT, not yet provisioned"
	fi
	exit 0
else
	echo "down"
	exit 2
fi
