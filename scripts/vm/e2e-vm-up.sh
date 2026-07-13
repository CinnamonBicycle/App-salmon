#!/usr/bin/env bash
# Boots a persistent, disposable QEMU VM for running the e2e suite against repeatedly within a
# session, without paying full boot+provision cost on every run. Idempotent: if a healthy
# instance is already up, does nothing and reports it. This is the only VM-based e2e path — an
# earlier one-shot boot/test/discard variant (run-e2e-in-vm.sh) was removed after it turned out
# to have the same 9p-permission bug this script's own history fixed (see below), and fixing it
# there too would have meant re-deriving a synchronous SSH channel it was never designed to have.
#
# Host requirements and the *only* place sudo is needed: scripts/vm/setup-vm-host.sh
# (`just setup-e2e-vm`).
#
# Usage: scripts/vm/e2e-vm-up.sh
# Then:  scripts/vm/e2e-vm-run-tests.sh   (repeatedly, as often as you like)
#        scripts/vm/e2e-vm-down.sh        (when done — wipes the VM's disk)
#
# Status: boot-verified end to end for real against a real KVM host, across two runs
# (2026-07-13). Run 1 confirmed boot, cloud-init's write_files/ssh_authorized_keys/host-key
# injection, and SSH all working, and also found a real bug: a live 9p share
# (security_model=none) passes the host's raw uid/gid/mode through to the guest, so a non-root
# guest user without a matching uid gets locked out of a normal-permission host checkout. Fixed
# by dropping the 9p share entirely in favor of copying the repo in over the same SSH channel
# (see lib.sh's vm_sync_repo). Run 2, after that fix: full up → sync → provision → e2e cycle
# completed cleanly, all 18 e2e tests passing. See docs/DESIGN.md §8c.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
# shellcheck source=./lib.sh
source "$SCRIPT_DIR/lib.sh"

STATE_DIR="$(vm_state_dir)"

# Shared by both the "already up, never finished provisioning" branch below and the fresh-boot
# tail at the end of this script, so a provisioning failure is always actually retried by
# re-running this script, not silently no-op'd.
provision_and_mark() {
	local port="$1"
	echo "== syncing repo =="
	vm_sync_repo "$STATE_DIR" "$port"
	echo "== provisioning (docker, rust, e2e accounts) =="
	if vm_ssh "$STATE_DIR" "$port" "sudo bash /repo/scripts/vm/guest-provision.sh"; then
		touch "$STATE_DIR/provisioned"
		echo "== up and ready: 127.0.0.1:$port =="
	else
		echo "VM is up but provisioning failed — see output above." >&2
		echo "  the VM is still running for inspection; 'scripts/vm/e2e-vm-down.sh' to tear it" >&2
		echo "  down, or fix the issue and re-run this script (it will retry provisioning)." >&2
		exit 1
	fi
}

if [ -d "$STATE_DIR" ] && vm_persistent_is_up "$STATE_DIR"; then
	if vm_persistent_is_provisioned "$STATE_DIR"; then
		echo "already up and provisioned on 127.0.0.1:$VM_PORT"
		exit 0
	fi
	echo "already up on 127.0.0.1:$VM_PORT but not yet provisioned — retrying provisioning"
	provision_and_mark "$VM_PORT"
	exit 0
fi

if [ -d "$STATE_DIR" ]; then
	echo "== stale state dir found, cleaning up =="
	vm_reap_stale "$STATE_DIR"
fi

echo "== checking host prerequisites =="
vm_check_host_prereqs
echo "  ok (seed tool: $SEED_TOOL)"

echo "== base image =="
vm_ensure_base_image

mkdir -m 0700 -p "$STATE_DIR"

echo "== ssh keys =="
ssh-keygen -q -t ed25519 -N '' -C app-salmon-e2e-vm-host -f "$STATE_DIR/host_key"
ssh-keygen -q -t ed25519 -N '' -C app-salmon-e2e-vm-client -f "$STATE_DIR/client_key"
chmod 600 "$STATE_DIR/host_key" "$STATE_DIR/client_key"

echo "== port =="
PORT="$(vm_find_free_port)"
echo "$PORT" >"$STATE_DIR/port"
echo "  using 127.0.0.1:$PORT"
printf '[127.0.0.1]:%s %s\n' "$PORT" "$(cut -d' ' -f1-2 "$STATE_DIR/host_key.pub")" \
	>"$STATE_DIR/known_hosts"

echo "== overlay disk (base image itself is never modified) =="
qemu-img create -f qcow2 -F qcow2 -b "$VM_BASE_IMAGE" "$STATE_DIR/overlay.qcow2" 20G >/dev/null

echo "== cloud-init seed =="
HOST_KEY_B64="$(base64 -w0 "$STATE_DIR/host_key")"
HOST_KEY_PUB_B64="$(base64 -w0 "$STATE_DIR/host_key.pub")"
CLIENT_PUBKEY="$(cat "$STATE_DIR/client_key.pub")"
cat >"$STATE_DIR/user-data" <<EOF
#cloud-config
hostname: app-salmon-e2e-persistent
ssh_pwauth: false
ssh_genkeytypes: []
ssh_deletekeys: false
ssh_authorized_keys:
  - ${CLIENT_PUBKEY}
write_files:
  - path: /etc/ssh/ssh_host_ed25519_key
    encoding: b64
    permissions: '0600'
    owner: root:root
    content: ${HOST_KEY_B64}
  - path: /etc/ssh/ssh_host_ed25519_key.pub
    encoding: b64
    permissions: '0644'
    owner: root:root
    content: ${HOST_KEY_PUB_B64}
runcmd:
  - [systemctl, restart, ssh]
EOF
cat >"$STATE_DIR/meta-data" <<EOF
instance-id: app-salmon-e2e-persistent-$(date +%s)
local-hostname: app-salmon-e2e-persistent
EOF
vm_build_seed_iso "$STATE_DIR/seed.iso" "$STATE_DIR/user-data" "$STATE_DIR/meta-data"

echo "== booting VM (daemonized; console log: $STATE_DIR/console.log) =="
qemu-system-x86_64 \
	-machine q35,accel=kvm \
	-cpu host \
	-smp 4 \
	-m 4096 \
	-no-reboot \
	-display none \
	-serial "file:$STATE_DIR/console.log" \
	-monitor none \
	-drive file="$STATE_DIR/overlay.qcow2",if=virtio,format=qcow2 \
	-drive file="$STATE_DIR/seed.iso",if=virtio,format=raw,media=cdrom,readonly=on \
	-netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${PORT}-:22" -device virtio-net-pci,netdev=net0 \
	-name "guest=app-salmon-e2e-persistent" \
	-pidfile "$STATE_DIR/qemu.pid" \
	-daemonize

echo "== waiting for SSH (up to 180s) =="
ready=0
for _ in $(seq 1 60); do
	if vm_ssh "$STATE_DIR" "$PORT" true >/dev/null 2>&1; then
		ready=1
		break
	fi
	sleep 3
done
if [ "$ready" -ne 1 ]; then
	echo "VM booted but SSH never became reachable within 180s." >&2
	echo "  console log: $STATE_DIR/console.log" >&2
	echo "  the VM is still running — 'scripts/vm/e2e-vm-down.sh' to tear it down, or inspect" >&2
	echo "  it manually first." >&2
	exit 1
fi
echo "  ssh reachable on 127.0.0.1:$PORT"

provision_and_mark "$PORT"
