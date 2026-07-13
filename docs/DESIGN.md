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
image, and discards the VM's disk when the run finishes. The invoking host only ever needs QEMU +
`/dev/kvm` access — never runs `useradd`, never writes to its own `/etc/sudoers.d`, and never
needs a Docker daemon of its own.

**Known, unfixed bug, found via §8c — do not treat this path as verified until it's fixed here
too.** `run-e2e-in-vm.sh`/`guest-init.sh` use the same `-virtfs local,...,security_model=none` 9p
share and the same `su - ubuntu -c "cd $REPO && ..."` pattern that §8c's first real KVM run
found broken: `security_model=none` passes the host's raw uid/gid/mode through to the guest, so a
non-root guest user without a numerically matching uid gets `Permission denied` on the shared
`/repo` — root bypasses the check (which is why the `setup-e2e-env.sh` step, run via `sudo`,
would still work), but the actual `cargo test --test e2e` step, run as plain `ubuntu`, would not.
This has not yet been fixed here the way §8c was (copying the repo in over a channel instead of a
live share) because this path has no SSH/synchronous channel to reuse — it's a fire-and-forget
cloud-init script with no interactive session, so the same fix would need a different mechanism.
Prefer `just e2e-vm-up`/`e2e-vm-test` (§8c) until this is addressed.

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

## 8c. Persistent VM for iterative testing — `just e2e-vm-up` / `e2e-vm-test` / `e2e-vm-down`

`test-e2e-vm` (§8b) boots, provisions, and discards a VM on every single call — correct for a
one-shot run, expensive if you're iterating on e2e-suite-relevant code and want to run it
repeatedly in one sitting. `just e2e-vm-up` boots a VM once and leaves it running; `just
e2e-vm-test` (which `just ci` also uses automatically when such a VM is up — see below) runs the
suite against it over SSH in a few seconds instead of minutes; `just e2e-vm-down` tears it down
and wipes its disk. Same host prerequisites as §8b (`just setup-e2e-vm`), same "only sudo used
is the one-time KVM group grant" property.

**How `just ci` finds it:** `ci`'s e2e step runs `scripts/vm/e2e-vm-status.sh` first (silently);
if that reports the persistent VM up, `ci` runs the suite against it (`just e2e-vm-test`) instead
of the bare-host path. `ci` never boots a VM itself — that's a multi-minute cost, too heavy for a
gate you might run many times while iterating — it only *uses* one if you already started it. If
neither the persistent VM nor the bare-host setup is available, `ci` prints all three options
(persistent VM, one-shot VM, bare host) so the omission is never silent.

**Design:**
- `scripts/vm/lib.sh`: helpers shared with `run-e2e-in-vm.sh` (host-prereq checks, base-image
  download+verify, seed-ISO building) plus persistent-VM-specific ones: `vm_find_free_port`
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
  mechanism §8b uses. **The first real KVM run of this tooling (2026-07-13) found that this is
  broken for any host checkout with normal-or-tighter permissions**: `security_model=none`
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

**Security model for the SSH transport (this is new attack surface `test-e2e-vm` doesn't have,
since that path never listens for inbound connections):**
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
- **Boot-verified for real against a real KVM host (2026-07-13):** VM boot, cloud-init's
  `write_files`/`ssh_authorized_keys`/host-key injection, and SSH all confirmed working exactly
  as designed — this was the piece flagged as least certain (untested cloud-init module
  ordering), and it worked on the first real run. That same run also surfaced the 9p-permission
  bug described above, found and fixed the same session — not yet re-verified against real KVM
  since that fix, though the qemu flags and cloud-init schema that *were* verified are otherwise
  unchanged by it.
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

What is still **not** verified: `guest-provision.sh` succeeding inside the real cloud image, and
end-to-end `e2e-vm-up.sh` → `e2e-vm-test` → `e2e-vm-down.sh` against real KVM *since* the
9p-to-tar fix (boot/cloud-init/SSH were verified before that fix; the fix itself hasn't yet had a
real KVM run to confirm `vm_sync_repo` and the rest of the pipeline behave against the real guest
the same way they did against the sandbox stand-in). Next step: re-run the full cycle against a
scratch checkout and confirm the fix actually resolves the originally-reported `Permission denied`.

## 9. Testing & coverage

- `just ci` — the single command: format check, clippy (deny-on-warnings, `--all-targets
  --all-features`), unit tests, and the e2e suite against whichever of the following is
  available, in order: a persistent e2e VM already up (§8c), the bare-host setup (checked via
  `docker info` + `id e2e-agent`), or — if neither — a clear message listing all three ways to
  get one running, never a silent skip.
- `just test-unit` / `just test-e2e` / `just test-all` — independently runnable, per the
  requirement that unit and e2e stay separable.
- `just setup-e2e-vm` / `just test-e2e-vm` (recommended over `setup-e2e`/`test-e2e` for a single
  run) — runs the e2e suite inside a disposable QEMU VM instead of on this machine, so
  `setup-e2e-env.sh`'s host-level changes never touch the machine actually running the tooling;
  this machine only needs QEMU + `/dev/kvm` access, which `setup-e2e-vm` gets for you (sudo used
  once, for two generic non-App-Salmon-specific things). See §8b for the design and its
  verification status.
- `just e2e-vm-up` / `e2e-vm-test` / `e2e-vm-down` (recommended over `test-e2e-vm` if you're
  running the suite more than once in a sitting) — same disposable-VM idea, but the VM stays up
  across multiple test runs instead of being discarded every call, and `just ci` detects and uses
  it automatically. See §8c for the design, the SSH transport's security model, and its
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
