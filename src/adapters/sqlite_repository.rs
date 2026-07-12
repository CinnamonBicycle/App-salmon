//! `ClusterRepository` via `SQLite` (through `rusqlite`, which is synchronous — every operation
//! runs inside `spawn_blocking` so it never blocks a tokio worker thread).
//!
//! Persists a JSON blob per row rather than one column per field: `ClusterState` has a different
//! shape per variant, and a hand-written tagged-enum JSON mirror (`PersistedState` below)
//! preserves "illegal states unrepresentable" in the storage format too — a row can't be
//! deserialized into `Ready` without also having a `connection`, the way a wide table with
//! nullable columns could drift out of sync.
//!
//! `ConnectionInfo::password` is `Redacted<String>`, which deliberately does not implement
//! `Serialize` (see `redacted.rs`) so it can't leak into logs/responses by accident. Persisting
//! it durably is a deliberate exception, made explicit here via `.expose()` in the hand-written
//! `PersistedConnection` conversion, rather than by giving `Redacted<T>` a blanket `Serialize`
//! impl that every other caller would also inherit.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::domain::cluster::{Cluster, ClusterState, DeleteReason};
use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
use crate::domain::service_kind::{ConnectionInfo, ServiceSpec};
use crate::ports::repository::{ClusterRepository, InsertOutcome, RepositoryError};
use crate::redacted::Redacted;

/// JSON mirror of [`ConnectionInfo`] for on-disk storage. Exists as a separate type (rather than
/// deriving `Serialize`/`Deserialize` on `ConnectionInfo` itself) because `password` there is
/// `Redacted<String>`, which deliberately has no `Serialize` impl — persisting the plaintext
/// password is a conscious, narrow exception made explicit via `.expose()` in the `From` impls
/// below, not something every caller of `ConnectionInfo` should be able to do by accident.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedConnection {
    /// Host to connect to — currently always `127.0.0.1`, since containers are only ever
    /// published to the loopback interface.
    host: String,
    /// Host-side port the container's Postgres is published on.
    port: u16,
    /// Database name to connect to.
    dbname: String,
    /// Database role to connect as.
    user: String,
    /// The plaintext database password. Stored durably as a deliberate exception to
    /// `Redacted<T>`'s no-`Serialize` rule — see the type-level doc comment above.
    password: String,
}

impl From<&ConnectionInfo> for PersistedConnection {
    /// Converts a live [`ConnectionInfo`] into its persistable form, unwrapping the redacted
    /// password so it can be serialized to disk.
    ///
    /// # Arguments
    ///
    /// - `connection`: the connection info to snapshot for storage.
    ///
    /// # Returns
    ///
    /// A [`PersistedConnection`] holding a clone of every field, with the password exposed as
    /// plaintext.
    fn from(connection: &ConnectionInfo) -> Self {
        Self {
            host: connection.host.clone(),
            port: connection.port,
            dbname: connection.dbname.clone(),
            user: connection.user.clone(),
            password: connection.password.expose().clone(),
        }
    }
}

impl From<PersistedConnection> for ConnectionInfo {
    /// Converts a row read back from storage into the live [`ConnectionInfo`] type, re-wrapping
    /// the plaintext password in [`Redacted`] so it goes back to being leak-resistant in memory.
    ///
    /// # Arguments
    ///
    /// - `persisted`: the persisted form read back from the database.
    ///
    /// # Returns
    ///
    /// The equivalent [`ConnectionInfo`], with `password` wrapped in `Redacted::new`.
    fn from(persisted: PersistedConnection) -> Self {
        Self {
            host: persisted.host,
            port: persisted.port,
            dbname: persisted.dbname,
            user: persisted.user,
            password: Redacted::new(persisted.password),
        }
    }
}

/// JSON mirror of [`ClusterState`], tagged by `status` so each variant only carries the fields
/// that make sense for it — a row can't deserialize into `Ready` without also having a
/// `connection`, the way a wide table with nullable columns could drift out of sync.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PersistedState {
    /// Mirrors [`ClusterState::Spawning`]: a background spawn task is still working on this
    /// cluster.
    Spawning {
        /// When the spawn began.
        started_at: DateTime<Utc>,
    },
    /// Mirrors [`ClusterState::Ready`]: the cluster is up and its connection details are known.
    Ready {
        /// When the cluster became ready.
        ready_at: DateTime<Utc>,
        /// When the cluster is due to be reaped (`ready_at` + the requested TTL).
        decommission_at: DateTime<Utc>,
        /// How to connect to the cluster.
        connection: PersistedConnection,
    },
    /// Mirrors [`ClusterState::Failed`]: the spawn failed and the cluster is unusable.
    Failed {
        /// When the failure was recorded.
        failed_at: DateTime<Utc>,
        /// Sanitized, human-readable summary of what went wrong.
        error_summary: String,
    },
    /// Mirrors [`ClusterState::Deleting`]: the cluster is being torn down.
    Deleting {
        /// When the deletion was requested/began.
        deleting_since: DateTime<Utc>,
        /// Why the cluster is being deleted.
        reason: DeleteReason,
    },
}

impl From<&ClusterState> for PersistedState {
    /// Converts a live [`ClusterState`] into its persistable, JSON-taggable form.
    ///
    /// # Arguments
    ///
    /// - `state`: the state to snapshot for storage.
    ///
    /// # Returns
    ///
    /// The matching [`PersistedState`] variant, with all fields cloned/copied from `state`.
    fn from(state: &ClusterState) -> Self {
        match state {
            ClusterState::Spawning { started_at } => PersistedState::Spawning {
                started_at: *started_at,
            },
            ClusterState::Ready {
                ready_at,
                decommission_at,
                connection,
            } => PersistedState::Ready {
                ready_at: *ready_at,
                decommission_at: *decommission_at,
                connection: connection.into(),
            },
            ClusterState::Failed {
                failed_at,
                error_summary,
            } => PersistedState::Failed {
                failed_at: *failed_at,
                error_summary: error_summary.clone(),
            },
            ClusterState::Deleting {
                deleting_since,
                reason,
            } => PersistedState::Deleting {
                deleting_since: *deleting_since,
                reason: *reason,
            },
        }
    }
}

impl From<PersistedState> for ClusterState {
    /// Converts a row read back from storage into the live [`ClusterState`] type.
    ///
    /// # Arguments
    ///
    /// - `persisted`: the persisted form read back from the database.
    ///
    /// # Returns
    ///
    /// The matching [`ClusterState`] variant.
    fn from(persisted: PersistedState) -> Self {
        match persisted {
            PersistedState::Spawning { started_at } => ClusterState::Spawning { started_at },
            PersistedState::Ready {
                ready_at,
                decommission_at,
                connection,
            } => ClusterState::Ready {
                ready_at,
                decommission_at,
                connection: connection.into(),
            },
            PersistedState::Failed {
                failed_at,
                error_summary,
            } => ClusterState::Failed {
                failed_at,
                error_summary,
            },
            PersistedState::Deleting {
                deleting_since,
                reason,
            } => ClusterState::Deleting {
                deleting_since,
                reason,
            },
        }
    }
}

/// JSON mirror of [`WorkerUser`] for on-disk storage.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedWorker {
    /// The worker account's Unix username (e.g. `salmon-worker-03`).
    name: String,
    /// The worker account's numeric user id.
    uid: u32,
    /// The worker account's numeric group id.
    gid: u32,
}

impl From<&WorkerUser> for PersistedWorker {
    /// Converts a live [`WorkerUser`] into its persistable form.
    ///
    /// # Arguments
    ///
    /// - `worker`: the worker to snapshot for storage.
    ///
    /// # Returns
    ///
    /// A [`PersistedWorker`] holding the worker's name/uid/gid.
    fn from(worker: &WorkerUser) -> Self {
        Self {
            name: worker.as_str().to_string(),
            uid: worker.uid(),
            gid: worker.gid(),
        }
    }
}

impl From<PersistedWorker> for WorkerUser {
    /// Converts a row read back from storage into the live [`WorkerUser`] type.
    ///
    /// # Arguments
    ///
    /// - `persisted`: the persisted form read back from the database.
    ///
    /// # Returns
    ///
    /// The equivalent [`WorkerUser`].
    fn from(persisted: PersistedWorker) -> Self {
        WorkerUser::new(persisted.name, persisted.uid, persisted.gid)
    }
}

/// JSON mirror of [`Cluster`] for on-disk storage — everything about a cluster except its `id`
/// and `owner`, which live in their own indexed columns rather than inside the JSON blob (see
/// [`run_migrations`]).
#[derive(Debug, Serialize, Deserialize)]
struct PersistedCluster {
    /// Which service kind this cluster is, and its service-specific options.
    service: ServiceSpec,
    /// The TTL the caller originally requested, in whole seconds (`chrono::TimeDelta` itself
    /// isn't easily `Serialize`-friendly across all its precision, so this stores just the
    /// seconds count that `Cluster::requested_ttl` is reconstructed from).
    requested_ttl_secs: i64,
    /// When the cluster was originally requested.
    requested_at: DateTime<Utc>,
    /// The cluster's current lifecycle state.
    state: PersistedState,
    /// The worker account allocated to this cluster, if one has been assigned yet.
    worker: Option<PersistedWorker>,
}

/// Builds the persistable snapshot of a live [`Cluster`], ready to be JSON-serialized into the
/// `data_json` column.
///
/// # Arguments
///
/// - `cluster`: the cluster to snapshot for storage.
///
/// # Returns
///
/// A [`PersistedCluster`] with every field derived from `cluster` (its `id`/`owner` are dropped,
/// since those are stored in their own columns instead).
fn to_persisted(cluster: &Cluster) -> PersistedCluster {
    PersistedCluster {
        service: cluster.service.clone(),
        requested_ttl_secs: cluster.requested_ttl.num_seconds(),
        requested_at: cluster.requested_at,
        state: PersistedState::from(&cluster.state),
        worker: cluster.worker.as_ref().map(PersistedWorker::from),
    }
}

/// Reassembles a [`Cluster`] from a stored row's `id`/`owner` columns plus its `data_json` blob.
///
/// # Arguments
///
/// - `id`: the cluster's id, read from the `id` column.
/// - `owner`: the cluster's owner, read from the `owner` column.
/// - `json`: the raw `data_json` column contents to deserialize.
///
/// # Returns
///
/// The reassembled [`Cluster`].
///
/// # Errors
///
/// Returns [`RepositoryError::Serde`] if `json` doesn't deserialize into a [`PersistedCluster`].
fn from_row(id: ClusterId, owner: ClientId, json: &str) -> Result<Cluster, RepositoryError> {
    let persisted: PersistedCluster = serde_json::from_str(json).map_err(RepositoryError::Serde)?;
    Ok(Cluster {
        id,
        owner,
        service: persisted.service,
        requested_ttl: TimeDelta::seconds(persisted.requested_ttl_secs),
        requested_at: persisted.requested_at,
        state: persisted.state.into(),
        worker: persisted.worker.map(WorkerUser::from),
    })
}

/// Parses the `id` column's text back into a [`ClusterId`]. Only needed by the multi-row queries
/// (`list_by_owner`/`list_all`), which read `id` as a plain column rather than being handed it by
/// the caller the way `get_owned`/`get_any` are.
///
/// # Arguments
///
/// - `raw`: the raw `id` column text, expected to be a valid ULID string.
///
/// # Returns
///
/// The parsed [`ClusterId`].
///
/// # Errors
///
/// Returns [`RepositoryError::CorruptRow`] if `raw` isn't a valid ULID — since this crate is the
/// only writer of this column, that should only be reachable via external tampering or a prior
/// bug.
fn parse_cluster_id(raw: &str) -> Result<ClusterId, RepositoryError> {
    ulid::Ulid::from_string(raw)
        .map(ClusterId::new)
        .map_err(|source| {
            RepositoryError::CorruptRow(format!("invalid cluster id '{raw}': {source}"))
        })
}

/// Creates the `clusters` table and its owner index if they don't already exist. Idempotent, so
/// it's safe to run on every [`SqliteClusterRepository::open`] call rather than needing a
/// separate one-time-setup step.
///
/// # Arguments
///
/// - `conn`: the connection to run the migration against.
///
/// # Errors
///
/// Returns [`RepositoryError::Db`] if the `CREATE TABLE`/`CREATE INDEX` statements fail.
fn run_migrations(conn: &Connection) -> Result<(), RepositoryError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clusters (
            id TEXT PRIMARY KEY,
            owner TEXT NOT NULL,
            data_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_clusters_owner ON clusters(owner);",
    )
    .map_err(RepositoryError::Db)
}

/// The real, `SQLite`-backed [`ClusterRepository`] implementation.
pub struct SqliteClusterRepository {
    /// The underlying connection, behind a `Mutex` (since `rusqlite::Connection` isn't `Sync`) and
    /// an `Arc` (so it can be cheaply cloned into the `spawn_blocking` closures each operation
    /// runs inside — see [`SqliteClusterRepository::with_conn`]).
    conn: Arc<Mutex<Connection>>,
}

impl SqliteClusterRepository {
    /// Opens (creating if necessary) a `SQLite` database file at `path` and runs migrations
    /// against it.
    ///
    /// # Arguments
    ///
    /// - `path`: filesystem path to the database file.
    ///
    /// # Returns
    ///
    /// A ready-to-use repository backed by the opened connection.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`] if the file can't be opened or migrations fail to apply,
    /// or [`RepositoryError::TaskJoin`] if the `spawn_blocking` task itself fails to complete.
    pub async fn open(path: &Path) -> Result<Self, RepositoryError> {
        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, RepositoryError> {
            let conn = Connection::open(path).map_err(RepositoryError::Db)?;
            run_migrations(&conn)?;
            Ok(conn)
        })
        .await
        .map_err(|source| RepositoryError::TaskJoin(source.to_string()))??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    async fn open_in_memory() -> Result<Self, RepositoryError> {
        let conn = tokio::task::spawn_blocking(|| -> Result<Connection, RepositoryError> {
            let conn = Connection::open_in_memory().map_err(RepositoryError::Db)?;
            run_migrations(&conn)?;
            Ok(conn)
        })
        .await
        .map_err(|source| RepositoryError::TaskJoin(source.to_string()))??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Test-only escape hatch to insert an intentionally corrupt row, for exercising
    /// [`RepositoryError::Serde`]/[`RepositoryError::CorruptRow`] paths.
    #[cfg(test)]
    async fn insert_raw(
        &self,
        id: &str,
        owner: &str,
        data_json: &str,
    ) -> Result<(), RepositoryError> {
        let id = id.to_string();
        let owner = owner.to_string();
        let data_json = data_json.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO clusters (id, owner, data_json) VALUES (?1, ?2, ?3)",
                params![id, owner, data_json],
            )
            .map_err(RepositoryError::Db)?;
            Ok(())
        })
        .await
    }

    /// Runs a synchronous closure against the underlying connection inside `spawn_blocking`, so
    /// callers can use `rusqlite`'s blocking API without ever blocking a tokio worker thread.
    /// Every [`ClusterRepository`] method on this type is a thin wrapper around a call to this.
    ///
    /// # Arguments
    ///
    /// - `f`: the synchronous operation to run against the connection. Takes `&mut Connection`
    ///   (rather than `&Connection`) since starting a transaction requires a mutable borrow.
    ///
    /// # Returns
    ///
    /// Whatever `f` returns, unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::TaskJoin`] if the connection's mutex is poisoned (only possible
    /// if a prior call's closure panicked, which none of ours do) or if the `spawn_blocking` task
    /// itself fails to complete. Otherwise propagates whatever error `f` itself returns.
    async fn with_conn<T, F>(&self, f: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_poisoned| {
                RepositoryError::TaskJoin("connection mutex poisoned".to_string())
            })?;
            f(&mut guard)
        })
        .await
        .map_err(|source| RepositoryError::TaskJoin(source.to_string()))?
    }
}

#[async_trait]
impl ClusterRepository for SqliteClusterRepository {
    /// Implements [`ClusterRepository::try_insert_if_under_quota`] as a single `SQLite`
    /// transaction: count `owner`'s existing rows, and only insert if under `limit`, so two
    /// concurrent calls for the same owner can't both observe "under quota" and both succeed.
    ///
    /// # Arguments
    ///
    /// - `cluster`: the cluster to insert if the owner is under quota.
    /// - `limit`: the maximum number of rows `cluster.owner` may have.
    ///
    /// # Returns
    ///
    /// [`InsertOutcome::Inserted`] if the row was inserted, or
    /// [`InsertOutcome::QuotaExceeded`] (carrying the owner's current row count) if it wasn't.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Serde`] if `cluster` can't be serialized, or
    /// [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] (via
    /// [`SqliteClusterRepository::with_conn`]) if the transaction itself fails.
    async fn try_insert_if_under_quota(
        &self,
        cluster: &Cluster,
        limit: u32,
    ) -> Result<InsertOutcome, RepositoryError> {
        let id = cluster.id.to_string();
        let owner = cluster.owner.to_string();
        let json = serde_json::to_string(&to_persisted(cluster)).map_err(RepositoryError::Serde)?;

        self.with_conn(move |conn| {
            let tx = conn.transaction().map_err(RepositoryError::Db)?;
            let count: u32 = tx
                .query_row(
                    "SELECT COUNT(*) FROM clusters WHERE owner = ?1",
                    params![owner],
                    |row| row.get(0),
                )
                .map_err(RepositoryError::Db)?;
            if count >= limit {
                return Ok(InsertOutcome::QuotaExceeded {
                    current_count: count,
                });
            }
            tx.execute(
                "INSERT INTO clusters (id, owner, data_json) VALUES (?1, ?2, ?3)",
                params![id, owner, json],
            )
            .map_err(RepositoryError::Db)?;
            tx.commit().map_err(RepositoryError::Db)?;
            Ok(InsertOutcome::Inserted)
        })
        .await
    }

    /// Implements [`ClusterRepository::get_owned`] as a query filtered by both `id` and `owner`
    /// in the same `WHERE` clause.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to look up.
    /// - `owner`: the expected owner; a row that exists but belongs to someone else is treated
    ///   the same as no row at all.
    ///
    /// # Returns
    ///
    /// `Some(cluster)` if a matching row exists, `None` otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] if the query fails, or
    /// [`RepositoryError::Serde`] (via [`from_row`]) if the stored JSON is corrupt.
    async fn get_owned(
        &self,
        id: &ClusterId,
        owner: &ClientId,
    ) -> Result<Option<Cluster>, RepositoryError> {
        let id_str = id.to_string();
        let owner_str = owner.to_string();
        let id = *id;
        let owner = owner.clone();
        self.with_conn(move |conn| {
            let json: Option<String> = conn
                .query_row(
                    "SELECT data_json FROM clusters WHERE id = ?1 AND owner = ?2",
                    params![id_str, owner_str],
                    |row| row.get(0),
                )
                .optional()
                .map_err(RepositoryError::Db)?;
            json.map(|json| from_row(id, owner, &json)).transpose()
        })
        .await
    }

    /// Implements [`ClusterRepository::get_any`] as an unscoped-by-owner lookup.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to look up.
    ///
    /// # Returns
    ///
    /// `Some(cluster)` if a row with this id exists (regardless of owner), `None` otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] if the query fails, or
    /// [`RepositoryError::Serde`] (via [`from_row`]) if the stored JSON is corrupt.
    async fn get_any(&self, id: &ClusterId) -> Result<Option<Cluster>, RepositoryError> {
        let id_str = id.to_string();
        let id = *id;
        self.with_conn(move |conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT owner, data_json FROM clusters WHERE id = ?1",
                    params![id_str],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(RepositoryError::Db)?;
            row.map(|(owner, json)| from_row(id, ClientId::new(owner), &json))
                .transpose()
        })
        .await
    }

    /// Implements [`ClusterRepository::list_by_owner`] as a query filtered by `owner`.
    ///
    /// # Arguments
    ///
    /// - `owner`: the owner to list clusters for.
    ///
    /// # Returns
    ///
    /// Every row belonging to `owner`, in no particular order. Empty if `owner` has none.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] if the query fails,
    /// [`RepositoryError::CorruptRow`] (via [`parse_cluster_id`]) if a row's `id` column isn't a
    /// valid ULID, or [`RepositoryError::Serde`] (via [`from_row`]) if a row's JSON is corrupt.
    async fn list_by_owner(&self, owner: &ClientId) -> Result<Vec<Cluster>, RepositoryError> {
        let owner_str = owner.to_string();
        let owner = owner.clone();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT id, data_json FROM clusters WHERE owner = ?1")
                .map_err(RepositoryError::Db)?;
            let rows = stmt
                .query_map(params![owner_str], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(RepositoryError::Db)?;
            let mut clusters = Vec::new();
            for row in rows {
                let (id_str, json) = row.map_err(RepositoryError::Db)?;
                let id = parse_cluster_id(&id_str)?;
                clusters.push(from_row(id, owner.clone(), &json)?);
            }
            Ok(clusters)
        })
        .await
    }

    /// Implements [`ClusterRepository::list_all`] as an unfiltered query over every row.
    ///
    /// # Returns
    ///
    /// Every persisted row regardless of owner, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] if the query fails,
    /// [`RepositoryError::CorruptRow`] (via [`parse_cluster_id`]) if a row's `id` column isn't a
    /// valid ULID, or [`RepositoryError::Serde`] (via [`from_row`]) if a row's JSON is corrupt.
    async fn list_all(&self) -> Result<Vec<Cluster>, RepositoryError> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, owner, data_json FROM clusters")
                .map_err(RepositoryError::Db)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(RepositoryError::Db)?;
            let mut clusters = Vec::new();
            for row in rows {
                let (id_str, owner_str, json) = row.map_err(RepositoryError::Db)?;
                let id = parse_cluster_id(&id_str)?;
                clusters.push(from_row(id, ClientId::new(owner_str), &json)?);
            }
            Ok(clusters)
        })
        .await
    }

    /// Implements [`ClusterRepository::update_state`] as a read-modify-write of the row's JSON
    /// blob: reads the existing `data_json`, replaces just the `state` field, and writes it back.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to update.
    /// - `state`: the new state to persist.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] if no row with `id` exists or
    /// the query/update fails, or [`RepositoryError::Serde`] if the existing JSON can't be parsed
    /// or the updated value can't be re-serialized.
    async fn update_state(
        &self,
        id: &ClusterId,
        state: &ClusterState,
    ) -> Result<(), RepositoryError> {
        let id_str = id.to_string();
        let new_state = PersistedState::from(state);
        self.with_conn(move |conn| {
            let existing: String = conn
                .query_row(
                    "SELECT data_json FROM clusters WHERE id = ?1",
                    params![id_str],
                    |row| row.get(0),
                )
                .map_err(RepositoryError::Db)?;
            let mut persisted: PersistedCluster =
                serde_json::from_str(&existing).map_err(RepositoryError::Serde)?;
            persisted.state = new_state;
            let updated_json = serde_json::to_string(&persisted).map_err(RepositoryError::Serde)?;
            conn.execute(
                "UPDATE clusters SET data_json = ?1 WHERE id = ?2",
                params![updated_json, id_str],
            )
            .map_err(RepositoryError::Db)?;
            Ok(())
        })
        .await
    }

    /// Implements [`ClusterRepository::set_worker`] as a read-modify-write of the row's JSON
    /// blob: reads the existing `data_json`, replaces just the `worker` field, and writes it back.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to update.
    /// - `worker`: the worker account now allocated to it.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] if no row with `id` exists or
    /// the query/update fails, or [`RepositoryError::Serde`] if the existing JSON can't be parsed
    /// or the updated value can't be re-serialized.
    async fn set_worker(&self, id: &ClusterId, worker: &WorkerUser) -> Result<(), RepositoryError> {
        let id_str = id.to_string();
        let new_worker = PersistedWorker::from(worker);
        self.with_conn(move |conn| {
            let existing: String = conn
                .query_row(
                    "SELECT data_json FROM clusters WHERE id = ?1",
                    params![id_str],
                    |row| row.get(0),
                )
                .map_err(RepositoryError::Db)?;
            let mut persisted: PersistedCluster =
                serde_json::from_str(&existing).map_err(RepositoryError::Serde)?;
            persisted.worker = Some(new_worker);
            let updated_json = serde_json::to_string(&persisted).map_err(RepositoryError::Serde)?;
            conn.execute(
                "UPDATE clusters SET data_json = ?1 WHERE id = ?2",
                params![updated_json, id_str],
            )
            .map_err(RepositoryError::Db)?;
            Ok(())
        })
        .await
    }

    /// Implements [`ClusterRepository::delete`]. Idempotent: deleting an id with no matching row
    /// is not an error.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to remove.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::Db`]/[`RepositoryError::TaskJoin`] if the delete statement
    /// itself fails.
    async fn delete(&self, id: &ClusterId) -> Result<(), RepositoryError> {
        let id_str = id.to_string();
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM clusters WHERE id = ?1", params![id_str])
                .map_err(RepositoryError::Db)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::SqliteClusterRepository;
    use crate::domain::cluster::{Cluster, ClusterState};
    use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
    use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};
    use crate::ports::repository::{ClusterRepository, InsertOutcome, RepositoryError};
    use crate::redacted::Redacted;
    use chrono::{TimeDelta, Utc};

    fn sample_cluster(owner: &str) -> Cluster {
        Cluster {
            id: ClusterId::new(ulid::Ulid::r#gen()),
            owner: ClientId::new(owner),
            service: ServiceSpec {
                kind: ServiceKind::Postgres,
                pgvector: true,
            },
            requested_ttl: TimeDelta::seconds(300),
            requested_at: Utc::now(),
            state: ClusterState::Spawning {
                started_at: Utc::now(),
            },
            worker: None,
        }
    }

    fn ready_connection() -> ConnectionInfo {
        ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 55432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: Redacted::new("s3cret-password".to_string()),
        }
    }

    #[tokio::test]
    async fn insert_then_get_owned_round_trips() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        let outcome = repo
            .try_insert_if_under_quota(&cluster, 2)
            .await
            .expect("insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        let fetched = repo
            .get_owned(&cluster.id, &cluster.owner)
            .await
            .expect("query")
            .expect("row present");
        assert_eq!(fetched.id, cluster.id);
        assert_eq!(fetched.owner, cluster.owner);
        assert!(fetched.service.pgvector);
    }

    #[tokio::test]
    async fn get_owned_returns_none_for_wrong_owner() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 2)
            .await
            .expect("insert");

        let result = repo
            .get_owned(&cluster.id, &ClientId::new("agent-b"))
            .await
            .expect("query");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_owned_returns_none_for_unknown_id() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let result = repo
            .get_owned(
                &ClusterId::new(ulid::Ulid::r#gen()),
                &ClientId::new("agent-a"),
            )
            .await
            .expect("query");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_any_ignores_owner() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 2)
            .await
            .expect("insert");

        let fetched = repo
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert_eq!(fetched.owner, ClientId::new("agent-a"));
    }

    #[tokio::test]
    async fn quota_enforced_atomically_at_the_boundary() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let owner = "agent-a";
        let first = repo
            .try_insert_if_under_quota(&sample_cluster(owner), 2)
            .await
            .expect("insert 1");
        let second = repo
            .try_insert_if_under_quota(&sample_cluster(owner), 2)
            .await
            .expect("insert 2");
        let third = repo
            .try_insert_if_under_quota(&sample_cluster(owner), 2)
            .await
            .expect("insert 3");

        assert_eq!(first, InsertOutcome::Inserted);
        assert_eq!(second, InsertOutcome::Inserted);
        assert_eq!(third, InsertOutcome::QuotaExceeded { current_count: 2 });

        let remaining = repo
            .list_by_owner(&ClientId::new(owner))
            .await
            .expect("list");
        assert_eq!(remaining.len(), 2);
    }

    #[tokio::test]
    async fn quota_holds_under_concurrent_creates() {
        // Fires 6 concurrent inserts against a limit of 2 for the same owner — this is the
        // TOCTOU race the atomic check-then-insert transaction exists to close. If the
        // check-then-insert weren't atomic, more than 2 could observe "under quota" and succeed.
        let repo = std::sync::Arc::new(
            SqliteClusterRepository::open_in_memory()
                .await
                .expect("open"),
        );
        let owner = "agent-concurrent";
        let mut handles = Vec::new();
        for _ in 0..6 {
            let repo = repo.clone();
            let cluster = sample_cluster(owner);
            handles.push(tokio::spawn(async move {
                repo.try_insert_if_under_quota(&cluster, 2).await
            }));
        }

        let mut inserted = 0;
        let mut rejected = 0;
        for handle in handles {
            match handle.await.expect("task completes").expect("no db error") {
                InsertOutcome::Inserted => inserted += 1,
                InsertOutcome::QuotaExceeded { .. } => rejected += 1,
            }
        }

        assert_eq!(inserted, 2);
        assert_eq!(rejected, 4);
    }

    #[tokio::test]
    async fn list_by_owner_filters_correctly() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        repo.try_insert_if_under_quota(&sample_cluster("agent-a"), 5)
            .await
            .expect("insert");
        repo.try_insert_if_under_quota(&sample_cluster("agent-a"), 5)
            .await
            .expect("insert");
        repo.try_insert_if_under_quota(&sample_cluster("agent-b"), 5)
            .await
            .expect("insert");

        let a_clusters = repo
            .list_by_owner(&ClientId::new("agent-a"))
            .await
            .expect("list");
        let b_clusters = repo
            .list_by_owner(&ClientId::new("agent-b"))
            .await
            .expect("list");
        assert_eq!(a_clusters.len(), 2);
        assert_eq!(b_clusters.len(), 1);
    }

    #[tokio::test]
    async fn list_all_returns_rows_across_owners() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        repo.try_insert_if_under_quota(&sample_cluster("agent-a"), 5)
            .await
            .expect("insert");
        repo.try_insert_if_under_quota(&sample_cluster("agent-b"), 5)
            .await
            .expect("insert");

        let all = repo.list_all().await.expect("list");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn update_state_changes_state_and_preserves_other_fields() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 5)
            .await
            .expect("insert");

        let ready_at = Utc::now();
        let decommission_at = ready_at + TimeDelta::seconds(300);
        let connection = ready_connection();
        repo.update_state(
            &cluster.id,
            &ClusterState::Ready {
                ready_at,
                decommission_at,
                connection: connection.clone(),
            },
        )
        .await
        .expect("update state");

        let updated = repo
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(updated.service.pgvector);
        match updated.state {
            ClusterState::Ready {
                connection: fetched_connection,
                ..
            } => {
                assert_eq!(
                    fetched_connection.password.expose(),
                    connection.password.expose()
                );
                assert_eq!(fetched_connection.port, connection.port);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_state_round_trips_failed_state() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 5)
            .await
            .expect("insert");

        repo.update_state(
            &cluster.id,
            &ClusterState::Failed {
                failed_at: Utc::now(),
                error_summary: "container did not become healthy in time".to_string(),
            },
        )
        .await
        .expect("update state");

        let updated = repo
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        match updated.state {
            ClusterState::Failed { error_summary, .. } => {
                assert_eq!(error_summary, "container did not become healthy in time");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_state_round_trips_deleting_state() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 5)
            .await
            .expect("insert");

        repo.update_state(
            &cluster.id,
            &ClusterState::Deleting {
                deleting_since: Utc::now(),
                reason: crate::domain::cluster::DeleteReason::TtlExpired,
            },
        )
        .await
        .expect("update state");

        let updated = repo
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        match updated.state {
            ClusterState::Deleting { reason, .. } => {
                assert_eq!(reason, crate::domain::cluster::DeleteReason::TtlExpired);
            }
            other => panic!("expected Deleting, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_creates_and_uses_a_real_sqlite_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let repo = SqliteClusterRepository::open(&path)
            .await
            .expect("open real file");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 5)
            .await
            .expect("insert");

        assert!(
            path.exists(),
            "sqlite file should have been created on disk"
        );
        let fetched = repo
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert_eq!(fetched.id, cluster.id);
    }

    #[tokio::test]
    async fn set_worker_persists_worker_assignment() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 5)
            .await
            .expect("insert");

        let worker = WorkerUser::new("salmon-worker-03", 2003, 2003);
        repo.set_worker(&cluster.id, &worker)
            .await
            .expect("set worker");

        let updated = repo
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert_eq!(updated.worker, Some(worker));
    }

    #[tokio::test]
    async fn delete_removes_the_row() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let cluster = sample_cluster("agent-a");
        repo.try_insert_if_under_quota(&cluster, 5)
            .await
            .expect("insert");

        repo.delete(&cluster.id).await.expect("delete");
        let result = repo.get_any(&cluster.id).await.expect("query");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn corrupt_json_payload_surfaces_as_serde_error() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let id = ClusterId::new(ulid::Ulid::r#gen());
        repo.insert_raw(&id.to_string(), "agent-a", "not valid json")
            .await
            .expect("raw insert");

        let err = repo.get_any(&id).await.expect_err("corrupt payload");
        assert!(matches!(err, RepositoryError::Serde(_)));
    }

    #[tokio::test]
    async fn corrupt_id_column_surfaces_as_corrupt_row_via_list() {
        let repo = SqliteClusterRepository::open_in_memory()
            .await
            .expect("open");
        let persisted = super::to_persisted(&sample_cluster("agent-a"));
        let json = serde_json::to_string(&persisted).expect("serialize");
        repo.insert_raw("not-a-valid-ulid", "agent-a", &json)
            .await
            .expect("raw insert");

        let err = repo.list_all().await.expect_err("corrupt id");
        assert!(matches!(err, RepositoryError::CorruptRow(_)));
    }
}
