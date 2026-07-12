# Single-command entry points for App Salmon. `just ci` is what CI (and you, before committing)
# should run; the pieces are also independently runnable.

# Run everything CI checks: formatting, lint, unit coverage, and e2e if this machine is set up
# for it (never silently skipped — see `test-e2e`).
ci: fmt-check lint test-unit
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1 && id salmon-worker-00 >/dev/null 2>&1; then
        just test-e2e
    else
        echo "e2e prerequisites not detected (docker reachable + salmon-worker-00 provisioned)."
        echo "Run 'sudo ./scripts/setup-e2e-env.sh' then 'just test-e2e' to run the full suite."
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
# since tests share a small fixed pool of real worker accounts.
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

# One-time setup for running the e2e suite on this machine (worker accounts, sudoers rule,
# postgres image). Needs root.
setup-e2e:
    sudo ./scripts/setup-e2e-env.sh
