#!/usr/bin/env bash
# One-time helper to get this host ready to run `just test-e2e-vm`.
#
# Unlike scripts/setup-e2e-env.sh (which provisions long-lived, App-Salmon-specific system
# accounts and a sudoers rule, needed again on every machine that runs the bare-host e2e path),
# this script does two ordinary, one-time things any KVM user needs on a fresh machine:
#   1. install qemu + a cloud-init seed-image tool, if not already present
#   2. add you to the `kvm` group, if you're not already in it
# Nothing here is App-Salmon-specific and nothing here persists once done — nothing to undo,
# nothing this repo's tests depend on beyond "qemu is on PATH and /dev/kvm is usable".
#
# Needs sudo only for the two steps above (invoked per-step, not for the whole script) — run
# this as your normal user, not as root. Safe to re-run: every step checks current state first.
#
# After this script adds you to the kvm group, log out and back in (or run `newgrp kvm` in your
# current shell) before running `just test-e2e-vm` — group membership changes don't apply to
# already-running sessions.
#
# Debian/Ubuntu-specific (uses apt-get), matching this repo's existing assumption elsewhere
# (scripts/setup-e2e-env.sh, the guest cloud image). On another distro, install
# qemu-system-x86_64/qemu-img and a cloud-init seed tool with your package manager, then add
# yourself to the group that owns /dev/kvm on that system (usually also `kvm`).

set -euo pipefail

echo "== qemu =="
if command -v qemu-system-x86_64 >/dev/null 2>&1 && command -v qemu-img >/dev/null 2>&1; then
	echo "  already installed"
else
	echo "  installing qemu-system-x86 qemu-utils"
	sudo apt-get update
	sudo apt-get install -y qemu-system-x86 qemu-utils
fi

echo "== cloud-init seed tool =="
SEED_TOOL=""
for candidate in cloud-localds genisoimage mkisofs xorriso; do
	if command -v "$candidate" >/dev/null 2>&1; then
		SEED_TOOL="$candidate"
		break
	fi
done
if [ -n "$SEED_TOOL" ]; then
	echo "  already have $SEED_TOOL"
else
	echo "  installing cloud-image-utils"
	sudo apt-get update
	sudo apt-get install -y cloud-image-utils
fi

echo "== kvm group membership =="
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
	echo "  already have working /dev/kvm access"
elif [ ! -e /dev/kvm ]; then
	echo "  /dev/kvm does not exist on this host." >&2
	echo "  This host's CPU/firmware/hypervisor doesn't have hardware virtualization exposed" >&2
	echo "  to it. If this host is itself a VM, its own hypervisor needs nested virtualization" >&2
	echo "  turned on for it; if it's bare metal, enable VT-x/AMD-V (\"Intel VT\" / \"SVM Mode\")" >&2
	echo "  in the firmware/BIOS setup. Nothing this script can fix from inside the OS." >&2
	exit 1
elif getent group kvm >/dev/null 2>&1 && getent group kvm | cut -d: -f4 | tr ',' '\n' | grep -qx "$(whoami)"; then
	echo "  you're already in the kvm group (per /etc/group), but this shell session predates"
	echo "  that — run 'newgrp kvm', or log out and back in, then re-run 'just test-e2e-vm'."
else
	echo "  adding $(whoami) to the kvm group (needs sudo, one-time)"
	sudo usermod -aG kvm "$(whoami)"
	echo "  done. Log out and back in (or run 'newgrp kvm' in this shell) for it to take"
	echo "  effect, then run 'just test-e2e-vm'."
fi

echo "== done =="
