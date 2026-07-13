# Single-command entry points for App Salmon. `just ci` is what CI (and you, before committing)
# should run; the pieces are also independently runnable.

# Run everything CI checks: formatting, lint, unit coverage, and e2e if this machine is set up
# for it (never silently skipped — see `test-e2e`). Prefers, in order: a persistent e2e VM
# already up (fast — no boot/provision cost per run), then the bare-host setup, then a reminder
# covering every option if neither is available. Deliberately does NOT boot an ephemeral VM
# itself (`test-e2e-vm`) — that's a multi-minute image-download-and-boot cost, too heavy for a
# gate you might run many times while iterating; that path stays opt-in.
ci: fmt-check lint test-unit
    #!/usr/bin/env bash
    set -euo pipefail
    if ./scripts/vm/e2e-vm-status.sh >/dev/null 2>&1; then
        just e2e-vm-test
    elif command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1 && id e2e-agent >/dev/null 2>&1; then
        just test-e2e
    else
        echo "e2e prerequisites not detected (no persistent e2e VM up, and no bare-host docker +"
        echo "e2e-agent account). Options, in order of preference:"
        echo "  just e2e-vm-up && just ci                         (persistent VM, reused across runs)"
        echo "  just setup-e2e-vm && just test-e2e-vm             (one-shot disposable VM)"
        echo "  sudo ./scripts/setup-e2e-env.sh && just test-e2e  (root, persists e2e system accounts)"
    fi

# Format the whole workspace.
fmt:
    cargo fmt

# Check formatting without modifying anything (what CI runs).
fmt-check:
    cargo fmt --check

# Clippy, deny-on-warnings, across lib + bin + all test targets.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Unit tests only (`src/`) — no Docker/sudo/root required.
test-unit:
    cargo test --lib

# End-to-end suite (`tests/e2e`) — requires `sudo ./scripts/setup-e2e-env.sh` to have been run on
# this machine first. Fails loudly (not silently) if prerequisites are missing; single-threaded
# since most tests share one client account's max_clusters_per_user quota, and parallel test
# functions creating clusters against the same account would spuriously race each other's quota.
test-e2e:
    cargo test --test e2e -- --test-threads=1

# Both test suites.
test-all: test-unit test-e2e

# Unit-test coverage (excludes `tests/e2e`, which cargo-llvm-cov doesn't instrument the same way
# and which needs real infra anyway). Requires `cargo install cargo-llvm-cov` +
# `rustup component add llvm-tools-preview` once per machine.
coverage:
    cargo llvm-cov --lib --summary-only

# Coverage as an HTML report you can open in a browser.
coverage-html:
    cargo llvm-cov --lib --html
    @echo "open target/llvm-cov/html/index.html"

# Run the server locally against a config file (defaults to ./config.toml).
run config="config.toml":
    cargo run -- --config {{ config }}

# One-time setup for running the e2e suite directly on this machine (worker accounts, sudoers
# rule, postgres image). Needs root, and persists App-Salmon-specific system accounts and a
# sudoers rule on this machine for as long as you keep them. Prefer `just setup-e2e-vm` +
# `just test-e2e-vm` unless you specifically want the suite to run against this host directly.
setup-e2e:
    sudo ./scripts/setup-e2e-env.sh

# One-time setup for running the e2e suite inside a disposable VM instead (see test-e2e-vm below):
# installs QEMU if missing and adds you to the `kvm` group if needed. Needs sudo only for those
# two ordinary, generic, one-time things — nothing App-Salmon-specific, nothing that persists
# beyond "this machine can run QEMU with KVM acceleration", which most dev machines want anyway.
# Log out and back in after running this if it added you to the kvm group.
setup-e2e-vm:
    ./scripts/vm/setup-vm-host.sh

# Run the e2e suite inside an ephemeral, disposable QEMU VM instead of on this machine directly
# — so setup-e2e's useradd/sudoers.d writes and the e2e suite's Docker usage land on a throwaway
# guest, not here. Needs `just setup-e2e-vm` to have been run once; does NOT need root, Docker,
# or scripts/setup-e2e-env.sh to have been run on this machine. Boots, provisions, tests, and
# discards the VM every single call — prefer `just e2e-vm-up` + `just e2e-vm-test` if you're
# going to run the suite more than once in a session, which skips the repeated boot/provision
# cost this pays every time.
test-e2e-vm *args:
    ./scripts/vm/run-e2e-in-vm.sh {{ args }}

# Boot a persistent e2e VM that stays up across multiple test runs — unlike test-e2e-vm, which
# boots, tests, and discards on every call. Idempotent (safe to run if already up). Needs
# `just setup-e2e-vm` to have been run once. Run this once per session, then `just e2e-vm-test`
# (or just `just ci`) as many times as you like, then `just e2e-vm-down` when done.
e2e-vm-up:
    ./scripts/vm/e2e-vm-up.sh

# Run the e2e suite against the persistent VM from `just e2e-vm-up` (must already be up).
e2e-vm-test:
    ./scripts/vm/e2e-vm-run-tests.sh

# Whether the persistent e2e VM is up (and provisioned/ready for e2e-vm-test).
e2e-vm-status:
    ./scripts/vm/e2e-vm-status.sh

# Tear down the persistent e2e VM and wipe its disk. Run this when you're done for the session.
e2e-vm-down:
    ./scripts/vm/e2e-vm-down.sh
