//! A Supabase stack: Postgres (Supabase's own image, with pgvector, roles, and schemas baked in),
//! `PostgREST`, `GoTrue`, Kong (the single ingress), and a Kata-sandboxed edge-function runtime —
//! see `docs/DESIGN.md` §11.
//!
//! Container images, port numbers, environment variable names, healthcheck commands, and mount
//! paths below are transcribed from upstream Supabase's own self-hosting
//! `docker/docker-compose.yml` (`supabase/supabase` on GitHub, read directly — not assumed from
//! training data) at the time this was written, restricted to the five services this backend
//! actually runs (Storage/Realtime/`postgres-meta`/Studio/Supavisor/Logflare are all omitted, see
//! `docs/DESIGN.md` §11's scope decision). This is **not** the same as having booted these five
//! containers together ourselves — some interactions (whether every env var named here is
//! actually sufficient, whether the `db` container needs anything from upstream's `db-config`
//! named volume to start cleanly) are only knowable by doing that, which is M6's remaining work.
//!
//! The `db` container specifically cannot be the same plain `pgvector/pgvector` image
//! [`crate::backends::postgres::PostgresBackend`] uses: `PostgREST`/`GoTrue` connect as
//! Supabase-specific roles (`authenticator`, `supabase_auth_admin`) and expect Supabase's own
//! `auth`/schemas already migrated in. Upstream's own `supabase/postgres` image bakes all of that
//! in; `roles.sql`/`jwt.sql` (mounted into `/docker-entrypoint-initdb.d/init-scripts/`, verbatim
//! from upstream — see [`ROLES_SQL`]/[`JWT_SQL`]) only need to set the *dynamic* per-deployment
//! password and JWT secret on those already-existing roles, via `psql`'s own `` \\set var
//! `echo "$ENV_VAR"` `` mechanism reading the container's own environment — so these two files
//! don't need any per-cluster templating on our side, just to be mounted in with the right env
//! vars set alongside them.
//!
//! What's structurally settled here — data-driven service list, network lifecycle, sequential
//! health-waits, JWT signing, and the `project` subdirectory convention `service::spawn_task`
//! populates before this backend's `spawn` is ever called — is fake-tested and not expected to
//! change in M6.
//!
//! Unlike [`crate::backends::postgres::PostgresBackend`], none of the five containers here bind-mount
//! durable, worker-owned storage for their *own* state (Postgres included) — these are ephemeral,
//! TTL'd clusters, so losing in-container state on a daemon restart is an accepted tradeoff, the
//! same one implicit in every other backend's data not surviving a full `app_salmon` restart with
//! Docker itself down. The one exception is the caller's own uploaded project tree, which *is*
//! worker-owned (via [`ClusterBackend::worker_subdirs`] declaring `project` — see
//! `service::spawn_task::adopt_project_tar`), since that's the caller's own data, not this
//! backend's internal state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header as JwtHeader, encode as jwt_encode};
use serde::Serialize;

use crate::backends::{ClusterBackend, health_wait};
use crate::client_workers::worker_data_dir;
use crate::domain::cluster::ClusterError;
use crate::domain::ids::{ClusterId, WorkerUser};
use crate::domain::service_kind::{
    ConnectionInfo, PostgresConnectionInfo, ServiceKind, ServiceSpec, SupabaseConnectionInfo,
};
use crate::ports::container_runtime::{
    ContainerHandle, ContainerRuntime, ContainerSpec, ContainerStatus, HealthCheck,
    NetworkAttachment, NetworkHandle, OciRuntime, PortPublish,
};
use crate::ports::secrets::SecretGenerator;
use crate::redacted::Redacted;

const DB_PORT: u16 = 5432;
const REST_PORT: u16 = 3000;
const AUTH_PORT: u16 = 9999;
const KONG_PORT: u16 = 8000;
/// Matches upstream's `functions` service healthcheck target (`/dev/tcp/127.0.0.1/9000`) in
/// `docker/docker-compose.yml`.
const FUNCTIONS_PORT: u16 = 9000;

/// Supabase's Postgres image creates its superuser as `postgres`, not a caller-chosen name — this
/// backend doesn't set `POSTGRES_USER`, so this is the image's own default, not a name we chose.
const DB_USER: &str = "postgres";
const DB_NAME: &str = "postgres";
/// The role `PostgREST` connects as (`PGRST_DB_URI`) — one of the roles `ROLES_SQL` sets the
/// shared `db_password` on; baked into the `supabase/postgres` image itself, not created by us.
const POSTGREST_DB_ROLE: &str = "authenticator";
/// The role `GoTrue` connects as (`GOTRUE_DB_DATABASE_URL`) — see [`POSTGREST_DB_ROLE`].
const GOTRUE_DB_ROLE: &str = "supabase_auth_admin";
const DB_PASSWORD_LEN: usize = 32;
const JWT_SECRET_LEN: usize = 40;
/// Value for the `app.settings.jwt_exp`/`GOTRUE_JWT_EXP`/`PGRST_APP_SETTINGS_JWT_EXP` GUCs —
/// upstream's own `.env.example` default (seconds). Distinct from [`JWT_VALIDITY_SECS`]: this is
/// how long a token these services *themselves* mint should live, not how long the tokens *this
/// backend* signs and hands back in [`SupabaseConnectionInfo`] should live.
const DB_JWT_EXP_SECS: &str = "3600";
/// How long a signed `anon`/`service_role` JWT stays valid for — deliberately long (not tied to
/// the cluster's own TTL, which `spawn` doesn't have access to): an expired-but-otherwise-valid
/// cluster's tokens failing early would be a confusing, silent failure mode a caller has no way
/// to distinguish from an actual auth problem.
const JWT_VALIDITY_SECS: i64 = 10 * 365 * 24 * 60 * 60;

const CLUSTER_ID_LABEL: &str = "app_salmon.cluster_id";

/// Worker-owned subdirectory (under the cluster's slot directory) an uploaded `project_tar` is
/// adopted into — must match `service::spawn_task::PROJECT_SUBDIR` exactly (see
/// [`ClusterBackend::worker_subdirs`]'s doc comment for why the two aren't literally the same
/// constant: `spawn_task` is kind-agnostic and doesn't import backend-specific names).
const PROJECT_SUBDIR: &str = "project";
/// Where the caller's `functions/` subtree is mounted inside the edge-runtime container — matches
/// upstream's own `./volumes/functions:/home/deno/functions` bind mount.
const FUNCTIONS_CONTAINER_PATH: &str = "/home/deno/functions";
/// `edge-runtime`'s `--main-service` flag names a single directory to serve as the fallback
/// function for any request path Kong routes to `/functions/v1/*` — matching upstream's own
/// `--main-service /home/deno/functions/main`. This means a caller's uploaded tar must place its
/// (single, for this pass) edge function at `functions/main/` — e.g. `functions/main/index.ts` —
/// not an arbitrarily-named subdirectory; per-path routing to multiple named functions is
/// upstream's own `EDGE_RUNTIME_ROUTER_CONFIG` mechanism, out of scope here (see `docs/DESIGN.md`
/// §11's scope decision — one edge-function slot, not a general router).
const FUNCTIONS_MAIN_SERVICE_PATH: &str = "/home/deno/functions/main";

/// Verbatim from `supabase/supabase`'s `docker/volumes/db/roles.sql` — sets the shared
/// `db_password` (read from the container's own `$POSTGRES_PASSWORD`, via `psql`'s `\set ... `
/// backtick-exec, not templated by us) on the internal roles the base image already creates.
/// Pinned as a `const` rather than fetched at runtime so a cluster can spawn without network
/// access beyond pulling images once — re-sync by hand if upstream changes this file.
const ROLES_SQL: &str = r#"-- NOTE: change to your own passwords for production environments
\set pgpass `echo "$POSTGRES_PASSWORD"`

ALTER USER authenticator WITH PASSWORD :'pgpass';
ALTER USER pgbouncer WITH PASSWORD :'pgpass';
ALTER USER supabase_auth_admin WITH PASSWORD :'pgpass';
ALTER USER supabase_functions_admin WITH PASSWORD :'pgpass';
ALTER USER supabase_storage_admin WITH PASSWORD :'pgpass';
"#;

/// Verbatim from `supabase/supabase`'s `docker/volumes/db/jwt.sql` — sets the
/// `app.settings.jwt_secret`/`app.settings.jwt_exp` database-level GUCs some of Supabase's baked-in
/// Postgres functions read, from the container's own `$JWT_SECRET`/`$JWT_EXP`. See [`ROLES_SQL`]
/// for why this is pinned verbatim rather than templated.
const JWT_SQL: &str = r#"\set jwt_secret `echo "$JWT_SECRET"`
\set jwt_exp `echo "$JWT_EXP"`

ALTER DATABASE postgres SET "app.settings.jwt_secret" TO :'jwt_secret';
ALTER DATABASE postgres SET "app.settings.jwt_exp" TO :'jwt_exp';
"#;

/// Claims signed into the `anon`/`service_role` JWTs handed back in [`SupabaseConnectionInfo`].
/// Mirrors real Supabase's own minimal claim set closely enough for `PostgREST`/`GoTrue` to
/// recognize the `role` claim; not claiming full compatibility beyond that.
#[derive(Serialize)]
struct SupabaseClaims<'a> {
    role: &'a str,
    iss: &'static str,
    iat: i64,
    exp: i64,
}

/// Signs a JWT asserting `role`, using `secret` as the HS256 signing key.
///
/// # Arguments
///
/// - `secret`: the HS256 signing secret (shared with `PostgREST`/`GoTrue` via their own env vars,
///   so they can verify tokens signed here).
/// - `role`: the `role` claim to assert (`"anon"` or `"service_role"`).
///
/// # Returns
///
/// The signed, encoded JWT string.
///
/// # Errors
///
/// [`ClusterError::BackendSpawnFailed`] if signing itself fails (not expected in normal
/// operation — `jsonwebtoken` only fails encoding on a malformed key, which a freshly generated
/// random secret never produces).
fn sign_jwt(secret: &str, role: &str) -> Result<String, ClusterError> {
    let now = chrono::Utc::now().timestamp();
    let claims = SupabaseClaims {
        role,
        iss: "app_salmon",
        iat: now,
        exp: now + JWT_VALIDITY_SECS,
    };
    jwt_encode(
        &JwtHeader::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|_source| ClusterError::BackendSpawnFailed("failed to sign JWT".to_string()))
}

/// Kong's DB-less declarative config (`_format_version: "3.0"`), routing App Salmon's minimal
/// ingress surface (`/rest/v1`, `/auth/v1`, `/functions/v1`) to the other containers by their
/// network alias. Uses Kong's traditional (`paths:`-based) route matching, which works under
/// Kong's default router flavor — deliberately does *not* set `KONG_ROUTER_FLAVOR: expressions`
/// the way upstream's own compose file does, since that flavor expects routes written in Kong's
/// newer expression DSL instead of this simpler form.
///
/// # Arguments
///
/// - `rest_alias` / `auth_alias` / `functions_alias`: the Docker network aliases Kong resolves
///   the other containers by (all on the same user-defined network Kong itself is attached to).
///
/// # Returns
///
/// The YAML document to write to the file bind-mounted into Kong at `KONG_DECLARATIVE_CONFIG`.
fn kong_declarative_config(rest_alias: &str, auth_alias: &str, functions_alias: &str) -> String {
    format!(
        "_format_version: \"3.0\"\n\
         services:\n\
         \x20\x20- name: rest\n\
         \x20\x20\x20\x20url: http://{rest_alias}:{REST_PORT}\n\
         \x20\x20\x20\x20routes:\n\
         \x20\x20\x20\x20\x20\x20- name: rest-route\n\
         \x20\x20\x20\x20\x20\x20\x20\x20paths:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20- /rest/v1\n\
         \x20\x20- name: auth\n\
         \x20\x20\x20\x20url: http://{auth_alias}:{AUTH_PORT}\n\
         \x20\x20\x20\x20routes:\n\
         \x20\x20\x20\x20\x20\x20- name: auth-route\n\
         \x20\x20\x20\x20\x20\x20\x20\x20paths:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20- /auth/v1\n\
         \x20\x20- name: functions\n\
         \x20\x20\x20\x20url: http://{functions_alias}:{FUNCTIONS_PORT}\n\
         \x20\x20\x20\x20routes:\n\
         \x20\x20\x20\x20\x20\x20- name: functions-route\n\
         \x20\x20\x20\x20\x20\x20\x20\x20paths:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20- /functions/v1\n"
    )
}

/// Computes the deterministic name for one of this cluster's containers, so teardown can find
/// every container again without any extra persisted lookup state — mirrors
/// `backends::postgres::container_name`.
///
/// # Arguments
///
/// - `cluster_id`: the cluster this container belongs to.
/// - `suffix`: which of the five containers (`"db"`, `"rest"`, `"auth"`, `"kong"`, `"functions"`).
///
/// # Returns
///
/// The container name to create/look up, of the form `app-salmon-<cluster_id>-<suffix>`.
fn container_name(cluster_id: &ClusterId, suffix: &str) -> String {
    format!("app-salmon-{cluster_id}-{suffix}")
}

/// Computes this cluster's deterministic network name, mirroring [`container_name`].
///
/// # Arguments
///
/// - `cluster_id`: the cluster this network belongs to.
///
/// # Returns
///
/// The network name to create/look up, of the form `app-salmon-net-<cluster_id>`.
fn network_name(cluster_id: &ClusterId) -> String {
    format!("app-salmon-net-{cluster_id}")
}

/// Every container this backend manages, in spawn order — later entries may assume earlier ones
/// are already up (e.g. `rest`/`auth` assume `db` is reachable at its network alias; `functions`'s
/// route only resolves once Kong's declarative config, written before Kong starts, names it).
/// Adding a future service (Storage, Realtime, `postgres-meta`) means adding one more entry to
/// [`SupabaseBackend::spawn`]'s construction of this list, not restructuring the loop that
/// creates/waits on each one.
const SERVICE_SUFFIXES: [&str; 5] = ["db", "rest", "auth", "kong", "functions"];

pub struct SupabaseBackend {
    container_runtime: Arc<dyn ContainerRuntime>,
    secrets: Arc<dyn SecretGenerator>,
    postgres_image: String,
    postgrest_image: String,
    gotrue_image: String,
    kong_image: String,
    edge_runtime_image: String,
    worker_data_dir_base: PathBuf,
    /// Base directory `spawn` writes each cluster's generated `kong.yml`/`roles.sql`/`jwt.sql`
    /// into — `app_salmon`-owned (unprivileged: this is `app_salmon`'s own generated config, not
    /// the caller's data, so it needs no worker-ownership dance), one subdirectory per cluster,
    /// removed by `teardown`.
    generated_config_dir_base: PathBuf,
    health_check_timeout: Duration,
}

impl SupabaseBackend {
    /// Builds a `SupabaseBackend` from its dependencies and configuration.
    ///
    /// # Arguments
    ///
    /// - `container_runtime`: how to create/inspect/remove containers and networks.
    /// - `secrets`: source of generated passwords/JWT secrets.
    /// - `postgres_image` / `postgrest_image` / `gotrue_image` / `kong_image` /
    ///   `edge_runtime_image`: the image references to run for each container.
    /// - `worker_data_dir_base`: base directory under which each worker's own data directory
    ///   lives — used to recompute the `project` subdirectory `service::spawn_task` populated.
    /// - `generated_config_dir_base`: base directory to write each cluster's generated `kong.yml` into.
    /// - `health_check_timeout`: the overall deadline each container's health-wait polls against.
    ///
    /// # Returns
    ///
    /// A ready-to-use `SupabaseBackend`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    // `postgres_image`/`postgrest_image` are deliberately named for the two distinct real
    // products they configure (Postgres vs. PostgREST), not accidentally similar.
    #[allow(clippy::similar_names)]
    pub fn new(
        container_runtime: Arc<dyn ContainerRuntime>,
        secrets: Arc<dyn SecretGenerator>,
        postgres_image: String,
        postgrest_image: String,
        gotrue_image: String,
        kong_image: String,
        edge_runtime_image: String,
        worker_data_dir_base: PathBuf,
        generated_config_dir_base: PathBuf,
        health_check_timeout: Duration,
    ) -> Self {
        Self {
            container_runtime,
            secrets,
            postgres_image,
            postgrest_image,
            gotrue_image,
            kong_image,
            edge_runtime_image,
            worker_data_dir_base,
            generated_config_dir_base,
            health_check_timeout,
        }
    }

    /// Builds the five `ContainerSpec`s for `cluster_id`, in spawn order — a private helper so
    /// `spawn` itself reads as "build specs, then create/wait on each," and so tests can inspect
    /// the specs without a real `ContainerRuntime`. Long by line count, not by complexity: it's
    /// five near-identical struct literals, not control flow — splitting it up would trade one
    /// easy-to-scan function for five hard-to-follow ones.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn build_specs(
        &self,
        cluster_id: &ClusterId,
        worker: &WorkerUser,
        db_password: &str,
        jwt_secret: &str,
        anon_key: &str,
        service_role_key: &str,
        kong_config_path: &str,
        roles_sql_path: &str,
        jwt_sql_path: &str,
        functions_host_path: &str,
    ) -> Vec<ContainerSpec> {
        let net = network_name(cluster_id);
        let mut labels = HashMap::with_capacity(1);
        labels.insert(CLUSTER_ID_LABEL.to_string(), cluster_id.to_string());

        let attach = |alias: &str| {
            Some(NetworkAttachment {
                network_name: net.clone(),
                alias: alias.to_string(),
            })
        };

        let db = ContainerSpec {
            name: container_name(cluster_id, "db"),
            image: self.postgres_image.clone(),
            env: vec![
                ("POSTGRES_DB".to_string(), DB_NAME.to_string()),
                ("POSTGRES_PASSWORD".to_string(), db_password.to_string()),
                ("JWT_SECRET".to_string(), jwt_secret.to_string()),
                ("JWT_EXP".to_string(), DB_JWT_EXP_SECS.to_string()),
            ],
            labels: labels.clone(),
            port_publish: PortPublish::Ephemeral,
            container_port: DB_PORT,
            bind_mounts: vec![
                crate::ports::container_runtime::BindMount {
                    host_path: roles_sql_path.to_string(),
                    container_path: "/docker-entrypoint-initdb.d/init-scripts/99-roles.sql"
                        .to_string(),
                },
                crate::ports::container_runtime::BindMount {
                    host_path: jwt_sql_path.to_string(),
                    container_path: "/docker-entrypoint-initdb.d/init-scripts/99-jwt.sql"
                        .to_string(),
                },
            ],
            // No worker-owned host storage backs this container's data directory (see the module
            // doc comment — Supabase clusters don't persist container-internal state across a
            // restart), so there's no host-ownership reason to run as `worker`, and doing so would
            // fight the `supabase/postgres` image's own baked-in expectation that its data
            // directory is owned by whatever internal user its entrypoint runs as.
            run_as: None,
            health_check: Some(HealthCheck {
                test: vec![
                    "CMD".to_string(),
                    "pg_isready".to_string(),
                    "-U".to_string(),
                    DB_USER.to_string(),
                    "-h".to_string(),
                    "localhost".to_string(),
                ],
                interval: Duration::from_secs(1),
                timeout: Duration::from_secs(2),
                retries: 10,
            }),
            runtime: OciRuntime::Runc,
            network: attach("db"),
            command: None,
        };

        let rest = ContainerSpec {
            name: container_name(cluster_id, "rest"),
            image: self.postgrest_image.clone(),
            env: vec![
                (
                    "PGRST_DB_URI".to_string(),
                    format!("postgres://{POSTGREST_DB_ROLE}:{db_password}@db:{DB_PORT}/{DB_NAME}"),
                ),
                (
                    "PGRST_DB_SCHEMAS".to_string(),
                    "public,graphql_public".to_string(),
                ),
                ("PGRST_DB_ANON_ROLE".to_string(), "anon".to_string()),
                ("PGRST_JWT_SECRET".to_string(), jwt_secret.to_string()),
            ],
            labels: labels.clone(),
            port_publish: PortPublish::Unpublished,
            container_port: REST_PORT,
            bind_mounts: vec![],
            run_as: None,
            health_check: Some(HealthCheck {
                test: vec![
                    "CMD".to_string(),
                    "postgrest".to_string(),
                    "--ready".to_string(),
                ],
                interval: Duration::from_secs(1),
                timeout: Duration::from_secs(2),
                retries: 5,
            }),
            runtime: OciRuntime::Runc,
            network: attach("rest"),
            command: None,
        };

        let auth = ContainerSpec {
            name: container_name(cluster_id, "auth"),
            image: self.gotrue_image.clone(),
            env: vec![
                ("GOTRUE_API_HOST".to_string(), "0.0.0.0".to_string()),
                ("GOTRUE_API_PORT".to_string(), AUTH_PORT.to_string()),
                (
                    "API_EXTERNAL_URL".to_string(),
                    "http://kong:8000/auth/v1".to_string(),
                ),
                ("GOTRUE_DB_DRIVER".to_string(), "postgres".to_string()),
                (
                    "GOTRUE_DB_DATABASE_URL".to_string(),
                    format!("postgres://{GOTRUE_DB_ROLE}:{db_password}@db:{DB_PORT}/{DB_NAME}"),
                ),
                (
                    "GOTRUE_SITE_URL".to_string(),
                    "http://localhost:3000".to_string(),
                ),
                ("GOTRUE_DISABLE_SIGNUP".to_string(), "false".to_string()),
                (
                    "GOTRUE_JWT_ADMIN_ROLES".to_string(),
                    "service_role".to_string(),
                ),
                ("GOTRUE_JWT_AUD".to_string(), "authenticated".to_string()),
                (
                    "GOTRUE_JWT_DEFAULT_GROUP_NAME".to_string(),
                    "authenticated".to_string(),
                ),
                ("GOTRUE_JWT_EXP".to_string(), DB_JWT_EXP_SECS.to_string()),
                ("GOTRUE_JWT_SECRET".to_string(), jwt_secret.to_string()),
            ],
            labels: labels.clone(),
            port_publish: PortPublish::Unpublished,
            container_port: AUTH_PORT,
            bind_mounts: vec![],
            run_as: None,
            health_check: Some(HealthCheck {
                test: vec![
                    "CMD".to_string(),
                    "wget".to_string(),
                    "--no-verbose".to_string(),
                    "--tries=1".to_string(),
                    "--spider".to_string(),
                    "http://localhost:9999/health".to_string(),
                ],
                interval: Duration::from_secs(1),
                timeout: Duration::from_secs(2),
                retries: 5,
            }),
            runtime: OciRuntime::Runc,
            network: attach("auth"),
            command: None,
        };

        let kong = ContainerSpec {
            name: container_name(cluster_id, "kong"),
            image: self.kong_image.clone(),
            env: vec![
                ("KONG_DATABASE".to_string(), "off".to_string()),
                (
                    "KONG_DECLARATIVE_CONFIG".to_string(),
                    "/usr/local/kong/kong.yml".to_string(),
                ),
                ("SUPABASE_ANON_KEY".to_string(), anon_key.to_string()),
                (
                    "SUPABASE_SERVICE_KEY".to_string(),
                    service_role_key.to_string(),
                ),
            ],
            labels: labels.clone(),
            port_publish: PortPublish::Ephemeral,
            container_port: KONG_PORT,
            bind_mounts: vec![crate::ports::container_runtime::BindMount {
                host_path: kong_config_path.to_string(),
                container_path: "/usr/local/kong/kong.yml".to_string(),
            }],
            run_as: None,
            health_check: Some(HealthCheck {
                test: vec!["CMD".to_string(), "kong".to_string(), "health".to_string()],
                interval: Duration::from_secs(1),
                timeout: Duration::from_secs(2),
                retries: 5,
            }),
            runtime: OciRuntime::Runc,
            network: attach("kong"),
            command: None,
        };

        let functions = ContainerSpec {
            name: container_name(cluster_id, "functions"),
            image: self.edge_runtime_image.clone(),
            env: vec![
                ("JWT_SECRET".to_string(), jwt_secret.to_string()),
                ("SUPABASE_URL".to_string(), "http://kong:8000".to_string()),
                ("SUPABASE_ANON_KEY".to_string(), anon_key.to_string()),
                (
                    "SUPABASE_SERVICE_ROLE_KEY".to_string(),
                    service_role_key.to_string(),
                ),
                (
                    "SUPABASE_DB_URL".to_string(),
                    format!("postgresql://{DB_USER}:{db_password}@db:{DB_PORT}/{DB_NAME}"),
                ),
                ("VERIFY_JWT".to_string(), "false".to_string()),
            ],
            labels,
            port_publish: PortPublish::Unpublished,
            container_port: FUNCTIONS_PORT,
            bind_mounts: vec![crate::ports::container_runtime::BindMount {
                host_path: functions_host_path.to_string(),
                container_path: FUNCTIONS_CONTAINER_PATH.to_string(),
            }],
            // Unlike `db`, this container's one bind mount *is* worker-owned host data (the
            // caller's uploaded functions/ subtree) — running as `worker` sidesteps having to
            // reason about whatever mode `PrepareWorkerDir`'s `mkdir -p` left it at.
            run_as: Some((worker.uid(), worker.gid())),
            health_check: None,
            runtime: OciRuntime::Kata,
            network: attach("functions"),
            command: Some(vec![
                "start".to_string(),
                "--main-service".to_string(),
                FUNCTIONS_MAIN_SERVICE_PATH.to_string(),
            ]),
        };

        vec![db, rest, auth, kong, functions]
    }
}

#[async_trait]
impl ClusterBackend for SupabaseBackend {
    /// # Returns
    ///
    /// Always [`ServiceKind::Supabase`] — this backend only ever handles the Supabase kind.
    fn kind(&self) -> ServiceKind {
        ServiceKind::Supabase
    }

    /// # Returns
    ///
    /// `&["project/functions"]`, not just `&["project"]`: `service::spawn_task` runs
    /// `PrepareWorkerDir` for every declared entry *before* `adopt_project_tar` copies the
    /// caller's upload in, and `mkdir -p` creates every missing ancestor — so this guarantees
    /// `<slot>/project/functions` exists, worker-owned, even if the caller's tar never contained a
    /// `functions/` entry at all (a `functions` bind-mount source Docker had to create itself
    /// would be root-owned, the same worker-ownership trap `worker_subdirs` exists to avoid in the
    /// first place — see `docs/DESIGN.md` §11's M4a section). `AdoptStagedTree`'s `cp -r` merges
    /// into an already-existing directory without complaint, so this is safe whether or not the
    /// tar actually included `functions/`.
    fn worker_subdirs(&self) -> &[&'static str] {
        &["project/functions"]
    }

    /// Creates the Docker network, then each of the five containers in order (`db` → `rest` →
    /// `auth` → `kong` → `functions`), health-waiting on each before moving to the next. Writes
    /// Kong's declarative config (routing to `rest`/`auth`/`functions` by network alias) before
    /// creating Kong itself.
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster this stack is being provisioned for.
    /// - `worker`: the pre-allocated worker account `db` and `functions` run as, and whose
    ///   `project` subdirectory (already populated — see [`Self::worker_subdirs`]) `functions`
    ///   mounts from.
    /// - `slot`: `cluster_id`'s assigned directory slot, used with `worker` to recompute the
    ///   `project` subdirectory's path.
    /// - `service`: unused beyond `service.kind` (already known to be `Supabase`) — `pgvector` is
    ///   always enabled for this kind (see `ServiceSpec::pgvector`'s doc comment), so there's
    ///   nothing else to consult here.
    ///
    /// # Returns
    ///
    /// A [`ConnectionInfo::Supabase`] with Kong's published address, direct Postgres connection
    /// details, and signed `anon`/`service_role` JWTs.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if the network or any container fails to create, or any container never
    /// becomes healthy within `health_check_timeout`.
    async fn spawn(
        &self,
        cluster_id: &ClusterId,
        worker: &WorkerUser,
        slot: u32,
        _service: &ServiceSpec,
    ) -> Result<ConnectionInfo, ClusterError> {
        let db_password = self.secrets.db_password(DB_PASSWORD_LEN);
        let jwt_secret = self.secrets.db_password(JWT_SECRET_LEN);
        // Signed up front, not after the spawn loop: the `functions` container's own env vars
        // need these (`SUPABASE_ANON_KEY`/`SUPABASE_SERVICE_ROLE_KEY`), and it's created within
        // this same loop below.
        let anon_key = sign_jwt(&jwt_secret, "anon")?;
        let service_role_key = sign_jwt(&jwt_secret, "service_role")?;

        self.container_runtime
            .create_network(&network_name(cluster_id))
            .await?;

        let generated_config_dir = self.generated_config_dir_base.join(cluster_id.to_string());
        tokio::fs::create_dir_all(&generated_config_dir)
            .await
            .map_err(|_source| {
                ClusterError::BackendSpawnFailed(
                    "failed to prepare generated config directory".to_string(),
                )
            })?;
        let kong_config_path = generated_config_dir.join("kong.yml");
        tokio::fs::write(
            &kong_config_path,
            kong_declarative_config("rest", "auth", "functions"),
        )
        .await
        .map_err(|_source| {
            ClusterError::BackendSpawnFailed("failed to write kong declarative config".to_string())
        })?;
        let roles_sql_path = generated_config_dir.join("roles.sql");
        tokio::fs::write(&roles_sql_path, ROLES_SQL)
            .await
            .map_err(|_source| {
                ClusterError::BackendSpawnFailed("failed to write roles.sql".to_string())
            })?;
        let jwt_sql_path = generated_config_dir.join("jwt.sql");
        tokio::fs::write(&jwt_sql_path, JWT_SQL)
            .await
            .map_err(|_source| {
                ClusterError::BackendSpawnFailed("failed to write jwt.sql".to_string())
            })?;

        let functions_host_path = worker_data_dir(&self.worker_data_dir_base, worker, slot)
            .join(PROJECT_SUBDIR)
            .join("functions");

        let specs = self.build_specs(
            cluster_id,
            worker,
            &db_password,
            &jwt_secret,
            &anon_key,
            &service_role_key,
            &kong_config_path.display().to_string(),
            &roles_sql_path.display().to_string(),
            &jwt_sql_path.display().to_string(),
            &functions_host_path.display().to_string(),
        );

        let mut kong_port = None;
        for spec in &specs {
            let handle = self.container_runtime.create_and_start(spec).await?;
            let published = health_wait::wait_until_healthy(
                self.container_runtime.as_ref(),
                &handle,
                self.health_check_timeout,
                spec.health_check.is_some(),
            )
            .await?;
            if spec.name == container_name(cluster_id, "kong") {
                kong_port = published;
            }
        }

        let kong_port = kong_port.ok_or_else(|| {
            ClusterError::BackendSpawnFailed(
                "kong reported healthy but published no port".to_string(),
            )
        })?;

        let db_handle = ContainerHandle::new(container_name(cluster_id, "db"));
        let ContainerStatus::Running {
            published_port: Some(db_port),
            ..
        } = self.container_runtime.inspect(&db_handle).await?
        else {
            return Err(ClusterError::BackendSpawnFailed(
                "db reported healthy but published no port".to_string(),
            ));
        };

        Ok(ConnectionInfo::Supabase(SupabaseConnectionInfo {
            api_url: format!("http://127.0.0.1:{kong_port}"),
            postgres: PostgresConnectionInfo {
                host: "127.0.0.1".to_string(),
                port: db_port,
                dbname: DB_NAME.to_string(),
                user: DB_USER.to_string(),
                password: Redacted::new(db_password),
            },
            anon_key: Redacted::new(anon_key),
            service_role_key: Redacted::new(service_role_key),
            jwt_secret: Redacted::new(jwt_secret),
        }))
    }

    /// Stops and removes every container `spawn` created for `cluster_id`, then the network last
    /// (Docker refuses removal while containers are attached), then removes the generated Kong
    /// config directory. Idempotent throughout: tolerates any subset of these already being gone
    /// (e.g. resuming after a crash mid-spawn, or a spawn that failed partway through).
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster whose resources should be torn down.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if an underlying `stop_and_remove`/`remove_network` call itself fails
    /// (not raised merely because a resource was already gone).
    async fn teardown(&self, cluster_id: &ClusterId) -> Result<(), ClusterError> {
        for suffix in SERVICE_SUFFIXES {
            let handle = ContainerHandle::new(container_name(cluster_id, suffix));
            self.container_runtime.stop_and_remove(&handle).await?;
        }
        self.container_runtime
            .remove_network(&NetworkHandle::new(network_name(cluster_id)))
            .await?;
        let generated_config_dir = self.generated_config_dir_base.join(cluster_id.to_string());
        if let Err(error) = tokio::fs::remove_dir_all(&generated_config_dir).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(cluster_id = %cluster_id, error = %error, "failed to remove kong config directory; leaking disk space, not failing teardown over it");
        }
        Ok(())
    }

    /// Checks whether every one of `cluster_id`'s containers still exists and is running.
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster whose resources should be checked.
    ///
    /// # Returns
    ///
    /// `true` only if all five containers are `Running`; `false` if any has exited or can't be
    /// found.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if any underlying `inspect` call itself fails.
    async fn is_alive(&self, cluster_id: &ClusterId) -> Result<bool, ClusterError> {
        for suffix in SERVICE_SUFFIXES {
            let handle = ContainerHandle::new(container_name(cluster_id, suffix));
            match self.container_runtime.inspect(&handle).await? {
                ContainerStatus::Running { .. } => {}
                ContainerStatus::Exited { .. } | ContainerStatus::NotFound => return Ok(false),
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{SupabaseBackend, container_name, network_name};
    use crate::backends::ClusterBackend;
    use crate::domain::ids::{ClusterId, WorkerUser};
    use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};
    use crate::ports::container_runtime::{
        ContainerHandle, ContainerRuntime, ContainerSpec, ContainerStatus, DockerError,
        HealthState, NetworkHandle, OciRuntime,
    };
    use crate::ports::secrets::SecretGenerator;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeContainerRuntime {
        created: Mutex<Vec<ContainerSpec>>,
        removed: Mutex<Vec<ContainerHandle>>,
        networks_created: Mutex<Vec<String>>,
        networks_removed: Mutex<Vec<NetworkHandle>>,
        /// `(name, published_port)` — every created container is reported healthy with this
        /// published port once inspected, unless the name matches `unhealthy_container`.
        published_port: u16,
        unhealthy_container: Option<&'static str>,
        fail_create_for: Option<&'static str>,
    }

    #[async_trait]
    impl ContainerRuntime for FakeContainerRuntime {
        async fn create_and_start(
            &self,
            spec: &ContainerSpec,
        ) -> Result<ContainerHandle, DockerError> {
            if self
                .fail_create_for
                .is_some_and(|name| spec.name.contains(name))
            {
                return Err(DockerError::CreateContainer {
                    source: bollard::errors::Error::DockerResponseServerError {
                        status_code: 500,
                        message: "boom".to_string(),
                    },
                });
            }
            self.created.lock().expect("lock").push(spec.clone());
            Ok(ContainerHandle::new(spec.name.clone()))
        }

        async fn inspect(&self, handle: &ContainerHandle) -> Result<ContainerStatus, DockerError> {
            if self
                .unhealthy_container
                .is_some_and(|name| handle.as_str().contains(name))
            {
                return Ok(ContainerStatus::Running {
                    published_port: Some(self.published_port),
                    health: Some(HealthState::Unhealthy),
                });
            }
            Ok(ContainerStatus::Running {
                published_port: Some(self.published_port),
                health: Some(HealthState::Healthy),
            })
        }

        async fn stop_and_remove(&self, handle: &ContainerHandle) -> Result<(), DockerError> {
            self.removed.lock().expect("lock").push(handle.clone());
            Ok(())
        }

        async fn create_network(&self, name: &str) -> Result<NetworkHandle, DockerError> {
            self.networks_created
                .lock()
                .expect("lock")
                .push(name.to_string());
            Ok(NetworkHandle::new(name))
        }

        async fn remove_network(&self, handle: &NetworkHandle) -> Result<(), DockerError> {
            self.networks_removed
                .lock()
                .expect("lock")
                .push(handle.clone());
            Ok(())
        }
    }

    #[derive(serde::Deserialize)]
    struct Claims {
        role: String,
    }

    struct FixedSecretGenerator;

    impl SecretGenerator for FixedSecretGenerator {
        fn cluster_id(&self) -> ClusterId {
            ClusterId::new(ulid::Ulid::nil())
        }

        fn db_password(&self, len: usize) -> String {
            "s".repeat(len)
        }
    }

    fn backend(
        runtime: FakeContainerRuntime,
        generated_config_dir: std::path::PathBuf,
    ) -> SupabaseBackend {
        SupabaseBackend::new(
            std::sync::Arc::new(runtime),
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            generated_config_dir,
            Duration::from_millis(50),
        )
    }

    fn worker() -> WorkerUser {
        WorkerUser::new("salmon-worker-00", 2000, 2000)
    }

    fn service_spec() -> ServiceSpec {
        ServiceSpec {
            kind: ServiceKind::Supabase,
            pgvector: false,
        }
    }

    #[test]
    fn kind_is_supabase() {
        let backend = backend(FakeContainerRuntime::default(), std::env::temp_dir());
        assert_eq!(backend.kind(), ServiceKind::Supabase);
    }

    #[test]
    fn worker_subdirs_declares_project_functions() {
        let backend = backend(FakeContainerRuntime::default(), std::env::temp_dir());
        assert_eq!(backend.worker_subdirs(), &["project/functions"]);
    }

    #[test]
    fn container_and_network_names_are_deterministic() {
        let id = ClusterId::new(ulid::Ulid::nil());
        assert_eq!(container_name(&id, "db"), format!("app-salmon-{id}-db"));
        assert_eq!(network_name(&id), format!("app-salmon-net-{id}"));
    }

    #[tokio::test]
    async fn spawn_creates_the_network_before_any_container() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            published_port: 5432,
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SupabaseBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        backend
            .spawn(&cluster_id, &worker(), 0, &service_spec())
            .await
            .expect("spawn succeeds");

        let networks = runtime.networks_created.lock().expect("lock");
        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0], network_name(&cluster_id));
        assert_eq!(runtime.created.lock().expect("lock").len(), 5);
    }

    #[tokio::test]
    async fn spawn_creates_five_containers_in_order_with_deterministic_names() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            published_port: 5432,
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SupabaseBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        backend
            .spawn(&cluster_id, &worker(), 0, &service_spec())
            .await
            .expect("spawn succeeds");

        let created = runtime.created.lock().expect("lock");
        let names: Vec<&str> = created.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                container_name(&cluster_id, "db"),
                container_name(&cluster_id, "rest"),
                container_name(&cluster_id, "auth"),
                container_name(&cluster_id, "kong"),
                container_name(&cluster_id, "functions"),
            ]
        );
    }

    #[tokio::test]
    async fn functions_container_uses_kata_and_the_rest_use_runc() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            published_port: 5432,
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SupabaseBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        backend
            .spawn(&cluster_id, &worker(), 0, &service_spec())
            .await
            .expect("spawn succeeds");

        let created = runtime.created.lock().expect("lock");
        for spec in created.iter() {
            if spec.name == container_name(&cluster_id, "functions") {
                assert_eq!(spec.runtime, OciRuntime::Kata);
            } else {
                assert_eq!(spec.runtime, OciRuntime::Runc);
            }
        }
    }

    #[tokio::test]
    async fn functions_container_bind_mounts_the_project_functions_subdir() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            published_port: 5432,
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SupabaseBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        backend
            .spawn(&cluster_id, &worker(), 3, &service_spec())
            .await
            .expect("spawn succeeds");

        let created = runtime.created.lock().expect("lock");
        let functions_spec = created
            .iter()
            .find(|spec| spec.name == container_name(&cluster_id, "functions"))
            .expect("functions container created");
        let mount = functions_spec.bind_mounts.first().expect("bind mount set");
        assert_eq!(
            mount.host_path,
            "/var/lib/app_salmon/workers/salmon-worker-00/slot-3/project/functions"
        );
    }

    #[tokio::test]
    async fn kong_bind_mounts_a_generated_declarative_config_routing_to_every_service() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            published_port: 5432,
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SupabaseBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        backend
            .spawn(&cluster_id, &worker(), 0, &service_spec())
            .await
            .expect("spawn succeeds");

        let kong_config_host_path = {
            let created = runtime.created.lock().expect("lock");
            let kong_spec = created
                .iter()
                .find(|spec| spec.name == container_name(&cluster_id, "kong"))
                .expect("kong container created");
            kong_spec
                .bind_mounts
                .first()
                .expect("bind mount set")
                .host_path
                .clone()
        };
        let config = tokio::fs::read_to_string(&kong_config_host_path)
            .await
            .expect("read generated kong config");
        assert!(config.contains("/rest/v1"));
        assert!(config.contains("/auth/v1"));
        assert!(config.contains("/functions/v1"));
        assert!(config.contains("http://rest:3000"));
        assert!(config.contains("http://auth:9999"));
        assert!(config.contains("http://functions:9000"));
    }

    #[tokio::test]
    async fn spawn_returns_supabase_connection_info_with_verifiable_jwts() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            published_port: 55000,
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SupabaseBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        let connection = backend
            .spawn(&cluster_id, &worker(), 0, &service_spec())
            .await
            .expect("spawn succeeds");

        let ConnectionInfo::Supabase(supabase) = connection else {
            panic!("expected Supabase connection info");
        };
        assert_eq!(supabase.api_url, "http://127.0.0.1:55000");
        assert_eq!(supabase.postgres.port, 55000);
        assert_eq!(supabase.postgres.host, "127.0.0.1");

        let validation = {
            let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
            v.validate_exp = false;
            v
        };
        let key = jsonwebtoken::DecodingKey::from_secret(supabase.jwt_secret.expose().as_bytes());
        let anon = jsonwebtoken::decode::<Claims>(supabase.anon_key.expose(), &key, &validation)
            .expect("anon jwt verifies against jwt_secret");
        assert_eq!(anon.claims.role, "anon");
        let service_role =
            jsonwebtoken::decode::<Claims>(supabase.service_role_key.expose(), &key, &validation)
                .expect("service_role jwt verifies against jwt_secret");
        assert_eq!(service_role.claims.role, "service_role");
    }

    #[tokio::test]
    async fn spawn_propagates_a_container_create_failure() {
        let runtime = FakeContainerRuntime {
            published_port: 5432,
            fail_create_for: Some("rest"),
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = backend(runtime, dir.path().to_path_buf());
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        let err = backend
            .spawn(&cluster_id, &worker(), 0, &service_spec())
            .await
            .expect_err("rest container fails to create");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(_)
        ));
    }

    #[tokio::test]
    async fn spawn_times_out_if_a_container_never_becomes_healthy() {
        // "db" specifically: it's the only one of the five with a real HEALTHCHECK configured
        // (see `build_specs`) — the others are waited on via `requires_healthcheck: false`, so
        // simulating them as `Unhealthy` wouldn't actually block readiness (see
        // `backends::health_wait`'s own doc comment for why that's by design, not an oversight).
        let runtime = FakeContainerRuntime {
            published_port: 5432,
            unhealthy_container: Some("db"),
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = backend(runtime, dir.path().to_path_buf());
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        let err = backend
            .spawn(&cluster_id, &worker(), 0, &service_spec())
            .await
            .expect_err("db never becomes healthy");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::HealthCheckTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn teardown_removes_all_five_containers_then_the_network() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            published_port: 5432,
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SupabaseBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());

        backend
            .teardown(&cluster_id)
            .await
            .expect("teardown succeeds");

        let removed = runtime.removed.lock().expect("lock");
        assert_eq!(removed.len(), 5);
        let networks_removed = runtime.networks_removed.lock().expect("lock");
        assert_eq!(networks_removed.len(), 1);
        assert_eq!(
            networks_removed[0],
            NetworkHandle::new(network_name(&cluster_id))
        );
    }

    #[tokio::test]
    async fn is_alive_true_when_every_container_is_running() {
        let backend = backend(
            FakeContainerRuntime {
                published_port: 5432,
                ..Default::default()
            },
            std::env::temp_dir(),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());
        assert!(
            backend
                .is_alive(&cluster_id)
                .await
                .expect("inspect succeeds")
        );
    }

    #[derive(Default)]
    struct AllNotFoundRuntime;

    #[async_trait]
    impl ContainerRuntime for AllNotFoundRuntime {
        async fn create_and_start(
            &self,
            _spec: &ContainerSpec,
        ) -> Result<ContainerHandle, DockerError> {
            unreachable!("is_alive tests never create containers")
        }

        async fn inspect(&self, _handle: &ContainerHandle) -> Result<ContainerStatus, DockerError> {
            Ok(ContainerStatus::NotFound)
        }

        async fn stop_and_remove(&self, _handle: &ContainerHandle) -> Result<(), DockerError> {
            unreachable!("is_alive tests never remove containers")
        }

        async fn create_network(&self, name: &str) -> Result<NetworkHandle, DockerError> {
            Ok(NetworkHandle::new(name))
        }

        async fn remove_network(&self, _handle: &NetworkHandle) -> Result<(), DockerError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn is_alive_false_when_a_container_is_not_found() {
        let backend = SupabaseBackend::new(
            std::sync::Arc::new(AllNotFoundRuntime),
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            "postgrest/postgrest:v12".to_string(),
            "supabase/gotrue:v2".to_string(),
            "kong:3".to_string(),
            "supabase/edge-runtime:v1".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            std::env::temp_dir(),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());
        assert!(
            !backend
                .is_alive(&cluster_id)
                .await
                .expect("inspect succeeds")
        );
    }
}
