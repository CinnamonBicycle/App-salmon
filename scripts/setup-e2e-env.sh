#!/usr/bin/env bash
# Idempotent setup for running App Salmon's e2e suite (`just test-e2e`) on this machine.
#
# Needs root (creates system users, writes /etc/sudoers.d). Run once per machine:
#   sudo ./scripts/setup-e2e-env.sh
#
# What it does:
#   1. Creates one Unix account per e2e test client (see APP_SALMON_E2E_CLIENTS below) — no
#      login shell, no home directory contents of their own interest, just an identity for
#      Docker's --user and sudo -u to target. Each client gets its own account (not a shared
#      pool): tests/e2e/common.rs uses the client name itself as the account name.
#   2. Writes /etc/sudoers.d/app-salmon, scoped to exactly the two operations
#      PrivilegedExecutor issues (mkdir -p <path>, find <path> -mindepth 1 -delete) against
#      each client's own literal, pre-enumerated directory-slot paths (one per
#      MAX_CLUSTERS_PER_USER, matching config.toml's limits.max_clusters_per_user), runnable by
#      the invoking (app_salmon) user as that client's account, without a password. Deliberately
#      NOT a wildcard: this environment's `sudo` implementation (`sudo-rs`) rejects wildcards
#      embedded in command arguments outright at `visudo -c` time ("wildcards are not allowed in
#      command arguments"), and even where a `sudo` implementation does accept them, a wildcard
#      here would grant more than intended. A cluster's directory is `<client>/slot-N`, where N is
#      assigned atomically at creation time (see `ClusterRepository::try_insert_if_under_quota`,
#      `docs/DESIGN.md` §6) — bounded and known ahead of time, so every path can be listed
#      literally.
#   3. Pulls the Postgres+pgvector image the e2e suite spawns clusters from.
#
# Safe to re-run: every step checks current state before changing anything.

set -euo pipefail

# Space-separated list of e2e client names — must match tests/e2e/common.rs's CLIENT_NAME /
# OTHER_CLIENT_NAME. Each name becomes both the client id and its own Unix account name.
APP_SALMON_E2E_CLIENTS="${APP_SALMON_E2E_CLIENTS:-e2e-agent e2e-agent-other}"
WORKER_UID_BASE="${APP_SALMON_WORKER_UID_BASE:-27000}"
WORKER_DATA_DIR_BASE="${APP_SALMON_WORKER_DATA_DIR_BASE:-/var/lib/app_salmon/workers}"
APP_SALMON_USER="${APP_SALMON_USER:-app_salmon}"
POSTGRES_IMAGE="${APP_SALMON_POSTGRES_IMAGE:-pgvector/pgvector:pg16}"
# Must match the e2e test server's configured limits.max_clusters_per_user (see
# tests/e2e/common.rs), which bounds how many directory slots each client's account needs.
MAX_CLUSTERS_PER_USER="${APP_SALMON_MAX_CLUSTERS_PER_USER:-2}"
SUDOERS_FILE="/etc/sudoers.d/app-salmon"

if [ "$(id -u)" -ne 0 ]; then
	echo "must be run as root (it creates system users and writes /etc/sudoers.d)" >&2
	exit 1
fi

echo "== client accounts =="
uid=$WORKER_UID_BASE
for name in $APP_SALMON_E2E_CLIENTS; do
	if id "$name" >/dev/null 2>&1; then
		echo "  $name already exists, skipping"
	else
		echo "  creating $name (uid/gid $uid)"
		useradd --system --no-create-home --shell /usr/sbin/nologin --uid "$uid" --user-group "$name"
	fi

	for slot in $(seq 0 $((MAX_CLUSTERS_PER_USER - 1))); do
		slot_dir="$WORKER_DATA_DIR_BASE/$name/slot-$slot"
		mkdir -p "$slot_dir"
		chown "$name:$name" "$slot_dir"
	done
	uid=$((uid + 1))
done

echo "== sudoers rule =="
tmp_sudoers=$(mktemp)
{
	echo "# Managed by scripts/setup-e2e-env.sh — do not edit by hand."
	echo "# Scoped to exactly the two operations PrivilegedExecutor issues, against exactly"
	echo "# MAX_CLUSTERS_PER_USER literal directory-slot paths per client account — no wildcard"
	echo "# (see the script's header comment for why)."
	for name in $APP_SALMON_E2E_CLIENTS; do
		for slot in $(seq 0 $((MAX_CLUSTERS_PER_USER - 1))); do
			slot_dir="$WORKER_DATA_DIR_BASE/$name/slot-$slot"
			echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/mkdir -p $slot_dir"
			echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/find $slot_dir -mindepth 1 -delete"
		done
	done
} >"$tmp_sudoers"
visudo -c -f "$tmp_sudoers" >/dev/null
install -m 0440 -o root -g root "$tmp_sudoers" "$SUDOERS_FILE"
rm -f "$tmp_sudoers"
echo "  wrote $SUDOERS_FILE"

echo "== postgres image =="
if command -v docker >/dev/null 2>&1; then
	docker pull "$POSTGRES_IMAGE"
else
	echo "  docker not found on PATH; install it and re-run, or pull $POSTGRES_IMAGE manually" >&2
	exit 1
fi

echo "done. client accounts ready ($APP_SALMON_E2E_CLIENTS), sudoers rule installed, $POSTGRES_IMAGE pulled."
