# App-salmon
A service that spawns apps (thus the salmon who are famous for spawning). Intended for test infrastructure. Initially limited to Supabase, an OpenRouter proxy, and plain Postgres (with pgvector extension) with optional observability.

## Status

Phase 1 is implemented: a REST API (bearer-token auth, plain TCP on `127.0.0.1`) that provisions
ephemeral Postgres+pgvector clusters via Docker, with sudo-based privilege separation per
cluster, a durable SQLite-backed lifecycle, a TTL reaper, and startup reconciliation. Supabase,
the OpenRouter proxy, edge-function sandboxing, and TLS are deferred — see
[`docs/DESIGN.md`](docs/DESIGN.md) for the full design, security model, and roadmap.

## Quick start

```sh
just ci          # fmt-check, clippy, unit tests, and e2e if this machine is set up for it
just test-unit   # unit tests only — no Docker/sudo/root required
just setup-e2e   # one-time, needs root: provisions worker accounts + sudoers rule + pulls the image
just test-e2e    # e2e suite against real Docker/sudo/Postgres
just run         # cargo run -- --config config.toml
```

See `just --list` for everything else, and `docs/DESIGN.md` for the config file shape.

## Testing

**`just ci` is the one command that runs everything** — format check, clippy (deny-on-warnings),
unit tests, and (if this machine has Docker reachable and the e2e worker accounts provisioned)
the full e2e suite too. It never silently skips e2e: if the prerequisites aren't detected, it
prints exactly what to run to set them up, rather than quietly passing without having run them.
This is what to run before committing, and what CI runs.

Requires `cargo`/`rustc` (stable) and [`just`](https://github.com/casey/just) on `PATH`.

| Command | What it does | Needs |
|---|---|---|
| `just ci` | Everything below, in order — the full gate | Docker+root optional (e2e step self-skips with instructions if absent) |
| `just test-unit` | `cargo test --lib` — all unit tests | Nothing special |
| `just setup-e2e` | Provisions worker accounts, a scoped `sudoers.d` rule, and pulls the Postgres image | Root, one-time per machine |
| `just test-e2e` | `cargo test --test e2e` against real Docker/sudo/Postgres | `just setup-e2e` having been run |
| `just test-all` | `test-unit` + `test-e2e` back to back | Same as `test-e2e` |
| `just coverage` | `cargo llvm-cov --lib` summary | `cargo install cargo-llvm-cov` + `rustup component add llvm-tools-preview` once |
| `just coverage-html` | Same, as a browsable HTML report at `target/llvm-cov/html/index.html` | Same as `coverage` |
| `just fmt` / `just fmt-check` | Apply / check formatting | Nothing special |
| `just lint` | `cargo clippy --all-targets --all-features -- -D warnings` | Nothing special |

**Why the e2e split matters:** unit tests (`just test-unit`) run against fakes for every external
system (a fake Docker Engine API server, a fake `sudo` script, an in-memory or real-file SQLite
DB) and need no privileges at all — anyone can run them, including in CI or a sandboxed session.
The e2e suite (`just test-e2e`) is the only thing that verifies the real adapters against a real
Docker daemon, real `sudo`, and a real Postgres container, and it genuinely needs root (to
provision worker accounts and the sudoers rule) plus a running Docker daemon — it cannot run in
every environment. Anyone with an environment that has both should run `just setup-e2e` once and
then `just test-e2e` (or just `just ci`) themselves, rather than relying on e2e-only code paths
being exercised for the first time only after a commit and push.
