# Single-command entry points for App Salmon. `just ci` is what CI (and you, before committing)
# should run; the pieces are also independently runnable.

# Run everything CI checks: formatting, lint, unit coverage, and e2e if a persistent e2e VM is up
# (never silently skipped — prints exactly what to run otherwise). Deliberately does NOT boot a
# VM itself — that's a multi-minute image-download-and-boot cost, too heavy for a gate you might
# run many times while iterating; `just e2e-vm-up` stays a separate, deliberate step.
ci: fmt-check lint test-unit
    #!/usr/bin/env bash
    set -euo pipefail
    if ./scripts/vm/e2e-vm-status.sh >/dev/null 2>&1; then
        just e2e-vm-test
    else
        echo "no persistent e2e VM is up. Run:"
        echo "  just setup-e2e-vm && just e2e-vm-up && just ci"
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

# One-time setup for running the e2e suite inside a disposable VM (see e2e-vm-up below):
# installs QEMU if missing and adds you to the `kvm` group if needed. Needs sudo only for those
# two ordinary, generic, one-time things — nothing App-Salmon-specific, nothing that persists
# beyond "this machine can run QEMU with KVM acceleration", which most dev machines want anyway.
# Log out and back in after running this if it added you to the kvm group.
setup-e2e-vm:
    ./scripts/vm/setup-vm-host.sh

# Boot a persistent e2e VM that stays up across multiple test runs. Idempotent (safe to run if
# already up). Needs `just setup-e2e-vm` to have been run once. Run this once per session, then
# `just e2e-vm-test` (or just `just ci`) as many times as you like, then `just e2e-vm-down` when
# done. This is the only VM-based e2e path — there used to be a one-shot boot/test/discard
# variant, but it had a permission bug in its 9p-based repo-sharing that duplicated
# `vm_sync_repo`'s job to fix properly, so it was removed rather than fixed twice.
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
