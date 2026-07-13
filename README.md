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
just ci             # fmt-check, clippy, unit tests, and e2e if this machine is set up for it
just test-unit      # unit tests only — no Docker/sudo/root required

# e2e suite, recommended path — runs in a disposable VM, no root/Docker on this host:
just setup-e2e-vm   # one-time: installs QEMU + adds you to the kvm group if needed (needs sudo
                     # only for those two ordinary, generic, one-time things — see below)
just e2e-vm-up       # boot a VM once and leave it running
just e2e-vm-test     # run the suite against it — seconds, not minutes (just ci uses this too,
                     # automatically, once the VM is up); run this as many times as you like
just e2e-vm-down     # tear it down and wipe its disk when you're done

just run            # cargo run -- --config config.toml
```

See `just --list` for everything else, and `docs/DESIGN.md` for the config file shape.

## Testing

**`just ci` is the one command that runs everything** — format check, clippy (deny-on-warnings),
unit tests, and the e2e suite too, against whichever of the paths below is available (preferring
a persistent e2e VM already up, per below). It never silently skips e2e: if nothing's set up, it
prints exactly what to run, rather than quietly passing without having run it. This is what to
run before committing, and what CI runs.

Requires `cargo`/`rustc` (stable) and [`just`](https://github.com/casey/just) on `PATH`.

| Command | What it does | Needs |
|---|---|---|
| `just ci` | Everything below, in order — the full gate | e2e step self-skips with instructions if no e2e path is set up |
| `just test-unit` | `cargo test --lib` — all unit tests | Nothing special |
| `just setup-e2e-vm` | Installs QEMU + adds you to the `kvm` group if needed | sudo, one-time, for two generic non-App-Salmon-specific things |
| `just e2e-vm-up` | Boots a persistent VM and leaves it running | `just setup-e2e-vm` having been run; **no root/Docker on this host** |
| `just e2e-vm-test` | Runs the e2e suite against the VM from `e2e-vm-up` — seconds, not minutes | `just e2e-vm-up` having been run |
| `just e2e-vm-status` | Whether that VM is up and ready | — |
| `just e2e-vm-down` | Tears down that VM and wipes its disk | — |
| `just setup-e2e` | Provisions worker accounts + a scoped `sudoers.d` rule directly on this host | Root, one-time, persists App-Salmon-specific system state |
| `just test-e2e` | `cargo test --test e2e` against Docker/sudo/Postgres running directly on this host | `just setup-e2e` having been run |
| `just test-all` | `test-unit` + `test-e2e` back to back | Same as `test-e2e` |
| `just coverage` | `cargo llvm-cov --lib` summary | `cargo install cargo-llvm-cov` + `rustup component add llvm-tools-preview` once |
| `just coverage-html` | Same, as a browsable HTML report at `target/llvm-cov/html/index.html` | Same as `coverage` |
| `just fmt` / `just fmt-check` | Apply / check formatting | Nothing special |
| `just lint` | `cargo clippy --all-targets --all-features -- -D warnings` | Nothing special |

**Why the e2e split matters:** unit tests (`just test-unit`) run against fakes for every external
system (a fake Docker Engine API server, a fake `sudo` script, an in-memory or real-file SQLite
DB) and need no privileges at all — anyone can run them, including in CI or a sandboxed session.
The e2e suite is the only thing that verifies the real adapters against a real Docker daemon,
real `sudo`, and a real Postgres container, and needs somewhere real to run those — it cannot run
in every environment. **There are two ways to give it that:**
- **`just e2e-vm-up` + `just e2e-vm-test` (recommended):** boots a disposable QEMU VM once,
  provisions it (Docker, Rust, the App-Salmon worker accounts and sudoers rule — all inside the
  VM), and leaves it running so repeated test runs are seconds instead of minutes. `just ci`
  detects and uses it automatically once it's up. `just e2e-vm-down` tears it down and wipes its
  disk when you're done. This host only ever needs `/dev/kvm` access — `just setup-e2e-vm` gets
  you that with sudo used just once, for installing QEMU and joining the standard `kvm` group,
  the same one-time step any KVM user needs on a fresh machine regardless of this project.
  Nothing App-Salmon-specific ever touches this host. See `docs/DESIGN.md` §8c for the design
  and, in particular, the SSH transport's security model (loopback-bound port forwarding,
  pubkey-only auth, a host-generated and pinned SSH host key — not trust-on-first-use) — this
  path has been confirmed working end to end against a real KVM host, including the arbitrary-uid
  Postgres path (§8a).
- **`just test-e2e` (direct):** runs against Docker/sudo/Postgres on this host directly. Needs
  root to provision worker accounts and a sudoers rule (`just setup-e2e`), and those persist on
  this host afterwards. Useful if you don't want or can't get KVM access, or specifically want to
  exercise the real host path.

Whichever path, run it yourself rather than relying on e2e-only code paths being exercised for
the first time only after a commit and push.
