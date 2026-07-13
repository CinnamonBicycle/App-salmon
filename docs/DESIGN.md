# App Salmon — Design & Roadmap

## 1. Overview

App Salmon spins up short-lived Postgres/Supabase instances plus an OpenRouter proxy so LLM
coding agents — running as unprivileged, non-Docker-capable Unix "client accounts" — can run
integration tests against real-shaped databases and LLM calls, without ever holding real
OpenRouter credentials or Docker access. `app_salmon` runs as its own privileged service account
and centralizes the one dangerous capability (Docker, sudo) in a single audited service instead
of handing it to every client account.

This document is the durable record of *why* the system looks the way it does: the original
design brief, the phase-1 scope actually built, the architecture, the security model, and — most
importantly for whoever picks this up next — everything deliberately deferred and why.

## 2. Original design writeup (verbatim)

> My current thought is a server running on localhost. The server will not be accessible from
> other machines. There is a conversation in the RainQueue project discussing some design
> considerations.
>
> The server will be a simple REST server running as the user `spawner_service`. `spawner_service`
> will be able to sudo to the users for each of the LLMs that can call it. I'll call the user
> running the llm a client account. The plan is that the llms do not run as a human user. The
> human logs in in the LLM account to run the LLM harness.
>
> Each client account will have a copy of a public key for `spawner_service` and will connect over
> TLS. Each client account will have a secret that it shares with `spawner_service` so it can
> authenticate.
>
> We will use Python 3.14 for this service.
>
> The service will have four endpoints and accessing the root will pull up documentation just like
> a normal swagger/OpenAPI system. It does not need to use OpenAPI, but that is certainly an
> option. I do want a machine readable specification for how to call the service. MCP would seem
> to be ideal, except its system is designed around OAuth delegated authority and I don't see how
> the authentication could be handled appropriately.
>
> The first endpoint creates a cluster with a particular time to live and services provided. At
> the beginning, I think we only need to provide the supabase and a proxy for OpenRouter (to hide
> the secret key). When the endpoint returns, it lists the approximate time at which the services
> will be available. There is a new public key, set of Supabase passwords, and OpenRouter proxy
> bearer token generated for each cluster. The caller passes a tar file containing the supabase
> directory to use when configuring the supabase endpoint. The server needs to consider this tar
> file untrusted and validate it before using it, guarding against escape attempts. When the
> server receives the request, it starts a background process to spawn the server and change
> permissions appropriately. Supabase itself is trusted code so we can run it in Docker, however,
> the edge functions will need to run in a lightweight virtual machine for extra escape
> resistance. If you can't use both Docker and a lightweight VM, use the VM for everything. Note
> that you won't be able to just use Supabase's built-in docker-compose because it doesn't
> separate out the edge functions and doesn't change users. The main purpose of this system is to
> eliminate the need to give LLM users the ability to run Docker — and if the LLM is providing
> arbitrary code in edge functions that runs under a user that can run Docker, that's the same as
> giving the LLM user access to Docker.
>
> The create-cluster endpoint also includes a requested time-to-live parameter, bounded both above
> and below (starting at 30 seconds minimum, 1 hour maximum — to be tuned with experience). The TTL
> clock does not include startup time; it starts once the info endpoint reports the cluster ready.
>
> A limited number of clusters can exist per user — starting at 2.
>
> The second endpoint is a cluster info endpoint: besides the auth token, it takes the cluster ID.
> If the background spawn hasn't finished, it says so. Once finished, it gives URLs, the public
> key for TLS, Supabase passwords, requested/started/scheduled-decommission times, and an
> OpenRouter bearer token. Users can only access their own cluster. A deleted cluster (timeout or
> explicit) returns 404 with text noting it may never have existed or may have been deleted — this
> avoids needing to keep a record of every deleted ID. A cluster still being deleted returns 410
> Gone; the 410→404 transition tells the caller deletion has completed.
>
> The third endpoint lists all clusters for the given user — useful after a restart, or when a
> user is at their limit and needs IDs to delete. It includes a cluster until deletion is fully
> complete.
>
> The fourth endpoint deletes a cluster. Only the owning client account or the system itself can
> delete it. The cluster may stick around briefly after the call while teardown proceeds. A
> cluster mid-deletion still counts toward the max-clusters limit (otherwise a create/delete loop
> is a DoS vector — unlikely for a frontier LLM to do on purpose, but a small local model might do
> it accidentally).
>
> This system needs documentation for setup and administration, performed by humans. It should
> keep detailed logs, structured for both human readability and automated analysis, written under
> something like `~/.local/share/spawner_service/logs`, with a background process to compress and
> rotate them daily.
>
> The implementation should be very careful to configure containers/VMs to prevent escape. Even
> though Supabase is trusted code, supply-chain attacks are still a concern, and we want to
> minimize blast radius. Code from the LLM (edge functions) is certainly not trusted.

**Naming note:** the writeup above calls the service account `spawner_service`; the actual
implementation uses `app_salmon` throughout (crate name, binary name, service/Unix-account name,
default config paths).

## 3. Downstream context brief

Summarized from a discussion of the RainQueue and OpenBrain projects — the first intended
consumers of App Salmon, provided for context on what App Salmon's clusters need to support, not
as a spec for App Salmon itself:

- **App Salmon provisions bare instances only.** It never runs application migrations, creates
  schemas/tables, or seeds data — that's each consumer's own test harness's job, running against
  the bare instance App Salmon hands back. The one deliberate exception is enabling the
  `pgvector` *extension* when the caller's request explicitly asks for it (`ServiceSpec.pgvector`)
  — that's "making a requested capability available," not schema/data setup.
- **pgvector is required**, not optional, for realistic OpenBrain/RainQueue integration testing —
  both systems store embeddings on a `thoughts` table and rely on vector similarity search.
- **Consumer schemas deliberately avoid fixed Postgres enum types** for fields like task status,
  specifically so new values can be added without a migration. This is a constraint on *what
  those systems do with the Postgres instance*, not on App Salmon — it's included here only so
  the implementer understands App Salmon must not impose a rigid schema on callers.
- **A prior credential-leak-in-logs incident** (an API key leaking into logs, plus a
  double-encoded-request-body-in-error-logs bug) makes the secrets-never-in-logs requirement a
  live concern, not a theoretical one — see §6 for how this shaped the implementation.
- **Least privilege and auditability are recurring values** for this user across related
  systems: agents should get scoped/ephemeral credentials to the throwaway environment, never the
  underlying OpenRouter key; and knowing which agent/run consumed what (traffic, compute) is
  valued even where not yet explicitly required.

## 4. Phase-1 scope (what this phase actually built)

Per explicit decisions made before implementation began:

- **Transport:** plain TCP on `127.0.0.1`, bearer-token auth (`Authorization: Bearer
  <client_name>:<secret>`), no TLS. Deliberately chosen over a Unix domain socket even though UDS
  would be more secure for this phase, on the reasoning that the service isn't production-viable
  until several other deferred phases (TLS included) land anyway, so phase 1 should look as much
  like the eventual real transport as possible rather than optimizing a transport that will be
  replaced.
- **Backend:** Postgres + pgvector only, one Docker container per cluster. No Supabase, no tar
  upload, no edge functions, no OpenRouter proxy.
- **Sudo-based privilege separation is in scope now** (not deferred) — each configured client
  runs its clusters as its own pre-provisioned Unix account (`config.toml`'s `unix_user`), per
  the original design writeup, not a pool of interchangeable worker accounts shared across
  clients (see §6, and §10.1 for why the implementation briefly diverged from this and was
  brought back in line).
- All 4 endpoints, the full cluster lifecycle state machine, the layered `thiserror` error
  hierarchy, structured `tracing` logging, SQLite-backed durable state with startup
  reconciliation, and a TTL reaper — all implemented and unit-tested against fakes, plus an e2e
  suite against real Docker/sudo/Postgres.

## 5. Architecture summary

### Module map

```
src/
  main.rs, lib.rs        # binary entrypoint / library root
  config.rs               # TOML config, validated at load — never a panic
  telemetry.rs             # tracing-subscriber init (JSON file + human stderr)
  error.rs                  # ApiError — the only IntoResponse impl in the crate
  redacted.rs                # Redacted<T>: no Serialize, Debug-redacted — secrets can't leak by accident
  auth/                        # bearer-token extractor, ClientRegistry, SHA-256 + constant-time compare
  domain/
    cluster.rs                  # Cluster, ClusterState, ClusterEvent, pure transition(), ClusterError
    ids.rs, service_kind.rs        # ClusterId (ULID), ClientId, WorkerUser, ServiceKind, ConnectionInfo
  ports/                            # trait seams: ContainerRuntime, PrivilegedExecutor, Clock, SecretGenerator, ClusterRepository, Filesystem
  adapters/                          # real impls: bollard, sudo Command::spawn, system clock, OsRng, rusqlite, tokio::fs
  client_workers.rs                   # static per-client Unix-account mapping + directory-slot path helper
  backends/postgres.rs                  # the one real ClusterBackend impl (phase 1)
  service/
    cluster_service.rs                    # create/info/list/delete business rules (no I/O to Docker/sudo)
    spawn_task.rs, teardown_task.rs          # background tokio tasks; teardown is reused by both
    ttl_reaper.rs, reconciliation.rs           # periodic TTL sweep; startup DB-vs-Docker reconciliation
    log_rotation.rs                              # bespoke gzip+prune (tracing-appender only rotates by time)
  http/handlers.rs, http/openapi.rs               # axum handlers + utoipa/Swagger UI mounted at "/"
tests/e2e/*                                        # real Docker + real sudo + real worker accounts
scripts/setup-e2e-env.sh                            # provisions worker accounts, sudoers rule, pulls the image
```

### Cluster state machine

One `enum ClusterState { Spawning, Ready, Failed, Deleting }` field on a `Cluster` struct — not
typestate — because independent tokio tasks (an HTTP handler, the spawn task, the TTL reaper,
startup reconciliation, possibly across a process restart) mutate the *same persisted row* from
different call stacks, so the state can't live in a compile-time type. What the type system buys
instead: `domain::cluster::transition()` is a pure function with an exhaustive match and no
wildcard arm, so adding a state or event later fails to compile until every call site is
reconsidered. `Gone` is deliberately not a variant — it's the absence of a row; teardown's last
act is deleting the row, which is what flips `GET /clusters/{id}` from `410` to `404`.

A `Failed` state was added beyond the original writeup (image-pull failure, health-check timeout,
or a directory-preparation failure mid-spawn all need somewhere to go) — `GET` returns `200` with a sanitized
error summary, not an error status, since a caller needs to be told to stop polling.

**Quota counting:** every row in `{Spawning, Ready, Failed, Deleting}` counts against the 2/user
limit — i.e. everything except an absent row — extending "mid-deletion still counts" to `Failed`
too, so unlimited free spawn-failure retries isn't a DoS vector. The check-then-insert is one
atomic SQLite transaction (`ClusterRepository::try_insert_if_under_quota`), closing the TOCTOU
race a naive check-then-insert would have; `adapters::sqlite_repository`'s
`quota_holds_under_concurrent_creates` test fires 6 concurrent creates against a limit of 2 and
asserts exactly 2 succeed.

**A `DELETE`-vs-spawn-completion race and its fix:** cancelling a `Spawning` cluster's background
task uses a `CancellationToken` (`service::deps::TaskRegistry`), but the token can only cancel a
spawn task the HTTP handler still finds registered *and in `Spawning`* — it can't stop a spawn task
whose `do_spawn` future has already resolved by the time the `DELETE` handler gets around to
signalling it. In that narrow window, `spawn_task::run` used to unconditionally write `Ready`
(using its own stale in-memory view of `cluster.state`), silently clobbering the `Deleting` row
`ClusterService::request_delete` had just persisted — the cluster would then survive until the TTL
reaper eventually caught it, despite the `DELETE` having returned `202`. Fixed by having `run`
re-fetch the row's *current* persisted state after `do_spawn` completes, before writing any
conclusion: if it's already `Deleting`, `run` tears down what it just allocated instead of
persisting `Ready`/`Failed` over it. See `service::spawn_task`'s
`spawn_succeeding_after_a_concurrent_delete_tears_down_instead_of_clobbering_deleting` test.

### Error hierarchy

`thiserror` enums sized to what each layer's *direct caller* needs to match on, composed via
`#[from]`/`#[source]` — no `Box<dyn Error>`/`anyhow` anywhere. `DockerError`, `PrivilegedExecError`,
`ClientWorkerError`, `RepositoryError`, `AuthError`, `ConfigError` at the bottom; `ClusterError` in
the middle (what `ClusterService`'s callers match on); `ApiError` at the top — the only type with
an `IntoResponse` impl, mapping 1:1 to HTTP status codes. `ClusterError -> ApiError` is a
hand-written `From` (not `#[from]`) since it branches on the wrapped variant. Ownership checks are
folded into `NotFound` at the repository layer (`get_owned(id, owner)`), so "doesn't exist" and
"exists but isn't yours" are indistinguishable everywhere — there's no `403` in this API by
design.

### Trait seams

`ContainerRuntime`, `PrivilegedExecutor`, `Clock`, `SecretGenerator`, `ClusterRepository` — each
has a real adapter and is fully unit-testable without touching the real external system:

- **`adapters::docker_bollard`**: unit tests point it at a small fake Docker Engine API server (a
  real `axum::Router` served over a temp Unix socket via `axum::serve`, which supports
  `tokio::net::UnixListener` directly) — the adapter's real `bollard` call sites execute for
  real, just against a stand-in daemon.
- **`adapters::sudo_exec`**: the `sudo` executable path is configurable; unit tests point it at a
  small fake shell script that passes through to the real underlying command (`mkdir`, `find`),
  exercising the real `Command::spawn`/argv-construction/exit-code-parsing code without real
  root.
- **`adapters::sqlite_repository`**: tested against both an in-memory SQLite DB and a real file
  path (`open()`, not just `open_in_memory()`).

## 6. Sudo/per-client-account security model — read this before assuming more than it provides

`app_salmon`'s own `bollard` connection drives *all* container lifecycle (create/start/inspect/
stop/remove) — `bollard` is an in-process async client, you cannot "sudo" a library call. Sudo
(`sudo -u <client's unix_user>`, via `PrivilegedExecutor`, restricted to a **closed enum** of two
operations — `PrepareWorkerDir`/`WipeWorkerDir`, never arbitrary argv) is used only to
create/own/wipe the per-cluster working directory that gets bind-mounted into the container,
which is itself configured with `--user <uid>:<gid>`.

**Each configured client runs its clusters as its own Unix account (`config.toml`'s `unix_user`),
not a pooled/shared account** — see §10.1 for why this matters and the (brief) history of the
implementation diverging from it. A client can hold up to `max_clusters_per_user` clusters at
once, so its account's directory is further scoped by a small **directory slot** (`0..
max_clusters_per_user`, e.g. `slot-0`, `slot-1`), assigned atomically at cluster-creation time by
`ClusterRepository::try_insert_if_under_quota` (the same transaction that enforces the quota —
see §5's TOCTOU note; a free slot always exists whenever the owner is under quota, by pigeonhole).
Fixed, literal, enumerable slot paths (rather than one directory per cluster id) are what let
`scripts/setup-e2e-env.sh` write a `/etc/sudoers.d` rule listing exactly `max_clusters_per_user`
allowed paths per client, with **no wildcard** — confirmed necessary, not merely tidier, in this
session: `sudo-rs` (increasingly Ubuntu's default `sudo`, and what this development environment
itself uses) rejects wildcards embedded in command arguments outright at `visudo -c` time
(`syntax error: wildcards are not allowed in command arguments`), so a rule scoped by cluster id
(unbounded, unenumerable) would have silently failed to install on any `sudo-rs` machine — the
first cluster-directory sudo call would then hard-fail with a permission denial. See §8a for the
verification status of everything else in this area.

**What this phase-1 privilege separation actually is: a file-ownership / attribution / blast-radius
boundary, not a container-escape boundary.** The Docker daemon itself still runs as root either
way — a compromised Postgres container can still reach the daemon's root, because the daemon's
root is the daemon's root regardless of which uid the container process runs as. This is
acceptable *only* because phase-1 workloads are a trusted, pinned image (`pgvector/pgvector`), not
LLM-authored code. What it does buy: every cluster's on-disk state has a distinct uid (a cleanup
bug can't let cluster B read cluster A's leftover data directory), `ps`/`lsof`/audit logs
attribute activity to a specific client's account rather than an undifferentiated `app_salmon`
(which matters for "centralize privilege in one *audited* service," a goal independent of
container escape resistance) — and, unlike a shared worker pool, **one client's account never
runs code on another client's behalf**, so a capability eventually granted to one client's account
(a real, confirmed future requirement — differentiated per-client capabilities) can't leak to a
different client merely through account reuse.

The real escape-resistance boundary is deferred to the Kata-Containers phase (§7c) — the same
`ContainerRuntime` port gets re-targeted at a `runtime=kata` container spec, and *that* is where
per-worker uid separation starts to matter for genuine isolation, because Kata's guest kernel plus
the worker uid together bound what a compromised edge function can touch on the host.

## 7. Deferred phases, in priority order

**(a) Supabase spawning + untrusted-tar validation.** The caller-supplied tar containing the
Supabase directory must be validated entry-by-entry (reject symlinks/hardlinks escaping the
target, absolute paths, `..` components, device/fifo/socket special files, oversized entries) —
never trust `Archive::unpack` directly on untrusted input.

**(b) OpenRouter proxy** with per-cluster scoped bearer tokens and traffic auditing, without ever
logging the underlying OpenRouter key or full request/response bodies (see the credential-leak
history in §3 — this is the phase most likely to reintroduce that class of bug if built
carelessly).

**(c) Kata-Containers-backed edge-function sandboxing** — see §6. Kata is OCI/Docker-API
compatible, so the existing `bollard`-backed `ContainerRuntime` port should need only a runtime
option change, not a rewrite.

**(d) TLS/mTLS transport + per-client public keys**, replacing the plain-TCP-bearer-token
transport phase 1 deliberately accepted as non-production-viable.

**(e) Log rotation is already bespoke** — `tracing_appender::rolling::daily` only handles
time-based file rotation, not compression or retention pruning, so `service::log_rotation` fills
exactly that gap (already implemented in phase 1; noted here for the record since it was an open
question in the original design).

**(f) Full admin/operator runbook.** Phase 1 ships `scripts/setup-e2e-env.sh` as a stub covering
per-client account creation, the sudoers rule, and image pulling — a human still needs to: create
the `app_salmon` service account itself, provision one Unix account per real client (no capacity
planning beyond that — unlike a shared pool, there's no separate "pool size" to plan, only
`max_clusters_per_user` per client, which bounds that client's own directory-slot count), and set
up the actual daemon/process supervision (systemd unit, log directory permissions, etc.) for
running `app_salmon` itself in production.

**(g) Revisit the 30s/3600s TTL bounds and the 2-per-user limit** once there's real usage data —
both are still first-guess values from the original design writeup.

## 8. Operator prerequisites (stub — full runbook is deferred, §7f)

This section describes prerequisites for running the `app_salmon` *service* on a real host —
production or otherwise. Its e2e *test suite* is a separate concern, covered entirely by §8c/§8d
now: it only ever runs inside a disposable VM, never directly against a real host.

- The `app_salmon` config (`config.toml`) needs: a `[[clients]]` entry per client account, each
  with `secret_hash = "sha256:<64 hex chars>"` (the hash of a secret generated and distributed to
  that client account out of band — no CLI tooling for this yet, `sha256sum` a random string by
  hand) and `unix_user = "<name>"` naming the pre-provisioned Unix account that client's clusters
  run as (must be unique across clients — `Config::validate` rejects two clients sharing one).
- The service needs read access to `/etc/passwd` (to resolve worker uid/gid), a writable
  `storage.sqlite_path` directory, a writable `logging.log_dir`, and access to the Docker socket
  (`docker.socket_path`).

## 8a. Formerly-open risk: the real Postgres/arbitrary-uid path — now confirmed, 2026-07-13

**Resolved.** This section originally flagged, as the single most likely thing to break the happy
path: `backends::postgres` runs the stock `postgres`/`pgvector` image with `--user
<worker-uid>:<worker-gid>` against a bind-mounted, worker-owned `PGDATA` directory (see §6) — an
arbitrary non-root uid with no matching `/etc/passwd` entry *inside* the container, writing to a
bind mount whose ownership was set by a host-side `chown`, historically a fragile spot for the
official Postgres image. **Confirmed working for real** via `just e2e-vm-up` + `just
e2e-vm-test` against a real KVM host (§8c): `create_cluster::valid_request_eventually_becomes_ready`
passed, along with all 17 other e2e tests, in a real guest with real Docker, real `sudo`, and a
real `pgvector/pgvector:pg16` container. `wait_until_ready`'s dependency on `pg_isready` being on
`PATH` inside the container (see §9) is confirmed by the same run — the healthcheck-driven
readiness path worked end to end, not just the container starting.

This does not, on its own, confirm every environment ever will behave the same (different
Postgres image versions, different Docker storage drivers, etc. remain theoretically possible
sources of divergence), but it retires this section's original "this has never been exercised
against a real Docker daemon in any session" caveat — it now has been, successfully.

**Newly verified this session, unlike the two risks above:** the `sudo-rs` wildcard rejection
described in §6 *was* confirmed directly, via `visudo -c -f` against a hand-written sudoers
snippet in this sandbox (no root needed for a syntax-only check) — first against a wildcard-based
rule (`.../client/*`), which failed with `syntax error: wildcards are not allowed in command
arguments`, then against the literal-slot-path rule actually shipped, which parsed cleanly. What's
still unverified is *runtime* behavior on a real target machine: whether that machine's `sudo` is
classic sudo or `sudo-rs` (both are plausible depending on the OS/version), and — for classic
sudo — whether the literal, wildcard-free rule this session settled on is accepted identically
(it should be, since it uses no wildcard syntax at all, but "should be" is not "confirmed", per
the standard set by the rest of this section).

**Load-bearing cross-artifact invariants — nothing in the codebase enforces these; they must be
kept in sync by hand:**
- `limits.max_clusters_per_user` in `config.toml` must equal `MAX_CLUSTERS_PER_USER` (env
  `APP_SALMON_MAX_CLUSTERS_PER_USER`, default `2`) passed to `scripts/setup-e2e-env.sh`.
  `try_insert_if_under_quota` assigns slots in `0..limits.max_clusters_per_user`; the script
  pre-creates directories and sudoers entries for `0..MAX_CLUSTERS_PER_USER`. If the config value
  is raised without re-running the script (or the script is run with a smaller value), a cluster
  can be assigned a slot the script never provisioned — the row inserts successfully, and the
  background spawn task's privileged `mkdir` for that slot is then denied by `sudo`/`sudo-rs` at
  runtime. No unit test catches this: it's purely a config/script agreement, invisible until an
  e2e run actually exhausts the lower slot count. Operators changing `max_clusters_per_user` must
  re-run `scripts/setup-e2e-env.sh` (or the production equivalent) before the new limit takes
  effect.
- `storage.sqlite_path`'s parent directory (`<parent>/workers`, i.e. `worker_data_dir_base`) must
  match `WORKER_DATA_DIR_BASE` (env `APP_SALMON_WORKER_DATA_DIR_BASE`) used by the same script —
  otherwise the directories the script `chown`s and the sudoers rule authorizes are not the ones
  the running server actually computes via `client_workers::worker_data_dir`, and every spawn
  fails the same way.
- Minor, edge-of-an-edge note: `try_insert_if_under_quota`'s slot-assignment step silently skips
  any existing row whose persisted JSON fails to parse (`filter_map(...ok())`) when computing
  `used_slots`, while the quota *count* above it still includes that row. A corrupt row could
  therefore theoretically cause a new insert to be assigned a slot already in use by the corrupt
  row. Not fixed here (no reproduction, no test infrastructure currently writes corrupt rows) —
  flagged for awareness, not treated as a phase-1 blocker.

## 8b. Removed: one-shot ephemeral VM e2e testing (`test-e2e-vm`)

An earlier version of this section described `just test-e2e-vm` (→ `scripts/vm/run-e2e-in-vm.sh`
+ `scripts/vm/guest-init.sh`): boot, provision, run the e2e suite, and discard a VM in one
fire-and-forget cloud-init script, no interactive session with the guest at any point. It used
the same `-virtfs local,...,security_model=none` 9p share §8c's persistent VM originally used —
and hit the identical bug (§8c, and §8a's now-resolved arbitrary-uid entry): `security_model=none`
passes the host's raw uid/gid/mode through to the guest, so the non-root `ubuntu` user running the
actual test suite got `Permission denied` on the shared `/repo`, confirmed on a real KVM run
(2026-07-13). §8c's fix was to copy the repo in over SSH instead of sharing it live — but this
one-shot path has no SSH or other synchronous channel to the guest to reuse for that fix; adding
one would mean redesigning it to look like §8c's `up`/`test`/`down` composed together, at which
point there was no reason to keep it as a second, separately-maintained implementation of the same
idea. **Removed rather than fixed twice.** `just e2e-vm-up` / `e2e-vm-test` / `e2e-vm-down` (§8c)
is now the only VM-based e2e path.

## 8c. Persistent VM for iterative testing — `just e2e-vm-up` / `e2e-vm-test` / `e2e-vm-down`

This is the VM-based e2e path — the only one (§8b describes an earlier one-shot variant, removed
after this section's first real run found the bug described below and fixing it here turned out
to make the one-shot variant redundant). `just e2e-vm-up` boots a VM once and leaves it running;
`just e2e-vm-test` (which `just ci` also uses automatically when such a VM is up — see below) runs
the suite against it over SSH in a few seconds instead of minutes once it's up; `just e2e-vm-down`
tears it down and wipes its disk. Host prerequisites: `just setup-e2e-vm` — the *only* sudo used
anywhere in this path, a one-time KVM group grant.

**How `just ci` finds it:** `ci`'s e2e step runs `scripts/vm/e2e-vm-status.sh` first (silently);
if that reports the persistent VM up, `ci` runs the suite against it (`just e2e-vm-test`) instead
of the bare-host path. `ci` never boots a VM itself — that's a multi-minute cost, too heavy for a
gate you might run many times while iterating — it only *uses* one if you already started it. If
neither the persistent VM nor the bare-host setup is available, `ci` prints all three options
(persistent VM, one-shot VM, bare host) so the omission is never silent.

**Design:**
- `scripts/vm/lib.sh`: host-prereq checks, base-image download+verify, and seed-ISO building,
  plus persistent-VM-specific helpers: `vm_find_free_port`
  (asks the OS for a free `127.0.0.1` port via a throwaway Python socket bind — a small, accepted
  TOCTOU race for a local single-user dev tool), `vm_pid_is_our_qemu` (checks `/proc/<pid>/cmdline`
  contains both `qemu-system-x86_64` and a marker — the overlay disk's path — so a stale pidfile
  whose PID has been reused by an unrelated process can't be mistaken for our VM), `vm_sync_repo`
  (see "getting the repo into the guest" below), and `vm_ssh` (see security model below).
- `scripts/vm/e2e-vm-up.sh`: idempotent (does nothing if a healthy instance is already up),
  reaps a stale state dir (dead or mismatched-identity pidfile) if one's left over from a crash,
  generates a fresh ed25519 host keypair and client keypair, picks a free port, boots with
  `-netdev user,id=net0,hostfwd=tcp:127.0.0.1:<port>-:22` (see security model — the `127.0.0.1`
  is load-bearing, not decorative) plus `-name guest=app-salmon-e2e-persistent -pidfile
  <state>/qemu.pid -daemonize` so it backgrounds itself and leaves a real PID for later checks,
  waits for SSH, then syncs the repo in and runs `guest-provision.sh` over SSH and marks the
  instance `provisioned`.
- **Getting the repo into the guest — `vm_sync_repo`, not a live share.** The first version of
  this script exported the checkout via `-virtfs local,...,security_model=none`, the same
  mechanism the now-removed one-shot path (§8b) used. **The first real KVM run of this tooling
  (2026-07-13) found that this is broken for any host checkout with normal-or-tighter
  permissions**: `security_model=none`
  passes the host's raw uid/gid/mode straight through to the guest, so `ls -ld` on the mounted
  share showed the guest reporting the exact host owner (uid/gid `UNKNOWN` inside the guest, no
  matching `/etc/passwd` entry) and mode. Root always bypasses that check (which is why
  provisioning — root, via `sudo` — worked in that first run), but the non-root `ubuntu` user hit
  `Permission denied` on a bare `cd /repo` the moment it tried to run the suite as itself. Fixed
  by dropping `-virtfs` entirely: `vm_sync_repo` instead `tar`s the checkout (excluding `.git`,
  `target/`, and this tooling's own `.e2e-vm-state/`/`.e2e-vm-result/` — the exclusion of
  `.e2e-vm-state` matters specifically because that's where the SSH private keys live, so the
  guest never receives its own keys) and pipes it over the same SSH channel used for everything
  else, replacing `/repo` (owned by `ubuntu`) on every call. Two things this bought beyond just
  fixing the bug: one fewer untested subsystem (the guest kernel's 9p module / virtio-9p
  transport no longer need to work at all), and a *stronger* isolation property than the live
  mount had — the guest can no longer write back into the real host checkout under any
  circumstance, not just "isn't expected to." The cost is a sync step before every provision/test
  run rather than zero-latency live edits, but for a project this size that's a sub-second-to-low-
  single-digit-second `tar | ssh | tar`, not a meaningfully different iteration speed — and
  because it runs on every call (not just once at `e2e-vm-up`), "always tests current code" still
  holds.
- `scripts/vm/guest-provision.sh`: the apt/rustup/`setup-e2e-env.sh` portion of the old
  `guest-init.sh`, factored out and made the *only* copy of that logic — both `e2e-vm-up.sh` (once,
  after first boot) and `e2e-vm-run-tests.sh` (every call) run it, and because every step in it
  checks current state before acting, a fully-provisioned guest re-running it pays only a few
  seconds of checks. **This is what makes a future phase's new e2e prerequisites "just work"**
  the next time someone runs the suite against an already-up VM, with no version tracking needed
  anywhere — v0.2.0 adding a new apt package or a new `setup-e2e-env.sh` step is picked up
  automatically by the next `just e2e-vm-test` or `just ci`.
- `scripts/vm/e2e-vm-run-tests.sh` / `e2e-vm-status.sh` / `e2e-vm-down.sh`: thin wrappers around
  `lib.sh`'s `vm_persistent_is_up`/`vm_persistent_is_provisioned`/`vm_reap_stale`.
  `e2e-vm-run-tests.sh` re-syncs the repo and re-runs `guest-provision.sh` before every test run,
  same reasoning as `e2e-vm-up.sh`. `down` tries a graceful `sudo poweroff` over SSH first, falls
  back to `SIGTERM`/`SIGKILL` on the qemu process, then always wipes the whole state directory
  (overlay disk, keys, logs) — the persistence boundary this session settled on is "within a
  session," not across `down`/`up` cycles, matching the rest of this tooling's ephemeral-by-
  default philosophy.
- **State directory:** `<repo>/.e2e-vm-state/` (gitignored, `0700`) — scoping it under the
  checkout itself, rather than e.g. a hash under `/tmp`, is what makes "one persistent VM per
  checkout" true with no extra bookkeeping: two clones naturally get two state dirs and two VMs.
  Contains the overlay disk, seed ISO, both SSH keypairs, a pinned `known_hosts`, the console
  log, qemu's pidfile, and a `provisioned` marker. Deliberately excluded from what gets synced
  into the guest (see above) — the guest has no reason to ever see its own host-side key material.

**Security model for the SSH transport** (attack surface specific to this path — a design with no
inbound listener at all wouldn't have it):
- **The forwarded port is bound to `127.0.0.1` only** (`hostfwd=tcp:127.0.0.1:<port>-:22`, not
  the bare `hostfwd=tcp::<port>-:22` form, which binds `0.0.0.0` and would expose the guest's
  sshd to the whole network — an easy mistake, since most hostfwd examples online use the bare
  form). This stops network-remote attackers.
- **Loopback binding alone is not enough**, because TCP loopback isn't scoped to the owning user
  the way a Unix socket's file permissions are — any other local account on a shared host can
  still `connect()` to a `127.0.0.1`-bound port. What actually stops them from getting a shell in
  the guest is **pubkey-only authentication**: `ssh_pwauth: false` in the guest's cloud-config,
  plus a client keypair generated fresh per instance (`0600`, deleted with the rest of the state
  dir on `down`). This is load-bearing, not defense-in-depth on top of the loopback bind.
- **The guest's SSH host key is generated on the host, not trust-on-first-use.** `e2e-vm-up.sh`
  runs `ssh-keygen` itself and injects the resulting keypair into the guest via cloud-init
  `write_files` (`ssh_genkeytypes: []` stops cloud-init from generating/overwriting it), so the
  host already knows the exact key before ever connecting and pins it in a per-instance
  `known_hosts` (`vm_ssh` always passes `StrictHostKeyChecking=yes` against that file, never
  `/dev/null`/`accept-new`). This closes the TOFU gap a freshly-booted VM would otherwise have,
  and doubles as what makes `vm_persistent_is_up`'s SSH probe a *reliable* health check — a
  probe against `StrictHostKeyChecking=no` could be fooled by some other process holding the
  port; a pinned-host-key probe can't.
- **No 9p share at all**, as of the fix described above — the guest can never write back into
  the real host checkout under any circumstance (not even root inside the guest can reach it;
  there's no channel to). The only thing that crosses the SSH boundary is what `vm_sync_repo`
  explicitly tars up and whatever test output/exit codes come back.

**Verification status:**
- **Fully boot-verified end to end against a real KVM host, 2026-07-13, across two runs.** Run 1
  confirmed VM boot, cloud-init's `write_files`/`ssh_authorized_keys`/host-key injection, and SSH
  — the piece flagged as least certain (untested cloud-init module ordering) — all working exactly
  as designed, and also surfaced the 9p-permission bug described above (found and fixed the same
  session). Run 2, after that fix (tear down, rebuild with no 9p share, `vm_sync_repo` instead):
  `just e2e-vm-up` completed with no issues, and `just e2e-vm-test` — sync, idempotent
  re-provisioning, then the full e2e suite — passed all 18 tests, including
  `create_cluster::valid_request_eventually_becomes_ready` (see §8a: this is also the first real
  confirmation of the arbitrary-uid Postgres path). The mountpoint guard on `vm_sync_repo` was
  also confirmed for real in this same session, refusing to run against the still-9p-mounted
  pre-fix VM exactly as designed, before the rebuild.
- **The SSH security model was independently verified for real in this sandbox, without needing
  KVM** — a real local `sshd` stood in for the guest's sshd (same host key, same
  `AuthorizedKeysFile` pointed at the generated client key, same `PasswordAuthentication no`),
  and `vm_ssh`'s exact option set was run against it:
  - A connection with the correct pinned host key and correct client key succeeds.
  - A connection with the *wrong* pinned host key in `known_hosts` is rejected outright by SSH's
    own host-key-changed warning (`Host key verification failed`) — confirming the pinning is
    load-bearing, not merely present.
  - A connection with the wrong client key is rejected with `Permission denied (publickey)` and
    no password fallback — confirming pubkey-only auth is actually enforced, not just configured.
- **`vm_sync_repo`'s tar-over-SSH mechanics were also verified for real** against that same
  stand-in `sshd`: a fake checkout containing `.git/`, `target/`, and `.e2e-vm-state/` (with a
  fake key file inside it) was synced end-to-end, and the result confirmed to contain the real
  source files byte-identical to the original, with all three exclusions actually excluded — in
  particular, confirming the guest never receives `.e2e-vm-state`'s key material.
- `vm_pid_is_our_qemu` was exercised against a real (tcg-accelerated) `qemu-system-x86_64`
  process booted with the exact `-name`/`-pidfile`/`-daemonize` flags this script uses, including
  its two negative cases (wrong marker, dead PID) — all three branches behave as designed.
- The persistent VM's cloud-init `user-data` (host key + pubkey `write_files`, `ssh_authorized_keys`)
  was parsed with a real YAML parser and the base64-embedded host keypair round-trips byte-for-byte.
- `qemu-img create -F qcow2` and the `-daemonize`/`-pidfile` combination were both exercised for
  real (the latter confirmed to write the correct PID before the launching shell command returns).

`e2e-vm-down.sh`'s teardown path (graceful SSH `poweroff`, state-dir wipe) was also exercised for
real as part of the same session, between the two runs above — used specifically to retire the
pre-fix VM before rebuilding with the `vm_sync_repo` fix. At this point every command in the
`setup-e2e-vm` → `e2e-vm-up` → `e2e-vm-test` → `e2e-vm-down` cycle has been run for real against a
real KVM host at least once.

## 8d. Removed: bare-host e2e testing (`test-e2e`/`setup-e2e`)

Before §8c, `just setup-e2e` (`scripts/setup-e2e-env.sh`, run as root) plus `just test-e2e`
(`cargo test --test e2e`) was the only way to run the e2e suite: directly against the real host,
creating persistent `e2e-agent`/`e2e-agent-other` Unix accounts and a `/etc/sudoers.d` rule that
stuck around afterward. Removed once §8c covered the same need without that persistence.

**Explicit reasoning for removing it outright, rather than keeping it as a documented fallback**
(the decision that shaped this, and worth stating for future readers of this file): a code path
that nothing exercises decays silently, and this project had just watched that happen — the
one-shot VM path (§8b) sat broken through the per-client-account refactor without anyone
noticing, precisely because nothing was running it. Keeping `test-e2e`/`setup-e2e` "just in case"
would have reintroduced exactly that risk: a second e2e entry point nobody runs day to day,
quietly drifting out of sync with whatever `tests/e2e/*` or `scripts/setup-e2e-env.sh` need next,
discovered broken only when someone actually reaches for it. If a line of code matters enough to
keep, it has to be exercised, or the question of whether it still works is just unanswered — and
if it isn't exercised, keeping it costs real maintenance burden for a guarantee that doesn't
actually hold. `scripts/setup-e2e-env.sh` itself was **not** deleted — `guest-provision.sh` still
calls it, now exclusively inside the disposable VM (§8c), where it's exercised on every
`e2e-vm-up`/`e2e-vm-test` and therefore actually kept honest.

## 9. Testing & coverage

- `just ci` — the single command: format check, clippy (deny-on-warnings, `--all-targets
  --all-features`), unit tests, and the e2e suite if a persistent e2e VM is already up (§8c) —
  never a silent skip; prints exactly what to run if one isn't.
- `just test-unit` — independently runnable, per the requirement that unit and e2e stay
  separable.
- `just setup-e2e-vm` / `e2e-vm-up` / `e2e-vm-test` / `e2e-vm-down` — the e2e suite's only path
  (§8d), runs it inside a disposable, persistent QEMU VM, so `setup-e2e-env.sh`'s host-level
  changes never touch the machine actually running the tooling; this machine only needs QEMU +
  `/dev/kvm` access, which `setup-e2e-vm` gets for you (sudo used once, for two generic
  non-App-Salmon-specific things). The VM stays up across multiple test runs instead of being
  discarded every call, and `just ci` detects and uses it automatically once it's up. See §8c for
  the design, the SSH transport's security model, and its verification status (confirmed working
  end to end against a real KVM host, including all 18 e2e tests and the arbitrary-uid Postgres
  path from §8a).
- `just coverage` (needs `cargo install cargo-llvm-cov` + `rustup component add
  llvm-tools-preview` once per machine) — measures the **entire** `--lib` target, no
  `--ignore-filename-regex` carve-out for adapters. Current state (2026-07-12): 96.2% region /
  97.8% line coverage — not literally 100%; see below for what accounts for the remaining lines
  and why each category is or isn't worth closing. Every adapter (`docker_bollard`,
  `sudo_exec`, `sqlite_repository`, `tokio_filesystem`) is covered via a fake stand-in for the
  *external system* (fake Docker Engine API server, fake sudo script, real SQLite, real tempdir
  filesystem + an injectable fake for error paths), not just a Rust-level fake of the port trait —
  so "100%" was never narrowed to exclude adapters as a category.
- **`mockall` is now used for repository fault-injection fakes** (`spawn_task`, `teardown_task`;
  `reconciliation`'s `FlakyRepository` was deliberately left as a hand-rolled fake — see below).
  `#[cfg_attr(test, mockall::automock)]` above `#[async_trait]` on `ClusterRepository` generates
  `MockClusterRepository`, verified to satisfy `Send + Sync + 'static` through a real
  `Arc<dyn ClusterRepository>` → `tokio::spawn` path. Prompted by a correction: hand-rolled fakes
  that implement a whole port trait just to override one method, with the rest as unexercised
  pass-through delegates, were flagged as a design smell — since `spawn_task::run`/
  `teardown_task::teardown` each only call 1-4 of `ClusterRepository`'s 7 methods, mocking means
  configuring (`.expect_*()`) only what's actually called, asserting on call arguments
  (`.withf()`/`.times()`) instead of reading storage back through a delegating fake. This
  eliminated the unused-delegate-method coverage gap entirely rather than chasing it test-by-test.
  Not used for `reconciliation`'s `FlakyRepository`: those tests seed multiple rows and assert
  real multi-row sweep behavior, which mocking fights rather than helps — pick per case, not one
  hammer for every fake.
- **`backends::postgres`'s readiness check was redesigned** (2026-07-12), not just re-tested: it
  no longer dials Postgres itself with `tokio_postgres::connect()` to determine readiness. It now
  sets a Docker `HEALTHCHECK` (`pg_isready -U app_salmon -d app_salmon`, `CMD-SHELL`, on
  `ContainerSpec::health_check` — see `ports::container_runtime::HealthCheck`/`HealthState`) and
  polls `ContainerRuntime::inspect`'s new `health` field until `Healthy`, mirroring every other
  status check this backend already does. This followed research (prompted by "look for a crate
  before hand-rolling this, and consider whether it belongs in a separate library crate") that
  found no crate lets App Salmon avoid owning this logic in production — `testcontainers` and
  similar are test-only tools built around their own container lifecycle, incompatible with our
  `bollard`/`ContainerRuntime` setup — but surfaced that the *design itself* was solving a problem
  Docker already solves: `bollard::models::HealthConfig` (settable on create) and
  `ContainerState.health` (returned by `inspect`) already do exactly what the hand-rolled
  `tokio_postgres::connect()` retry loop was reimplementing. The result: the success path is now
  covered by a unit test (`spawn_succeeds_once_the_healthcheck_reports_healthy`) via the existing
  fake Docker Engine API server, with **no new test infrastructure** — it was never really a
  "testing problem" needing a fake Postgres wire-protocol server (the option this section
  previously proposed and the user correctly redirected away from); it was a design that put a
  redundant readiness check in the wrong layer. `connect()` still exists for exactly one thing:
  the `pgvector` `CREATE EXTENSION IF NOT EXISTS vector` call once health confirms readiness — that
  genuinely needs a real Postgres connection and remains e2e-only
  (`create_cluster::pgvector_flag_enables_the_extension`), now a much smaller, honestly-scoped
  exception than "the whole readiness path."
- **`service::log_rotation` is now generic over a new `ports::filesystem::Filesystem` port**
  (2026-07-12), closing the directory-iteration/per-entry-stat error branches that were previously
  flagged as needing either a flaky real filesystem race or a new port. Built as a **generic bound,
  not `dyn`** — deliberately inconsistent with every other port in this crate, and documented as
  such in `ports/filesystem.rs`: `log_rotation` has exactly one production implementation
  (`adapters::tokio_filesystem::TokioFilesystem`) wired in at its single call site in `main.rs`, so
  there's no runtime polymorphism to buy with a trait object, and a generic bound monomorphizes to
  the same code direct `tokio::fs` calls would produce — no vtable, no boxed futures. This needed
  care to actually deliver: a first pass using plain `async fn` in the trait compiled, but only
  because the concrete real adapter's future happened to be inferrable as `Send` — nothing in the
  trait *required* it, so `rustc` warned (`async_fn_in_trait`, denied under `-D warnings`) that a
  different, non-`Send` implementation would compile against the trait but fail wherever a caller
  tried to `tokio::spawn` a generic function built on it (which `log_rotation::run_forever` does).
  Fixed by spelling out `fn(..) -> impl Future<..> + Send` explicitly on every trait method — still
  zero-cost (implementations still just write `async fn`), but the `Send` guarantee is now checked
  at the `impl` site instead of surfacing as a confusing error at an unrelated `tokio::spawn` call
  site. Verified with a standalone prototype before touching production code.
- **The pooled worker-account subsystem was eliminated in favor of per-client accounts**
  (2026-07-12) — see §6 and §10.1 for the design rationale. `src/worker_pool.rs` (bounded
  free-list, acquire/release, `PoolExhausted`/`DoubleRelease` errors) is gone entirely; the
  replacement, `src/client_workers.rs`, is a stateless static mapping with no acquire/release
  lifecycle to test. This closed a real, unit-testable correctness gap discovered mid-change: two
  concurrent creates for the same client can each hold up to `max_clusters_per_user` clusters, so
  each cluster still needs its own on-disk directory — naming it by cluster id would have forced
  the sudoers rule to use a wildcard, which `sudo-rs` rejects outright (§6, §8a). The fix — a
  directory *slot* (`0..max_clusters_per_user`) assigned atomically inside the existing
  `try_insert_if_under_quota` transaction, the smallest slot not already used by one of the
  owner's other active rows — is covered by unit tests exercising exactly the property that
  matters and can be verified in a sandboxed environment with no root/Docker:
  `quota_holds_under_concurrent_creates` (strengthened to assert the two winners of 6 concurrent
  same-owner inserts get distinct slots, not just that exactly 2 succeed) and
  `a_freed_slot_is_reused_by_the_next_insert` in `adapters::sqlite_repository`, mirrored in
  `test_support::InMemoryClusterRepository`'s own `try_insert_if_under_quota` impl so every
  service-layer test gets the same guarantee. The `sudo-rs` finding itself was verified directly
  in this sandbox via `visudo -c -f` (no root required for a syntax-only check) — see §8a.
- **What's left, in three categories with different implications:**
  1. **Unreachable by design** (`cluster_service.rs:156`'s `?` after `transition(.., DeleteRequested)`
     — every `ClusterState` accepts `DeleteRequested`, so the error branch can't fire;
     `config.rs`'s and `docker_bollard.rs`'s defensive `Ok(_) => panic!(...)` /
     `Err(other) => panic!(...)` match arms that exist purely to fail loudly if a regression
     changes the guaranteed outcome; the real `tokio_filesystem::TokioFilesystem::compress`'s
     `spawn_blocking` join-error mapping, which would require a genuine task panic to hit for
     real — the *logical* branch it feeds is still covered, via a fake that returns the
     equivalent error directly). Not closeable without deliberately breaking the invariant being
     guarded — that would be testing a bug, not a feature. This is a documented floor, not debt.
  2. **Disproportionate to build, recommended against**: `backends::postgres`'s remaining
     `connect()`/`pgvector` path (see above) — the only piece left in this bucket after the
     readiness redesign.
  3. **Test-only scaffolding** — pass-through delegate methods on hand-rolled in-test fakes
     (`reconciliation`'s `FlakyRepository`/`ScriptedAliveness`, `test_support.rs`'s shared fakes)
     that satisfy a port trait's full surface but aren't all called by the specific tests that use
     them. Writing tests solely to color these green adds no assurance — it isn't application
     logic. Whether these count toward "100% of source lines we write" is a real, open question the
     user should decide explicitly: the original coverage-carve-out rejection was about not
     excluding *production adapters* from measurement, which is a different question from whether
     test-double interface-satisfaction stubs need their own dedicated tests. Most of this category
     was eliminated by the `mockall` migration above rather than answered — the residual is what's
     left in fakes mocking didn't fit.
- `tests/e2e/*` runs every endpoint and major variation over real HTTP against real Docker/real
  sudo/real worker accounts/a real Postgres container: valid + invalid TTL, over-quota create,
  pgvector enablement (verified via a real `tokio_postgres` connection checking `pg_extension`),
  info while spawning/ready/failed/deleting/gone, list scoping, delete by owner vs. non-owner,
  delete-while-spawning (cancellation), and TTL auto-expiry.

## 10. Open design questions carried forward

1. **Config's client→worker mapping — resolved (2026-07-12), was briefly a deviation.** The
   implementation originally allocated workers from a shared pool (`salmon-worker-00`, `01`, ...)
   at cluster-create time rather than statically mapping one Unix account per client, flagged at
   the time as "chosen deliberately" with no inline justification recorded. Revisited this session
   and reverted to the original design writeup's actual intent (§2: `spawner_service` sudos to
   "the users for each of the LLMs that can call it") once the underlying reason for a pooled
   account surfaced a real problem: a pooled/recycled identity that runs code on behalf of
   *multiple different clients* must carry the union of every capability any client it services
   might need — a security hole in waiting once any capability needs to vary per client, which was
   confirmed as a real, near-term requirement ("different client capabilities is DEFINITELY a
   thing"), not a hypothetical. Per-client accounts also delete a whole subsystem
   (`WorkerPool`'s acquire/release/exhaustion bookkeeping, `PoolExhausted`/`DoubleRelease` errors)
   rather than requiring it to be threaded through and worked around in every future phase — see
   §6 and §9 for the mechanics of what replaced it (`client_workers.rs`, directory slots).
2. **TTL anchor.** `decommission_at = ready_at + requested_ttl`, where `ready_at` is set the
   moment the spawn task actually persists `Ready` — `GET` merely *reports* this, polling doesn't
   delay it. The alternative (clock starts at first poll) would let an unpolled-but-ready cluster
   consume a quota slot forever, which is a resource leak, not a feature.
3. ~~A crash-window gap in worker-release bookkeeping~~ — **moot as of the per-client-account
   change (2026-07-12).** This item described a two-step, non-transactional "release worker, then
   delete row" sequence in `teardown_task::teardown` that a crash could interleave with. Per-client
   accounts have no release step at all (an account is never returned to a shared pool — it's
   always "the same client's account," full stop), and the replacement directory-slot assignment
   is computed fresh from whichever rows are still persisted (via the same atomic
   `try_insert_if_under_quota` transaction that assigns it), not from separate in-memory
   bookkeeping that could drift from the database. Left here, struck through, rather than
   silently deleted, since it was carried forward once already and a future reader auditing this
   list should be able to see it was resolved by a structural change, not forgotten.
4. **Orphan-container detection.** `teardown_task` proceeds to delete the row even if the
   backend's own teardown call failed (logged, not fatal) — the alternative (never deleting the
   row on a failed backend teardown) risks a row stuck in `Deleting` forever, permanently
   consuming a quota slot. This means a container that fails to tear down cleanly becomes
   invisible to reconciliation (which only checks "does this *row's* container look right," not
   "are there containers with no matching row"). No reverse-direction orphan sweep exists yet.
5. **Per-client account capacity planning** is now just "one Unix account per real client,
   provisioned when that client is onboarded" — no separate pool-sizing question remains (unlike
   the eliminated shared pool, whose size needed advance capacity planning independent of which
   clients existed). `max_clusters_per_user` (how many directory slots a single client's account
   needs) is still a first-guess value from the original design writeup — revisit with real usage
   data (§7g).

## 11. §7(a)+§7(c) combined: Supabase spawning + untrusted-tar validation + Kata edge functions

In progress. §7(a) and §7(c) were originally separate, differently-prioritized deferred items;
this work combines them deliberately, on the reasoning that building trusted-Supabase-in-Docker
first and retrofitting Kata later risked rework, and that Kata's host/guest infrastructure should
be proven early — the same discipline that drove proving QEMU/KVM early (§8) before building VM
tooling on top of it. Full scope/architecture decisions are tracked in the session's working plan,
not duplicated here; this section records what's been *verified*, milestone by milestone, as it
happens — matching this document's established practice of recording real findings as they land
rather than only once a phase is fully complete.

### M0 — Kata/nested-KVM feasibility probe: passed, 2026-07-13

**A key clarification worth recording plainly: Kata Containers is not "Docker running inside a
VM."** It's a drop-in replacement for `runc` — registered as an alternate OCI/containerd runtime
— that boots a minimal micro-VM per container and runs the container's process directly inside
it, with no nested container runtime in the guest at all. From the `ContainerRuntime` port's
perspective, spawning a container under Kata should look identical to spawning one under `runc`;
only the isolation mechanism underneath differs.

This was proven for real, on a real KVM host, inside App Salmon's own persistent e2e VM (§8c) —
not assumed. The chain of dependencies: this session's sandbox was granted real `/dev/kvm` access;
nested virtualization was already enabled at the host kernel module level; the e2e VM's existing
`-cpu host` flag (chosen for unrelated reasons in §8c) passes those extensions through to the
guest without any change to the outer QEMU invocation. Confirmed directly: `svm` visible in the
guest's `/proc/cpuinfo`, `/dev/kvm` present inside the guest.

**Two real bugs were found and fixed while installing Kata 3.32.0 inside the guest, both now
captured as an idempotent section in `scripts/vm/guest-provision.sh`, re-verified from a
completely fresh VM (not just idempotency-checked on an already-provisioned one):**

1. **Kata's own installer script (`kata-manager.sh`) is broken against current releases.** It
   verifies `/opt/kata/bin` exists after extraction, but the 3.32.0 release tarball (and 3.30.0,
   tested too — not version-specific) ships the now-Rust-only shim under `runtime-rs/bin/`
   instead, not `bin/`. The installer's completion check predates that layout change and fails
   every time, even though the actual extracted content (including `/opt/kata/bin/kata-runtime`,
   confirmed present) is complete and correct. Worked around by downloading and extracting the
   official static release tarball directly (`kata-static-<version>-amd64.tar.zst` from the
   project's GitHub releases), bypassing the installer script's broken check entirely, then
   symlinking `containerd-shim-kata-v2` and `kata-runtime` onto `PATH`.
2. **Registering the runtime with Docker needs `daemon.json`'s `runtimeType` key, not `path`.**
   Docker's own runtime-name validation rejects any name not present in its `/etc/docker/
   daemon.json` `"runtimes"` map (containerd itself would resolve any name to a
   `containerd-shim-<name>-v2` binary on `PATH` without this, but Docker's own layer in front of
   that doesn't). The `"path"` key is Docker's *legacy* mechanism, for a raw `runc`-CLI-compatible
   binary — pointing it at a shim-v2 binary produces a real, misleading failure
   (`flag provided but not defined: -root`), since shim-v2 speaks a completely different
   (containerd task-management GRPC) protocol than the `create`/`start`/`--bundle`/`--root`
   CLI convention Docker's legacy path assumes. `"runtimeType": "io.containerd.kata.v2"` instead
   tells Docker to hand the name straight to containerd's native runtime-v2 shim resolution,
   which is what `containerd-shim-kata-v2` actually implements. Confirmed both ways empirically —
   `"path"` reproduces the `-root` failure every time; `"runtimeType"` works. (A third path was
   tried and found unnecessary: manually registering the runtime in containerd's own CRI-plugin
   config, `/etc/containerd/config.toml` — that config surface is what Kubernetes' kubelet
   consults via CRI, not what `dockerd` consults for its own direct container-creation API. Kata
   worked identically with that file removed; `guest-provision.sh` does not touch it.)

**Verification, not just "the command didn't error":** a container run with `docker run --runtime
kata ...` was confirmed to actually execute inside a separate kernel — `uname -r` inside the
container reported `6.18.35` (Kata's own bundled guest kernel) while the outer e2e VM's own
kernel is `6.8.0-124-generic` — and a real `qemu-system-x86_64` process (`-machine
q35,accel=kvm`) was observed running for the container's lifetime. `sudo kata-runtime check`
(the project's own capability probe) also reports both "capable of" and "can currently create"
Kata Containers. This is the strongest evidence available short of a full workload trace that
containers really are VM-isolated under this configuration, not silently falling back to a
shared-kernel runtime.

**Guest-provisioning changes**: `scripts/vm/guest-provision.sh` gained an idempotent `kata
containers` section (checks for `/opt/kata/bin/kata-runtime` + Docker reporting `kata` as a
registered runtime before doing anything) implementing exactly the sequence above, pinned to Kata
`3.32.0` deliberately (not "latest" — bump by re-verifying, not by drifting, matching this
project's established pattern for the base cloud image and other fixed dependencies).

### M1 — `ContainerRuntime`/`ContainerSpec` extension: done

`ContainerSpec` gained a closed `OciRuntime { Runc, Kata }` enum field (`pub runtime`,
non-optional — every call site states its choice explicitly) instead of the `Option<String>` an
earlier draft used, plus an `Option<NetworkAttachment { network_name, alias }>` field for
Kong-reaches-PostgREST/GoTrue/Postgres-by-name networking. `docker_bollard.rs` maps
`OciRuntime::Runc` to Docker's default (`HostConfig.runtime` omitted) and `OciRuntime::Kata` to
`HostConfig.runtime = Some(<configured kata runtime name>)` — the *choice* of runtime is now a
compile-time-checked enum; the *installed name* the guest's `daemon.json` registers it under
remains a runtime string, since that's a genuine operational detail (see the M0 section above for
what that string looks like in practice: `"kata"`, registered via `runtimeType`). New
`create_network`/`remove_network` port methods (idempotent, matching `stop_and_remove`'s pattern),
backed by `bollard::Docker::create_network`/`remove_network`. Fake-tested only, against the
existing fake Docker Engine API test server extended with network create/remove handlers;
`PostgresBackend` passes `runtime: OciRuntime::Runc, network: None` and is otherwise unaffected.

### M2 — `ConnectionInfo` → enum, `ServiceKind::Supabase` variant: done

`ServiceKind` gained `Supabase`; `ConnectionInfo` became a closed enum
(`Postgres(PostgresConnectionInfo)` / `Supabase(SupabaseConnectionInfo)`) rather than one flat
struct, matching `ClusterState`'s established closed-enum pattern — `SupabaseConnectionInfo`
carries `api_url` (Kong's published address), a nested `PostgresConnectionInfo`, and
`anon_key`/`service_role_key`/`jwt_secret` (all `Redacted<String>`). Ripples through
`sqlite_repository.rs`'s `PersistedConnection` (kept as a hand-written `From` impl, not derived —
it's the deliberate point where secrets get unwrapped for persistence) and `http/handlers.rs`'s
`ConnectionResponse` (a `#[serde(tag = "kind")]` enum, same pattern as `ClusterInfoResponse`). No
backend is registered for `ServiceKind::Supabase` yet — a request for it fails the same way an
unregistered kind already did (`ClusterError::BackendSpawnFailed`, surfaced as a `Failed` cluster
state), which is exactly the state M3 below builds on.

### M3 — multipart tar upload + tar-validation module: done

Two independent pieces, deliberately not yet wired together — see the scope note below.

**`src/domain/tar_validation.rs`**: validates and extracts an untrusted tar archive using the
`tar` crate's own documented-safe primitive, `Entry::unpack_in`, rather than a hand-rolled path-
joining reimplementation — confirmed by reading `unpack_in`'s actual source (not just its docs)
that it correctly strips a leading root component and rejects `..` outright. Pinned to
`tar = "0.4.46"` specifically: RUSTSEC-2026-0067, a real symlink-following `chmod` bug in
`unpack_in` itself, was fixed in 0.4.45. Layers checks on top of what `unpack_in` doesn't cover by
its own docs: entry type restricted to `Regular`/`Directory` (symlinks, hardlinks, device/fifo
files rejected outright, never target-validated), plus per-entry and cumulative size caps checked
from the header *before* extraction (bounding decompression-bomb-style abuse, which `unpack_in`
doesn't bound either). Fully unit-tested with in-memory tars built via the crate's own `Builder`,
including hand-crafted-header test fixtures that bypass `Header::set_path`'s own safety validation
(needed because a real attacker's tar wouldn't be built through that safe API either).

**HTTP**: `POST /clusters` now branches on `Content-Type`. `application/json` is unchanged and
only accepts `ServiceKind::Postgres`. `multipart/form-data` (`axum::extract::Multipart`) accepts
two parts — `metadata` (JSON, same shape as the JSON body) and `project_tar` (raw bytes) — and
only accepts `ServiceKind::Supabase`; either kind sent the wrong way is rejected with `400`. A new
`[limits].max_tar_bytes` config value sizes a `DefaultBodyLimit` layer scoped to the `/clusters`
route only (every other route keeps axum's built-in 2MB default).

**Deliberate scope boundary, decided explicitly rather than drifted into:** M3 does *not* call
`tar_validation` from the HTTP layer, and does not thread the uploaded tar bytes anywhere past the
handler — they're read (so the size cap is actually enforced) and dropped once the handler
returns. The reason: the tar's *destination* is the cluster's worker/slot directory, which isn't
assigned until `cluster_service.create()` returns, and isn't created on disk until the background
spawn task's privileged `PrepareWorkerDir` step runs — neither exists yet at the point `POST
/clusters` is handling the multipart request. Extracting into a throwaway temp directory just to
validate structure at upload time, only to discard it and re-extract for real later, would mean
writing code in M3 that gets deleted in M4 once the real extraction point (`SupabaseBackend::spawn`,
which *does* know the destination) exists — exactly the kind of unexercised, soon-superseded path
this project avoids. So: byte cap at the edge now (M3); structural tar validation happens exactly
once, in M4, at the point that actually has a destination to extract into.

### M4a — privileged worker-owned tar adoption: done

Before writing `SupabaseBackend` itself, a real design gap surfaced: `tar_validation::validate_and_extract`
(M3) runs as the `app_salmon` process, but its destination — the cluster's worker/slot directory —
is created via `PrepareWorkerDir` and is worker-owned, not `app_salmon`-owned. Extracting directly
into it would fail with `EACCES` the moment a real worker account is involved; M3/M4's fake-backed
tests wouldn't have caught this at all, since they run as a single uid. Caught by reasoning through
the ownership chain before writing `SupabaseBackend`, not by hitting it in M6 — matching this
project's established preference for finding this class of bug early rather than at the end.

Also surfaced: Postgres's own data directory and an uploaded project tree cannot share one
directory (`initdb` refuses a non-empty `PGDATA`), so a backend that wants an uploaded tree needs
its own worker-owned subdirectory distinct from wherever else it stores state.

**Resolution, implemented kind-agnostically in `service::spawn_task`, not as Supabase-specific
logic:**

- `ClusterBackend` gained `worker_subdirs()` — which worker-owned subdirectories, relative to the
  slot directory, this backend needs prepared before `spawn()` is called. Defaults to empty
  (`PostgresBackend` needs no override: it bind-mounts the slot directory directly, unchanged).
  `do_spawn` issues one privileged `mkdir` (`PrepareWorkerDir`) per declared entry instead of
  always issuing exactly one for the bare slot directory — the *number* of privileged calls is
  now data-driven per backend, without `do_spawn` ever branching on `ServiceKind`.
- A new `PrivilegedCommand::AdoptStagedTree { staging_path, dest_path }`, mapped to `cp -r
  <staging_path>/. <dest_path>` (via `sudo -u <worker>`, same `SudoExecutor` argv-based mechanism
  as every other privileged command — never a shell). Deliberately a *copy*, not a rename/move:
  `staging_path` is `app_salmon`-owned; running the copy as the worker means every byte written to
  `dest_path` is a fresh write under the worker's own uid, which is what makes the result
  worker-owned with no separate `chown` step, and sidesteps `mv`'s cross-filesystem `EXDEV` failure
  mode (and its preserving the *original* uid, which `cp` doesn't).
- `do_spawn` now: resolves the worker → prepares every declared worker-owned subdirectory → if a
  `project_tar` was uploaded, extracts it (in Rust, via M3's hardened `validate_and_extract`) into
  a fresh `app_salmon`-owned staging directory, then adopts it into the conventional `project`
  subdirectory via the privileged copy above, then removes the staging directory (best-effort,
  logged not fatal on cleanup failure) → *then* calls the backend's `spawn()`. Extraction happens
  before any container is created, so a malformed upload is rejected without spinning up
  Postgres/PostgREST/GoTrue/Kong first.
- The raw tar bytes flow `create_cluster` → `launch_spawn` → `spawn_task::run` → `do_spawn`
  entirely in-memory, never touching the persisted `Cluster` row or `ServiceSpec` — consistent
  with this project's existing crash model (a spawn that dies mid-flight already isn't resumed
  from the original request; it's reconciled/torn down and the caller re-submits).

Fake-tested only (`RecordingExecutor` asserts exactly which privileged commands `do_spawn` issues
and with what paths; a real, minimal tar built via `tar::Builder` exercises the success path end
to end including staging-directory cleanup; a malformed-bytes case confirms extraction failure
short-circuits before any `AdoptStagedTree` call). Real worker-uid ownership correctness — does a
container actually get to write files a real worker account produced via this path — is an M6
concern against the real VM, not something a fake single-uid test process can prove.

### M4b — `SupabaseBackend`: done, fake-tested only

New `src/backends/supabase.rs`. Structural design, settled and unit-tested:

- Five containers (`db`, `rest`/`PostgREST`, `auth`/`GoTrue`, `kong`, `functions`), created in a
  fixed sequence with a shared network (`app-salmon-net-<cluster_id>`) so they reach each other by
  alias (`db`, `rest`, `auth`, `functions`). Adding a future service (Storage, Realtime,
  `postgres-meta`) means adding one more container to that sequence, not restructuring it — the
  extensibility goal from this section's original scope decision.
- A shared `backends::health_wait::wait_until_healthy` helper, factored out of
  `PostgresBackend`'s original `wait_until_ready` (which now delegates to it, behavior-preserving —
  every existing Postgres test still passes unchanged). Widened slightly from the original to also
  accept `HealthState::None` (Docker's own confirmation that a container has no `HEALTHCHECK`
  configured at all) as "ready," not just `HealthState::Healthy` — needed since `rest`/`auth`/
  `functions` don't get an explicit `HealthCheck` on their `ContainerSpec` (see placeholders below).
- **Deliberate simplifier, decided explicitly**: none of the five containers bind-mount durable,
  worker-owned storage for their own state — including `db`. These are ephemeral, TTL'd clusters;
  losing in-container state on a Docker daemon restart is an accepted tradeoff, and it means only
  one worker-owned subdirectory (`project`, the caller's uploaded tree) is needed, not two,
  avoiding the `PGDATA`-must-be-empty collision a shared `db`+`project` directory would otherwise
  hit. `ClusterBackend::worker_subdirs()` returns `&["project"]` for this backend, `&[]` (default)
  for `PostgresBackend` — unchanged.
- **Kong configuration**: DB-less mode (`KONG_DATABASE=off`, `KONG_DECLARATIVE_CONFIG`), confirmed
  against Kong's own current docs (not assumed) rather than the Admin API — `SupabaseBackend`
  writes a generated `_format_version: "3.0"` YAML file itself (plain, unprivileged
  `tokio::fs::write`; it's `app_salmon`'s own generated config, not the caller's data, so it needs
  no worker-ownership dance) and bind-mounts it read-only into Kong. Routes `/rest/v1`, `/auth/v1`,
  `/functions/v1` to their respective containers by network alias.
- JWT signing via the `jsonwebtoken` crate (HS256), reusing `SecretGenerator::db_password` for the
  signing secret rather than adding a dedicated method — a random alphanumeric string is equally
  suitable for either purpose.
- Sequential spawn order, not the originally-sketched concurrent dependency-tiered fan-out —
  simpler to get right and test; revisit only if real spawn latency demands it.

**Explicit placeholders, not yet verified against real images (M6's job, not M4b's)**: `PostgREST`/
`GoTrue`/edge-runtime container ports, their exact environment variable names, and the edge-runtime
image's own mount-path convention for `functions/`. Written as reasonable, documented guesses —
the same starting point Kata's guest-provisioning steps had before M0 corrected them against a real
VM. Nothing in M4b's own test suite depends on these being exactly right; M6 is where they get
fixed against reality.

#### Post-M4b review: three real bugs found and fixed before M5

A design review against the actual `adapters::docker_bollard` mapping code (not just against
M4b's own fakes, which had quietly encoded the assumptions in question) found three real problems
— the kind fakes structurally can't catch, since the fake *is* the assumption under test:

1. **Four of the five containers would never have reported ready.** `wait_until_healthy` treated
   `Some(HealthState::None)` as "no healthcheck configured, ready" — but the real adapter
   (`container_status_from_response`) produces a bare `None` (the `Option` itself, not
   `Some(HealthState::None)`) when a container's `.State.Health` is absent entirely, which is
   exactly what happens for `rest`/`auth`/`kong`/`functions` (none of which set `health_check` on
   their `ContainerSpec`). `wait_until_healthy` never matched that shape, so those four containers
   would have polled until `HealthCheckTimeout`, every time. **Fixed** by having
   `wait_until_healthy` take an explicit `requires_healthcheck: bool` instead of trying to infer
   intent from the `health` value — the caller already knows, from the `ContainerSpec` it just
   built, whether it asked for a `HEALTHCHECK`; `SupabaseBackend` passes
   `spec.health_check.is_some()` per container.
2. **Every container was host-published, not just Kong.** `ContainerSpec.host_port: Option<u16>`
   conflated "no specific port requested" with "don't publish" — `container_create_body` inserts a
   `PortBinding` unconditionally, so `host_port: None` always meant *ephemeral-but-still-published*,
   never *unpublished*. `rest`/`auth`/`functions` were each getting a `127.0.0.1` port directly
   reachable from the host, bypassing Kong entirely — for `functions` specifically, a hole punched
   around the one container where the Kata isolation boundary is supposed to matter most. **Fixed**
   by replacing `host_port: Option<u16>` with a closed `PortPublish { Unpublished, Ephemeral }` enum
   (matching this project's established closed-enum-over-ambiguous-`Option`/bool preference — the
   same reasoning `OciRuntime` was introduced for). Only `db` and `kong` now publish.
3. **The `functions` bind-mount source could still end up root-owned.** If a caller's tar omitted
   `functions/` entirely, nothing would have created `<slot>/project/functions` before Docker's own
   bind-mount machinery did — as root, the exact worker-ownership trap `worker_subdirs` exists to
   avoid (see M4a above). **Fixed**: `SupabaseBackend::worker_subdirs()` now declares
   `["project/functions"]` rather than `["project"]`, so `service::spawn_task`'s privileged `mkdir
   -p` creates it worker-owned unconditionally; `AdoptStagedTree`'s `cp -r` merges the tar's actual
   contents into it without complaint whether or not `functions/` was present.

All three are fake-tested (a new `PortPublish::Unpublished` case in `docker_bollard`'s own test
suite; `health_wait`'s widened test coverage) but, like the rest of M4b, not proof the real
containers behave as intended — that's still M6.

### M5 — wired into the registry: done

New `[supabase]` config table (`postgrest_image`, `gotrue_image`, `kong_image`,
`edge_runtime_image`, `kata_runtime_name`) — required alongside `[docker]`, matching this
project's no-silent-partial-config convention (a missing `[supabase]` section fails to parse at
startup, the same as a missing `[docker]` section always has). `main.rs` now constructs
`SupabaseBackend` from it and registers it in the same `backends` map `PostgresBackend` was
already in — `ServiceKind::Supabase` requests route through the real backend now, not the
"no backend registered" `Failed` path M2/M3/M4 deliberately left it in.

`kata_runtime_name` closes the cross-artifact placeholder from M0/M4a: `main.rs` no longer hard-codes
`"kata"` when connecting `BollardContainerRuntime` — it comes from config, which
`scripts/vm/guest-provision.sh`'s own registration must continue to match (same invariant class
`docs/DESIGN.md` §8a already documents elsewhere). `db`'s image is deliberately *not* duplicated
into `[supabase]` — `SupabaseBackend` reuses `[docker].postgres_image`, the same image
`PostgresBackend` runs, per the M4b design note that Supabase's `db` isn't a separate image.

### Not yet built

M6: real e2e verification of the whole stack against the actual VM — Postgres+pgvector alone under
the new backend path first, then +PostgREST+GoTrue (inter-container DNS-by-alias, new territory),
then +Kong (ingress), then the edge-function container under real Kata with a genuinely
tar-supplied function actually executing. This is where every M4b placeholder (container ports,
env var names, the edge-runtime mount-path convention) and the M4b-review fixes (health-wait
semantics, `PortPublish`, `functions` worker-ownership) get checked against reality, not assumed.
