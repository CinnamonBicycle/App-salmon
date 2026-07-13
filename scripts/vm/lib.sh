#!/usr/bin/env bash
# Shared helpers for scripts/vm/*.sh. Sourced, not executed — has no shebang-execute purpose of
# its own. Every function assumes `set -euo pipefail` is active in the caller.

# Fails with a pointer to setup-vm-host.sh if qemu-system-x86_64, qemu-img, /dev/kvm access, or a
# cloud-init seed-image tool aren't available. On success, sets SEED_TOOL in the caller's scope.
vm_check_host_prereqs() {
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
}

# Downloads (if not already cached) and checksum-verifies the base Ubuntu cloud image against the
# vendor's published SHA256SUMS — re-checked every call, not just the first, so a stale local
# cache is caught even though the "current" image at that URL moves over time. Sets
# VM_BASE_IMAGE to the verified local path in the caller's scope.
vm_ensure_base_image() {
	local cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/app-salmon-e2e-vm"
	local image_name="noble-server-cloudimg-amd64.img"
	local image_url="https://cloud-images.ubuntu.com/noble/current/${image_name}"
	local sha_url="https://cloud-images.ubuntu.com/noble/current/SHA256SUMS"

	mkdir -p "$cache_dir"

	if [ ! -f "$cache_dir/$image_name" ]; then
		echo "  downloading $image_url"
		curl -fL --progress-bar -o "$cache_dir/$image_name.part" "$image_url"
		mv "$cache_dir/$image_name.part" "$cache_dir/$image_name"
	fi
	echo "  verifying checksum against $sha_url"
	local expected actual
	expected="$(curl -fsL "$sha_url" | grep " \*${image_name}\$" | cut -d' ' -f1)"
	[ -n "$expected" ] || {
		echo "could not find $image_name in SHA256SUMS" >&2
		exit 1
	}
	actual="$(sha256sum "$cache_dir/$image_name" | cut -d' ' -f1)"
	if [ "$expected" != "$actual" ]; then
		echo "  cached image checksum mismatch (expected $expected, got $actual); redownloading" >&2
		rm -f "$cache_dir/$image_name"
		curl -fL --progress-bar -o "$cache_dir/$image_name" "$image_url"
		actual="$(sha256sum "$cache_dir/$image_name" | cut -d' ' -f1)"
		[ "$expected" = "$actual" ] || {
			echo "checksum still mismatched after redownload; aborting" >&2
			exit 1
		}
	fi
	echo "  ok ($actual)"
	VM_BASE_IMAGE="$cache_dir/$image_name"
}

# Builds a cloud-init NoCloud seed ISO at $1 from user-data $2 and meta-data $3, using whichever
# tool vm_check_host_prereqs found (SEED_TOOL, set by that function).
vm_build_seed_iso() {
	local seed_iso="$1" user_data="$2" meta_data="$3"
	case "$SEED_TOOL" in
	cloud-localds)
		cloud-localds "$seed_iso" "$user_data" "$meta_data"
		;;
	*)
		# genisoimage, mkisofs, and `xorriso -as genisoimage` all accept this same argument shape.
		local iso_cmd=("$SEED_TOOL")
		[ "$SEED_TOOL" = "xorriso" ] && iso_cmd+=(-as genisoimage)
		"${iso_cmd[@]}" -output "$seed_iso" -volid cidata -joliet -rock "$user_data" "$meta_data" >/dev/null
		;;
	esac
}

# Finds a free TCP port on 127.0.0.1. Inherently a small TOCTOU race (the port is closed again
# immediately, something else could grab it before qemu binds it) — acceptable for a local,
# single-user dev tool; not used for anything where that race matters.
vm_find_free_port() {
	python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()"
}

# True if $1 is the PID of a qemu-system-x86_64 process whose command line contains marker $2
# (we pass the state directory's overlay disk path) — guards against a stale state file whose PID
# has been reused by an unrelated process, which a plain `kill -0` can't distinguish.
vm_pid_is_our_qemu() {
	local pid="$1" marker="$2"
	[ -r "/proc/$pid/cmdline" ] || return 1
	local cmdline
	cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
	case "$cmdline" in
	*qemu-system-x86_64*"$marker"*) return 0 ;;
	*) return 1 ;;
	esac
}

# Path to the persistent VM's state directory for this checkout (one VM per checkout, per
# docs/DESIGN.md §8b): overlay disk, seed ISO, host/client SSH keypairs, pinned known_hosts,
# console log, qemu's pidfile, and a `provisioned` marker. Gitignored; 0700. Scoping it under the
# checkout itself (rather than e.g. a hash under /tmp) is what makes "one VM per checkout" true
# without extra bookkeeping — two clones naturally get two state dirs.
vm_state_dir() {
	echo "$REPO_ROOT/.e2e-vm-state"
}

# Checks whether the persistent VM for state dir $1 is up and reachable: the pidfile names a live
# process that is actually our qemu (see vm_pid_is_our_qemu — not just *a* live process at that
# PID), and a host-key-pinned SSH probe succeeds. On success, sets VM_PORT and returns 0; on any
# failure, returns 1 and callers should treat the instance as down (possibly stale — see
# vm_reap_stale). Deliberately does not check the `provisioned` marker — see
# vm_persistent_is_provisioned for that, kept separate since "up" and "ready for tests" are
# different questions (a fresh boot is up but not yet provisioned).
vm_persistent_is_up() {
	local state_dir="$1"
	[ -f "$state_dir/qemu.pid" ] && [ -f "$state_dir/port" ] || return 1
	local pid port
	pid="$(cat "$state_dir/qemu.pid")"
	port="$(cat "$state_dir/port")"
	vm_pid_is_our_qemu "$pid" "$state_dir/overlay.qcow2" || return 1
	vm_ssh "$state_dir" "$port" true >/dev/null 2>&1 || return 1
	VM_PORT="$port"
	return 0
}

# True if the (already confirmed up, via vm_persistent_is_up) VM at state dir $1 has finished
# guest-provision.sh at least once.
vm_persistent_is_provisioned() {
	[ -f "$1/provisioned" ]
}

# Kills whatever's left of a stale persistent-VM instance (dead-but-present pidfile, or a pidfile
# pointing at a PID that isn't actually our qemu) and removes the state dir, so callers can boot
# a fresh instance in its place. Safe to call on an already-clean state dir.
vm_reap_stale() {
	local state_dir="$1"
	if [ -f "$state_dir/qemu.pid" ]; then
		local pid
		pid="$(cat "$state_dir/qemu.pid" 2>/dev/null || true)"
		if [ -n "$pid" ] && vm_pid_is_our_qemu "$pid" "$state_dir/overlay.qcow2"; then
			echo "  reaping stale VM process (pid $pid)"
			kill "$pid" 2>/dev/null || true
			for _ in $(seq 1 20); do
				kill -0 "$pid" 2>/dev/null || break
				sleep 0.5
			done
			kill -9 "$pid" 2>/dev/null || true
		fi
	fi
	rm -rf "$state_dir"
}

# ssh/scp invoked against a specific persistent-VM instance's pinned host key and client key.
# Usage: vm_ssh "$STATE_DIR" "$PORT" [ssh args...]
# Every call site is non-interactive (health probes, provisioning, running tests, poweroff) —
# BatchMode=yes ensures a not-yet-ready or mismatched auth state fails fast instead of ssh
# falling back to an interactive prompt (which would hang the wait loop / just ci with no
# terminal to answer it).
vm_ssh() {
	local state_dir="$1" port="$2"
	shift 2
	ssh \
		-o BatchMode=yes \
		-o StrictHostKeyChecking=yes \
		-o UserKnownHostsFile="$state_dir/known_hosts" \
		-o HostKeyAlgorithms=ssh-ed25519 \
		-o IdentitiesOnly=yes \
		-o ConnectTimeout=5 \
		-i "$state_dir/client_key" \
		-p "$port" \
		ubuntu@127.0.0.1 \
		"$@"
}
