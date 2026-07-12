#!/usr/bin/env bash
# Idempotent setup for running App Salmon's e2e suite (`just test-e2e`) on this machine.
#
# Needs root (creates system users, writes /etc/sudoers.d). Run once per machine:
#   sudo ./scripts/setup-e2e-env.sh
#
# What it does:
#   1. Creates WORKER_COUNT worker accounts (salmon-worker-00, salmon-worker-01, ...) — no
#      login shell, no home directory contents of their own interest, just an identity for
#      Docker's --user and sudo -u to target.
#   2. Writes /etc/sudoers.d/app-salmon, scoped to exactly the two operations
#      PrivilegedExecutor issues (mkdir -p <path>, find <path> -mindepth 1 -delete) against
#      paths under WORKER_DATA_DIR_BASE, runnable by the invoking (app_salmon) user as any
#      salmon-worker-* account, without a password.
#   3. Pulls the Postgres+pgvector image the e2e suite spawns clusters from.
#
# Safe to re-run: every step checks current state before changing anything.

set -euo pipefail

WORKER_PREFIX="${APP_SALMON_WORKER_PREFIX:-salmon-worker-}"
WORKER_COUNT="${APP_SALMON_WORKER_COUNT:-4}"
WORKER_UID_BASE="${APP_SALMON_WORKER_UID_BASE:-27000}"
WORKER_DATA_DIR_BASE="${APP_SALMON_WORKER_DATA_DIR_BASE:-/var/lib/app_salmon/workers}"
APP_SALMON_USER="${APP_SALMON_USER:-app_salmon}"
POSTGRES_IMAGE="${APP_SALMON_POSTGRES_IMAGE:-pgvector/pgvector:pg16}"
SUDOERS_FILE="/etc/sudoers.d/app-salmon"

if [ "$(id -u)" -ne 0 ]; then
	echo "must be run as root (it creates system users and writes /etc/sudoers.d)" >&2
	exit 1
fi

echo "== worker accounts =="
for i in $(seq 0 $((WORKER_COUNT - 1))); do
	name=$(printf '%s%02d' "$WORKER_PREFIX" "$i")
	uid=$((WORKER_UID_BASE + i))
	if id "$name" >/dev/null 2>&1; then
		echo "  $name already exists, skipping"
	else
		echo "  creating $name (uid/gid $uid)"
		useradd --system --no-create-home --shell /usr/sbin/nologin --uid "$uid" --user-group "$name"
	fi

	worker_dir="$WORKER_DATA_DIR_BASE/$name"
	mkdir -p "$worker_dir"
	chown "$name:$name" "$worker_dir"
done

echo "== sudoers rule =="
tmp_sudoers=$(mktemp)
{
	echo "# Managed by scripts/setup-e2e-env.sh — do not edit by hand."
	echo "# Scoped to exactly the two operations PrivilegedExecutor issues."
	for i in $(seq 0 $((WORKER_COUNT - 1))); do
		name=$(printf '%s%02d' "$WORKER_PREFIX" "$i")
		echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/mkdir -p $WORKER_DATA_DIR_BASE/$name"
		echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/find $WORKER_DATA_DIR_BASE/$name -mindepth 1 -delete"
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

echo "done. $WORKER_COUNT worker accounts ready, sudoers rule installed, $POSTGRES_IMAGE pulled."
