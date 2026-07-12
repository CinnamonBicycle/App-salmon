//! The set of cluster backends App Salmon knows how to provision, and the connection details
//! handed back once a cluster is ready.
//!
//! `ServiceKind` intentionally has one variant in phase 1. Deserializing any other value fails
//! with a serde error (mapped to `400 Bad Request` at the HTTP layer) rather than needing a
//! hand-written "unsupported service" branch — future phases add a variant and register a
//! backend for it, nothing else changes.

use crate::redacted::Redacted;
use serde::{Deserialize, Serialize};

/// A backend kind App Salmon knows how to provision. See the module docs for why adding a
/// variant is the only change needed to support a new backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    /// A bare Postgres instance, optionally with the `pgvector` extension enabled.
    Postgres,
}

/// What the caller asked for when creating a cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServiceSpec {
    /// Which backend to provision.
    pub kind: ServiceKind,
    /// Only meaningful for `ServiceKind::Postgres`; ignored (but accepted) otherwise so adding a
    /// service kind that doesn't have this concept isn't a breaking wire change.
    #[serde(default)]
    pub pgvector: bool,
}

/// How to reach a ready cluster. Fields are backend-specific; phase 1 only ever populates the
/// Postgres shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// Hostname or IP address to connect to — currently always `127.0.0.1`, since containers
    /// publish to the host's loopback interface.
    pub host: String,
    /// TCP port to connect to.
    pub port: u16,
    /// The database name to connect to.
    pub dbname: String,
    /// The database user to authenticate as.
    pub user: String,
    /// The database password to authenticate with.
    pub password: Redacted<String>,
}

#[cfg(test)]
mod tests {
    use super::{ServiceKind, ServiceSpec};

    #[test]
    fn service_kind_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ServiceKind::Postgres).expect("serialize"),
            "\"postgres\""
        );
    }

    #[test]
    fn service_kind_rejects_unknown_values() {
        let result: Result<ServiceKind, _> = serde_json::from_str("\"supabase\"");
        assert!(result.is_err());
    }

    #[test]
    fn service_spec_defaults_pgvector_to_false_when_absent() {
        let spec: ServiceSpec = serde_json::from_str(r#"{"kind":"postgres"}"#).expect("parse");
        assert!(!spec.pgvector);
    }

    #[test]
    fn service_spec_round_trip() {
        let spec = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: true,
        };
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: ServiceSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }
}
