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

This section describes running `app_salmon` (and its e2e suite) directly against a host. §8b
describes an alternative for the e2e suite specifically — `just test-e2e-vm` — that avoids the
root requirement in the first bullet below entirely, by running it inside a disposable VM
instead; that's the recommended way to run the e2e suite unless you specifically want it against
a real host.

- `scripts/setup-e2e-env.sh` (must run as root): creates one Unix account per configured e2e
  client, writes a `/etc/sudoers.d/app-salmon` rule scoped to exactly `mkdir -p <path>` and
  `find <path> -mindepth 1 -delete` against that client's `max_clusters_per_user` literal
  directory-slot paths (no wildcard — see §6 for why), and pulls the configured Postgres image.
- The `app_salmon` config (`config.toml`) needs: a `[[clients]]` entry per client account, each
  with `secret_hash = "sha256:<64 hex chars>"` (the hash of a secret generated and distributed to
  that client account out of band — no CLI tooling for this yet, `sha256sum` a random string by
  hand) and `unix_user = "<name>"` naming the pre-provisioned Unix account that client's clusters
  run as (must be unique across clients — `Config::validate` rejects two clients sharing one).
- The service needs read access to `/etc/passwd` (to resolve worker uid/gid), a writable
  `storage.sqlite_path` directory, a writable `logging.log_dir`, and access to the Docker socket
  (`docker.socket_path`).

## 8a. Known risk: the real Postgres/arbitrary-uid path has never actually run

Flagging this prominently rather than leaving it as one line among the open questions in §10,
because it's judged the single most likely thing to break the happy path in a follow-up session:
`backends::postgres` runs the stock `postgres`/`pgvector` image with `--user <worker-uid>:<worker-gid>`
against a bind-mounted, worker-owned `PGDATA` directory (see §6). This combination — an arbitrary
non-root uid with no matching `/etc/passwd` entry *inside* the container, writing to a bind mount
whose ownership was set by a host-side `chown` — is a historically fragile spot for the official
Postgres image: its entrypoint script does its own uid/gid and permission probing on startup, and
versions have differed in how gracefully they handle an unmapped uid. This has been implemented
faithfully per the design but has **never been exercised against a real Docker daemon** in any
session so far (this sandboxed environment has neither Docker access nor root — see §9's e2e
caveat). The first thing a follow-up session with real Docker access should do is simply run
`just setup-e2e && just test-e2e` and watch `create_cluster::valid_request_eventually_becomes_ready`
either pass or fail with a concrete container log — before trusting anything else about phase 1.

Unrelated to (and not fixed by) the readiness-mechanism change in §9's testing notes below: that
change affects *how App Salmon knows Postgres is ready*, not *whether an arbitrary-uid Postgres
process can start and write to its data directory at all* on a real host. This risk stands as-is.

Also newly relevant here: `wait_until_ready` now depends on the `postgres`/`pgvector` image
actually having `pg_isready` on `PATH` inside the container for the `CMD-SHELL` healthcheck to
run (see §9). The official images ship the full `postgresql-client` toolchain including
`pg_isready`, so this is expected to hold, but — like the arbitrary-uid path above — has not been
confirmed against a real image pull in this environment.

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

## 8b. VM e2e testing — `just test-e2e-vm`

`scripts/setup-e2e-env.sh` (per §8) makes real, host-level changes: it creates system Unix
accounts and writes an `/etc/sudoers.d` rule. Requiring that on every machine that wants to run
the e2e suite — including a disposable CI runner or a developer's own laptop — is a real cost,
and the earlier design left no way to run the e2e suite *without* accepting it. `just
test-e2e-vm` (→ `scripts/vm/run-e2e-in-vm.sh`) closes that gap: it runs the entire e2e suite,
including `setup-e2e-env.sh` itself, inside an ephemeral QEMU VM booted from a stock Ubuntu cloud
image, and discards the VM's disk when the run finishes. **This is now the recommended way to run
the e2e suite.** The invoking host only ever needs QEMU + `/dev/kvm` access — never runs
`useradd`, never writes to its own `/etc/sudoers.d`, and never needs a Docker daemon of its own.

**Getting to "QEMU + `/dev/kvm` access" — `just setup-e2e-vm` (→ `scripts/vm/setup-vm-host.sh`):**
this is the *only* place sudo is needed anywhere in the VM e2e path, and it's a one-time,
App-Salmon-agnostic setup step, not a per-run or per-project privilege: it installs
`qemu-system-x86`/`qemu-utils` and a cloud-init seed tool (`cloud-image-utils`) if missing, and
adds the invoking user to the standard `kvm` group if not already a member (the same group grant
any KVM user needs on a fresh machine, for any purpose — nothing here is specific to this repo).
It does not touch `/etc/sudoers.d`, does not create any App-Salmon-specific accounts, and there's
nothing to uninstall or revert afterwards. If `/dev/kvm` doesn't exist at all, that's a firmware
(VT-x/AMD-V) or, if the host is itself a VM, a nested-virtualization setting the script can
detect but not fix from inside the OS — it says so and exits rather than failing confusingly
later. After it runs (and, if it changed group membership, after logging back in), `just
test-e2e-vm` itself needs no further privilege at all.

**Design:**
- `scripts/vm/run-e2e-in-vm.sh` (host side): downloads the official Ubuntu 24.04 (`noble`) server
  cloud image once, caches it under `~/.cache/app-salmon-e2e-vm/`, and **re-verifies its SHA-256
  against the vendor's published `SHA256SUMS` on every run** (not a checksum hardcoded once in
  this script, which would silently go stale as Ubuntu republishes the `current` image) —
  redownloads once and re-checks if the cached copy ever fails to match. Creates a copy-on-write
  qcow2 overlay backed by that cached image (`qemu-img create -f qcow2 -F qcow2 -b <base>
  overlay.qcow2 20G`) so the cached base image itself is never written to. Builds a `cidata`
  (NoCloud) cloud-init seed ISO via whichever of `cloud-localds` / `genisoimage` / `mkisofs` /
  `xorriso -as genisoimage` is on `PATH`. Boots with `-machine q35,accel=kvm -cpu host`, a
  `virtio-net` user-mode NIC (outbound only, for `apt`/`docker pull`/`rustup`), and a `-virtfs
  local` 9p share exposing the repo checkout read-write at `/repo` inside the guest — the same
  checkout the host is running from, not a copy. Waits on the qemu process under a `timeout`
  (default 1800s), then reads the guest's result back from `<repo>/.e2e-vm-result/` (which is
  simply a subdirectory of the same 9p share, so both sides see it without any extra transport).
- `scripts/vm/guest-init.sh` (guest side, run as root via cloud-init `runcmd`): mounts the 9p
  share at `/repo`, installs `docker.io` and a build toolchain via `apt`, installs Rust via
  `rustup` for the cloud image's default `ubuntu` user, runs `APP_SALMON_USER=ubuntu
  scripts/setup-e2e-env.sh` — so the invasive host changes §8 describes land on *this disposable
  guest*, exactly once, and are thrown away with the VM's disk — then runs `cargo test --test
  e2e -- --test-threads=1` as `ubuntu` (the same command `just test-e2e` runs; called directly
  here rather than through `just` itself, since `just` isn't guaranteed present in every Ubuntu
  release's default repos and this is the one place avoiding that dependency was worth the small
  duplication), and writes its exit code and full test log to `/repo/.e2e-vm-result/`. An
  `EXIT` trap guarantees the guest always powers off and always leaves an `exit_code` file
  behind, however the script exits — including a failure before the test run even starts (a bad
  apt mirror, a rustup network hiccup, the 9p mount itself) — specifically so a pre-test failure
  surfaces as "failed in a couple of minutes with a log to read" on the host side rather than
  "the host blocks for the full `--timeout` with no explanation." The VM's own poweroff is what
  makes the host's `qemu-system-x86_64` process exit and the host script proceed to read the
  result.
- **Why not nested virtualization:** phase-1 e2e only needs plain Docker (runc containers, i.e.
  Linux namespaces + cgroups in the guest kernel) — one level of hardware virtualization
  (`-enable-kvm`/`accel=kvm`) is sufficient and is all this tooling requires or configures.
  Nested virtualization (`kvm_intel`/`kvm_amd`'s `nested=1` module parameter) only becomes
  necessary once the *guest itself* needs to run a second-level hypervisor — that's the
  Kata-Containers phase (§7c), not this one. Building today's tooling to require nested virt now
  would make it fail on hosts that don't have that host-kernel module setting even though nothing
  it currently runs needs it; the same QEMU invocation carries forward unchanged into the Kata
  phase, at which point only a host-side module flag changes, not this script.

**Verification status — read before trusting this beyond "the pieces are individually sound":**
this sandbox has no `/dev/kvm` access (confirmed: this user isn't in the `kvm` group, a direct
open of `/dev/kvm` is denied, and there's no passwordless sudo available to fix either) — so
**no VM has actually been booted in any session so far.** What *was* verified in this sandbox,
directly rather than by inspection:
- Both scripts are `bash -n`-clean.
- The generated cloud-init `user-data`/`meta-data` were parsed with a real YAML parser
  (`python3` + `PyYAML`), including base64-decoding the embedded `guest-init.sh` and confirming
  it round-trips byte-for-byte.
- Every QEMU flag used (`-machine q35,accel=kvm|tcg`, `-cpu`, `-smp`, `-no-reboot`, `-display
  none`, `-serial file:...`, `-drive ...,if=virtio`, `-netdev user` + `virtio-net-pci`, `-virtfs
  local,...,security_model=none`) was checked against this machine's actual `qemu-system-x86_64
  --help` / `-device help` / `-accel help` output, and **the exact command line this script
  builds was run against real (throwaway) disk/ISO files**, with `accel=tcg` substituted for
  `accel=kvm` and `-cpu qemu64` substituted for `-cpu host` (both real-run values require KVM,
  which this sandbox doesn't have) — every other flag was passed through unchanged, and QEMU
  accepted the full command line and started successfully before being killed. `accel=kvm` and
  `-cpu host` themselves were not, and could not be, exercised here.
- `qemu-img create -f qcow2 -F qcow2 -b <base> <overlay> 20G` was run for real against a scratch
  backing file and produces a correctly-sized, correctly-backed overlay (`qemu-img info`
  confirms `backing file`/`backing file format`/`virtual size`).
- The base image URL and its `SHA256SUMS` companion were fetched for real from
  `cloud-images.ubuntu.com` in this sandbox and do resolve/match as this script expects.

What is **not** verified, because it requires either KVM or root this sandbox doesn't have:
whether the guest actually boots this cloud image under `accel=kvm`, whether cloud-init actually
runs `write_files`/`runcmd` as configured, whether the 9p mount actually comes up inside the
guest, whether `apt`/`rustup`/`cargo test --test e2e` actually succeed inside it, and whether
`scripts/setup-e2e-env.sh` behaves the same inside this specific cloud image as it does on a bare
host. `scripts/vm/setup-vm-host.sh` itself is equally unverified end-to-end for the same reason —
its `apt-get install`/`usermod` steps were checked only for correct package names and syntax
(`apt-cache policy qemu-system-x86 qemu-utils cloud-image-utils`, `bash -n`, and the `kvm`-group
membership-detection logic exercised directly against this sandbox's real `/etc/group`), not run
for real, since doing so would modify this sandbox's host. The first session with real `/dev/kvm`
access should run `just setup-e2e-vm && just test-e2e-vm` against a scratch checkout and treat
whatever it finds as a bug report against this section — same posture already established for the
e2e suite itself in §8a.

## 9. Testing & coverage

- `just ci` — the single command: format check, clippy (deny-on-warnings, `--all-targets
  --all-features`), unit tests, and the e2e suite *if* this machine has Docker + the e2e client
  accounts provisioned (checked via `docker info` + `id e2e-agent`; if not, `just ci` prints a
  clear message and does not silently skip — running it *without* those prerequisites is expected
  to leave e2e unrun, not to fail).
- `just test-unit` / `just test-e2e` / `just test-all` — independently runnable, per the
  requirement that unit and e2e stay separable.
- `just setup-e2e-vm` / `just test-e2e-vm` (recommended over `setup-e2e`/`test-e2e`) — runs the
  same e2e suite inside a disposable QEMU VM instead of on this machine, so
  `setup-e2e-env.sh`'s host-level changes never touch the machine actually running the tooling;
  this machine only needs QEMU + `/dev/kvm` access, which `setup-e2e-vm` gets for you (sudo used
  once, for two generic non-App-Salmon-specific things). See §8b for the design and its
  verification status.
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
