#!/usr/bin/env bash
# Idempotent setup for running App Salmon's e2e suite on a machine. Not meant to be run directly
# on your own machine — `scripts/vm/guest-provision.sh` runs this automatically, as root, inside
# the disposable e2e VM (see `just e2e-vm-up`/`e2e-vm-test`, docs/DESIGN.md §8c). There used to be
# a bare-host path that ran this directly (`just setup-e2e`); it was removed (§8d) once the VM
# path covered the same need without the persistent host-level changes below.
#
# Needs root (creates system users, writes /etc/sudoers.d).
#
# What it does:
#   1. Creates one Unix account per e2e test client (see APP_SALMON_E2E_CLIENTS below) — no
#      login shell, no home directory contents of their own interest, just an identity for
#      Docker's --user and sudo -u to target. Each client gets its own account (not a shared
#      pool): tests/e2e/common.rs uses the client name itself as the account name.
#   2. Creates (and chowns to APP_SALMON_USER) the two base directories app_salmon's own process
#      writes into directly, unprivileged, for a Supabase spawn's generated files: the tar-staging
#      area an uploaded project_tar is validated/extracted into before its privileged copy (see
#      service::spawn_task::adopt_project_tar), and the generated-config area SupabaseBackend
#      writes each cluster's kong.yml/roles.sql/jwt.sql into. Unlike WORKER_DATA_DIR_BASE (only
#      ever written by privileged, per-client commands), these two are written directly by
#      app_salmon itself, so they need to be app_salmon-owned from the start, not left for
#      `mkdir -p` to create root-owned as a side effect of the first privileged call that touches
#      a path under them.
#   3. Writes /etc/sudoers.d/app-salmon, scoped to exactly the three operations
#      PrivilegedExecutor issues (mkdir -p <path>, find <path> -mindepth 1 -delete, and
#      cp -r <staging>/. <dest> — see PrivilegedCommand::AdoptStagedTree) against each client's
#      own literal, pre-enumerated directory-slot paths (one per MAX_CLUSTERS_PER_USER, matching
#      config.toml's limits.max_clusters_per_user), runnable by the invoking (app_salmon) user as
#      that client's account, without a password. Deliberately NOT a wildcard: this environment's
#      `sudo` implementation (`sudo-rs`) rejects wildcards embedded in command arguments outright
#      at `visudo -c` time ("wildcards are not allowed in command arguments"), and even where a
#      `sudo` implementation does accept them, a wildcard here would grant more than intended. A
#      cluster's directory is `<client>/slot-N`, where N is assigned atomically at creation time
#      (see `ClusterRepository::try_insert_if_under_quota`, `docs/DESIGN.md` §6) — bounded and
#      known ahead of time, so every path can be listed literally. This includes the mkdir rule
#      for `<client>/slot-N/project/functions` (SupabaseBackend::worker_subdirs()) alongside the
#      bare `<client>/slot-N` one (ServiceKind::Postgres, and Supabase's own PrepareWorkerDir call
#      against the slot dir itself) — the same slot can host either kind across its lifetime, so
#      both target paths need coverage.
#   4. Pulls the Postgres+pgvector image the plain (non-Supabase) e2e clusters spawn from, plus
#      the five images a Supabase cluster's stack spawns from (db/rest/auth/kong/functions) — see
#      tests/e2e/common.rs's SUPABASE_*_IMAGE constants, which these defaults must match exactly
#      (a cross-artifact invariant, like the ones docs/DESIGN.md §8a documents elsewhere).
#
# Safe to re-run: every step checks current state before changing anything.

set -euo pipefail

# Space-separated list of e2e client names — must match tests/e2e/common.rs's CLIENT_NAME /
# OTHER_CLIENT_NAME. Each name becomes both the client id and its own Unix account name.
APP_SALMON_E2E_CLIENTS="${APP_SALMON_E2E_CLIENTS:-e2e-agent e2e-agent-other}"
WORKER_UID_BASE="${APP_SALMON_WORKER_UID_BASE:-27000}"
WORKER_DATA_DIR_BASE="${APP_SALMON_WORKER_DATA_DIR_BASE:-/var/lib/app_salmon/workers}"
# Must match tests/e2e/common.rs's TAR_STAGING_DIR_BASE / GENERATED_CONFIG_DIR_BASE constants.
TAR_STAGING_DIR_BASE="${APP_SALMON_TAR_STAGING_DIR_BASE:-/var/lib/app_salmon/tar-staging}"
GENERATED_CONFIG_DIR_BASE="${APP_SALMON_GENERATED_CONFIG_DIR_BASE:-/var/lib/app_salmon/generated-config}"
APP_SALMON_USER="${APP_SALMON_USER:-app_salmon}"
POSTGRES_IMAGE="${APP_SALMON_POSTGRES_IMAGE:-pgvector/pgvector:pg16}"
# Must match tests/e2e/common.rs's SUPABASE_*_IMAGE constants.
SUPABASE_POSTGRES_IMAGE="${APP_SALMON_SUPABASE_POSTGRES_IMAGE:-supabase/postgres:17.6.1.136}"
SUPABASE_POSTGREST_IMAGE="${APP_SALMON_SUPABASE_POSTGREST_IMAGE:-postgrest/postgrest:v14.12}"
SUPABASE_GOTRUE_IMAGE="${APP_SALMON_SUPABASE_GOTRUE_IMAGE:-supabase/gotrue:v2.189.0}"
SUPABASE_KONG_IMAGE="${APP_SALMON_SUPABASE_KONG_IMAGE:-kong/kong:3.9.1}"
SUPABASE_EDGE_RUNTIME_IMAGE="${APP_SALMON_SUPABASE_EDGE_RUNTIME_IMAGE:-supabase/edge-runtime:v1.74.0}"
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

echo "== app_salmon-owned base directories =="
for dir in "$TAR_STAGING_DIR_BASE" "$GENERATED_CONFIG_DIR_BASE"; do
	mkdir -p "$dir"
	chown "$APP_SALMON_USER:$APP_SALMON_USER" "$dir"
done

echo "== sudoers rule =="
tmp_sudoers=$(mktemp)
{
	echo "# Managed by scripts/setup-e2e-env.sh — do not edit by hand."
	echo "# Scoped to exactly the three operations PrivilegedExecutor issues, against exactly"
	echo "# MAX_CLUSTERS_PER_USER literal directory-slot paths per client account — no wildcard"
	echo "# (see the script's header comment for why)."
	for name in $APP_SALMON_E2E_CLIENTS; do
		for slot in $(seq 0 $((MAX_CLUSTERS_PER_USER - 1))); do
			slot_dir="$WORKER_DATA_DIR_BASE/$name/slot-$slot"
			staging_dir="$TAR_STAGING_DIR_BASE/$name/slot-$slot"
			echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/mkdir -p $slot_dir"
			echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/mkdir -p $slot_dir/project/functions"
			echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/find $slot_dir -mindepth 1 -delete"
			echo "$APP_SALMON_USER ALL=($name) NOPASSWD: /usr/bin/cp -r $staging_dir/. $slot_dir/project"
		done
	done
} >"$tmp_sudoers"
visudo -c -f "$tmp_sudoers" >/dev/null
install -m 0440 -o root -g root "$tmp_sudoers" "$SUDOERS_FILE"
rm -f "$tmp_sudoers"
echo "  wrote $SUDOERS_FILE"

echo "== images =="
if ! command -v docker >/dev/null 2>&1; then
	echo "  docker not found on PATH; install it and re-run, or pull the images below manually" >&2
	exit 1
fi
for image in "$POSTGRES_IMAGE" "$SUPABASE_POSTGRES_IMAGE" "$SUPABASE_POSTGREST_IMAGE" \
	"$SUPABASE_GOTRUE_IMAGE" "$SUPABASE_KONG_IMAGE" "$SUPABASE_EDGE_RUNTIME_IMAGE"; do
	docker pull "$image"
done

echo "done. client accounts ready ($APP_SALMON_E2E_CLIENTS), sudoers rule installed, images pulled."
