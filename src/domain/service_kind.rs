//! The set of cluster backends App Salmon knows how to provision, and the connection details
//! handed back once a cluster is ready.
//!
//! Deserializing any `ServiceKind` value not listed below fails with a serde error (mapped to
//! `400 Bad Request` at the HTTP layer) rather than needing a hand-written "unsupported service"
//! branch — adding a variant and registering a backend for it is the only change needed.

use crate::redacted::Redacted;
use serde::{Deserialize, Serialize};

/// A backend kind App Salmon knows how to provision. See the module docs for why adding a
/// variant is the only change needed to support a new backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    /// A bare Postgres instance, optionally with the `pgvector` extension enabled.
    Postgres,
    /// A Supabase stack (Postgres+pgvector, `PostgREST`, `GoTrue`, Kong, and a Kata-sandboxed
    /// edge-function runtime) — see `docs/DESIGN.md` §11.
    Supabase,
}

/// What the caller asked for when creating a cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServiceSpec {
    /// Which backend to provision.
    pub kind: ServiceKind,
    /// Only meaningful for `ServiceKind::Postgres`; ignored (but accepted) otherwise so adding a
    /// service kind that doesn't have this concept isn't a breaking wire change.
    /// `ServiceKind::Supabase` clusters always have pgvector enabled — not caller-optional (see
    /// `docs/DESIGN.md` §11).
    #[serde(default)]
    pub pgvector: bool,
}

/// How to reach a ready cluster — backend-specific, one variant per [`ServiceKind`]. A closed
/// enum (matching `ClusterState`'s pattern) rather than one flat struct with backend-specific
/// fields, since Supabase's connection details (several URLs/keys) don't fit the single
/// `host`/`port`/`dbname`/`user`/`password` tuple `ServiceKind::Postgres` clusters use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionInfo {
    /// Connection details for a `ServiceKind::Postgres` cluster.
    Postgres(PostgresConnectionInfo),
    /// Connection details for a `ServiceKind::Supabase` cluster.
    Supabase(SupabaseConnectionInfo),
}

/// How to reach a ready `ServiceKind::Postgres` cluster (and the Postgres instance underlying a
/// `ServiceKind::Supabase` cluster — see [`SupabaseConnectionInfo::postgres`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresConnectionInfo {
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

/// How to reach a ready `ServiceKind::Supabase` cluster. Only `api_url` (Kong's published
/// address) is meant to be given to arbitrary callers — the keys below are also returned since
/// the caller *is* the trusted owner of this cluster, the same way `PostgresConnectionInfo`
/// hands back a plaintext password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupabaseConnectionInfo {
    /// Kong's published `host:port` — the single ingress for `PostgREST`/`GoTrue`/edge-function
    /// traffic.
    pub api_url: String,
    /// Direct connection details for the underlying Postgres instance.
    pub postgres: PostgresConnectionInfo,
    /// A JWT signed with the `anon` role, for anonymous/public API access through Kong.
    pub anon_key: Redacted<String>,
    /// A JWT signed with the `service_role` role, for privileged API access through Kong.
    pub service_role_key: Redacted<String>,
    /// The secret `anon_key`/`service_role_key` are signed with — needed by a caller that wants
    /// to mint its own additional tokens (e.g. for tests exercising specific roles/claims).
    pub jwt_secret: Redacted<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectionInfo, PostgresConnectionInfo, ServiceKind, ServiceSpec, SupabaseConnectionInfo,
    };
    use crate::redacted::Redacted;

    #[test]
    fn service_kind_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ServiceKind::Postgres).expect("serialize"),
            "\"postgres\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceKind::Supabase).expect("serialize"),
            "\"supabase\""
        );
    }

    #[test]
    fn service_kind_round_trips_supabase() {
        let kind: ServiceKind = serde_json::from_str("\"supabase\"").expect("parse");
        assert_eq!(kind, ServiceKind::Supabase);
    }

    #[test]
    fn service_kind_rejects_unknown_values() {
        let result: Result<ServiceKind, _> = serde_json::from_str("\"mysql\"");
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

    fn sample_postgres_connection() -> PostgresConnectionInfo {
        PostgresConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 55432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: Redacted::new("secret".to_string()),
        }
    }

    #[test]
    fn connection_info_postgres_variant_equality() {
        let a = ConnectionInfo::Postgres(sample_postgres_connection());
        let b = ConnectionInfo::Postgres(sample_postgres_connection());
        assert_eq!(a, b);
    }

    #[test]
    fn connection_info_supabase_variant_carries_a_nested_postgres_connection() {
        let connection = ConnectionInfo::Supabase(SupabaseConnectionInfo {
            api_url: "http://127.0.0.1:8000".to_string(),
            postgres: sample_postgres_connection(),
            anon_key: Redacted::new("anon.jwt".to_string()),
            service_role_key: Redacted::new("service.jwt".to_string()),
            jwt_secret: Redacted::new("jwt-secret".to_string()),
        });
        match connection {
            ConnectionInfo::Supabase(supabase) => {
                assert_eq!(supabase.api_url, "http://127.0.0.1:8000");
                assert_eq!(supabase.postgres, sample_postgres_connection());
            }
            ConnectionInfo::Postgres(_) => panic!("expected Supabase variant"),
        }
    }
}
