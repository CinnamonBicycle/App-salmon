#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::missing_panics_doc
    )
)]

pub mod adapters;
pub mod auth;
pub mod backends;
pub mod config;
pub mod domain;
pub mod error;
pub mod http;
pub mod ports;
pub mod redacted;
pub mod service;
pub mod telemetry;
#[cfg(test)]
mod test_support;
pub mod worker_pool;
