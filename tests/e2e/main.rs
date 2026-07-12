//! End-to-end suite: every endpoint driven over real HTTP against a real Docker daemon, real
//! `sudo`, and real worker accounts. Requires `sudo ./scripts/setup-e2e-env.sh` to have been run
//! on this machine — `common::ensure_prerequisites` checks for that and panics with a clear
//! remediation message if it hasn't, rather than silently skipping.
//!
//! Separate from the unit suite (`cargo test --lib`): run with `cargo test --test e2e` /
//! `just test-e2e`. Prefer `--test-threads=1` (see the justfile) since tests share a small,
//! fixed pool of real worker accounts.
//!
//! This binary target is not covered by `src/lib.rs`'s strict `#![cfg_attr(test, ...)]` lint
//! exemptions (it's a separate crate), so the same exemptions are declared here directly —
//! `.unwrap()`/`.expect()`/`panic!` are the normal, correct way to fail a test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    // `serde_json::Value`'s `Index` impl returns `Value::Null` for a missing key/type mismatch —
    // it never panics — so this lint's generic "indexing may panic" warning is a false positive
    // for every `value["field"]` access in this suite.
    clippy::indexing_slicing
)]

mod common;
mod create_cluster;
mod delete_cluster;
mod info_cluster;
mod list_clusters;
mod ttl_expiry;
