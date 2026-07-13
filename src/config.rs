//! TOML configuration, loaded once at startup. Validation failures are [`ConfigError`], never a
//! panic — `main` prints the error and exits non-zero.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::auth::ClientRegistry;
use crate::auth::hashing::SecretHash;
use crate::domain::ids::ClientId;

/// Why loading or validating [`Config`] failed.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file at `path` couldn't be read from disk.
    #[error("failed to read config file {path}: {source}")]
    Read {
        /// The path that was attempted.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The config file at `path` was read but isn't valid TOML matching [`Config`]'s shape.
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        /// The path that was attempted.
        path: PathBuf,
        /// The underlying TOML parse failure.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The config parsed but failed a semantic validation rule (see [`Config::validate`]); the
    /// string describes which rule and why.
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// `[server]` — where the HTTP API listens.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// The address (host:port) the server binds its TCP listener to.
    pub bind_addr: SocketAddr,
}

/// `[limits]` — TTL bounds, quota, and timing knobs enforced on every cluster.
#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    /// The shortest TTL a `POST /clusters` request may request, in seconds.
    pub min_ttl_secs: i64,
    /// The longest TTL a `POST /clusters` request may request, in seconds.
    pub max_ttl_secs: i64,
    /// How many non-absent clusters (any state except deleted) a single owner may hold at once.
    pub max_clusters_per_user: u32,
    /// Display-only estimate of how long a spawn is expected to take, used to compute
    /// `estimated_ready_at` in the `POST /clusters` response — not enforced anywhere.
    pub spawn_estimate_secs: u64,
    /// How long a backend's own readiness poll (see `backends::postgres::wait_until_ready`) waits
    /// before giving up and marking the cluster `Failed`.
    pub health_check_timeout_secs: u64,
    /// How often the TTL reaper sweeps for expired/failed clusters to delete.
    pub ttl_reaper_interval_secs: u64,
    /// How long a `Failed` cluster is kept (so its error is visible via `GET`) before the reaper
    /// deletes it.
    pub failed_cluster_reap_delay_secs: u64,
    /// The largest `POST /clusters` request body accepted on the `multipart/form-data` path (a
    /// Supabase `project_tar` upload), in bytes — see [`crate::http::AppState::max_tar_bytes`].
    /// Every other route keeps axum's built-in 2MB default. Also used as
    /// `tar_validation::TarLimits::max_total_bytes`, the cumulative-extracted-size cap applied
    /// once the tar is actually validated (see `service::spawn_task`) — the same "how big is too
    /// big" number applies at both the wire-upload boundary and the extracted-contents boundary.
    pub max_tar_bytes: usize,
    /// The largest single entry (file) a `project_tar` upload may contain, in bytes — checked
    /// from each entry's header before it's read, bounding decompression-bomb-style abuse. See
    /// `tar_validation::TarLimits::max_entry_bytes`.
    pub max_tar_entry_bytes: u64,
}

/// `[docker]` — how to reach the Docker daemon and what image to run.
#[derive(Debug, Clone, Deserialize)]
pub struct DockerConfig {
    /// Filesystem path to the Docker Engine API's Unix domain socket.
    pub socket_path: String,
    /// The Postgres/pgvector image reference to run for each cluster.
    pub postgres_image: String,
}

/// `[storage]` — where durable cluster state is persisted.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// Filesystem path to the `SQLite` database file backing [`crate::ports::repository::ClusterRepository`].
    pub sqlite_path: PathBuf,
}

/// `[logging]` — where logs go and how long they're kept.
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    /// Directory `tracing-appender` writes daily-rotated log files into.
    pub log_dir: PathBuf,
    /// Whether to emit structured JSON logs (in addition to the always-on human-readable stderr
    /// layer).
    pub json: bool,
    /// How many days a compressed (`.gz`) log archive is kept before `service::log_rotation`
    /// deletes it.
    pub retention_days: u32,
    /// How many days a plain (uncompressed) log file is kept before `service::log_rotation`
    /// gzips it.
    pub compress_after_days: u32,
}

/// One `[[clients]]` entry — a caller authorized to use the API.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientConfig {
    /// The client's name, used as the `<client_name>` half of the `Bearer <name>:<secret>`
    /// `Authorization` header.
    pub name: String,
    /// The SHA-256 hash of the client's bearer secret, in `sha256:<64 hex chars>` form (see
    /// [`crate::auth::hashing::SecretHash::from_hex_with_prefix`]).
    pub secret_hash: String,
    /// The pre-provisioned Unix account this client's clusters run as (see
    /// `client_workers::ClientWorkers`) — must exist in `/etc/passwd`, and must not be shared
    /// with any other `[[clients]]` entry (see [`Config::validate`]).
    pub unix_user: String,
}

/// The full parsed contents of `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `[server]` settings.
    pub server: ServerConfig,
    /// `[limits]` settings.
    pub limits: LimitsConfig,
    /// `[docker]` settings.
    pub docker: DockerConfig,
    /// `[storage]` settings.
    pub storage: StorageConfig,
    /// `[logging]` settings.
    pub logging: LoggingConfig,
    /// Every `[[clients]]` entry; defaults to empty if the key is absent (then rejected by
    /// [`Config::validate`], since at least one client is required).
    #[serde(default)]
    pub clients: Vec<ClientConfig>,
}

impl Config {
    /// Reads, parses, and validates the config file at `path`.
    ///
    /// # Arguments
    ///
    /// - `path`: filesystem path to the TOML config file to load.
    ///
    /// # Returns
    ///
    /// The parsed and validated [`Config`].
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] if the file can't be read, [`ConfigError::Parse`] if it isn't valid
    /// TOML matching this shape, [`ConfigError::Invalid`] if it parses but fails validation.
    pub async fn load(path: &Path) -> Result<Config, ConfigError> {
        let raw = tokio::fs::read_to_string(path)
            .await
            .map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        let config: Config = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Checks semantic constraints `serde`'s structural deserialization can't express on its own
    /// (TTL ordering, non-zero quotas, at least one client, well-formed secret hashes, no two
    /// clients sharing a `unix_user`).
    ///
    /// # Returns
    ///
    /// `Ok(())` if every constraint holds.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] describing the first constraint violated, if any.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.limits.min_ttl_secs <= 0 {
            return Err(ConfigError::Invalid(
                "limits.min_ttl_secs must be positive".to_string(),
            ));
        }
        if self.limits.min_ttl_secs >= self.limits.max_ttl_secs {
            return Err(ConfigError::Invalid(
                "limits.min_ttl_secs must be less than limits.max_ttl_secs".to_string(),
            ));
        }
        if self.limits.max_clusters_per_user == 0 {
            return Err(ConfigError::Invalid(
                "limits.max_clusters_per_user must be at least 1".to_string(),
            ));
        }
        if self.limits.max_tar_bytes == 0 {
            return Err(ConfigError::Invalid(
                "limits.max_tar_bytes must be at least 1".to_string(),
            ));
        }
        if self.limits.max_tar_entry_bytes == 0 {
            return Err(ConfigError::Invalid(
                "limits.max_tar_entry_bytes must be at least 1".to_string(),
            ));
        }
        if self.limits.max_tar_entry_bytes > self.limits.max_tar_bytes as u64 {
            return Err(ConfigError::Invalid(
                "limits.max_tar_entry_bytes must not exceed limits.max_tar_bytes".to_string(),
            ));
        }
        if self.clients.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[clients]] entry is required".to_string(),
            ));
        }
        for client in &self.clients {
            if SecretHash::from_hex_with_prefix(&client.secret_hash).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "client '{}' has a malformed secret_hash (expected sha256:<64 hex chars>)",
                    client.name
                )));
            }
        }
        let mut seen_unix_users = std::collections::HashSet::with_capacity(self.clients.len());
        for client in &self.clients {
            if !seen_unix_users.insert(client.unix_user.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "unix_user '{}' is used by more than one [[clients]] entry — each client \
                     must have its own account",
                    client.unix_user
                )));
            }
        }
        Ok(())
    }

    /// Builds the runtime [`ClientRegistry`] from `self.clients`. Only fails if a hash is
    /// malformed, which [`Config::load`]'s validation already rules out — kept as a `Result` (not
    /// infallible) so a `Config` constructed by hand (e.g. in a test) can't silently drop bad
    /// entries.
    ///
    /// # Returns
    ///
    /// A [`ClientRegistry`] populated from every entry in `self.clients`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] if any client's `secret_hash` fails to parse.
    pub fn client_registry(&self) -> Result<ClientRegistry, ConfigError> {
        let mut clients = HashMap::with_capacity(self.clients.len());
        for client in &self.clients {
            let hash = SecretHash::from_hex_with_prefix(&client.secret_hash).ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "client '{}' has a malformed secret_hash (expected sha256:<64 hex chars>)",
                    client.name
                ))
            })?;
            clients.insert(ClientId::new(client.name.clone()), hash);
        }
        Ok(ClientRegistry::new(clients))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    fn valid_toml() -> &'static str {
        r#"
[server]
bind_addr = "127.0.0.1:8843"

[limits]
min_ttl_secs = 30
max_ttl_secs = 3600
max_clusters_per_user = 2
spawn_estimate_secs = 20
health_check_timeout_secs = 60
ttl_reaper_interval_secs = 5
failed_cluster_reap_delay_secs = 5
max_tar_bytes = 52428800
max_tar_entry_bytes = 10485760

[docker]
socket_path = "/var/run/docker.sock"
postgres_image = "pgvector/pgvector:pg16"

[storage]
sqlite_path = "/var/lib/app_salmon/state.sqlite3"

[logging]
log_dir = "/home/app_salmon/.local/share/app_salmon/logs"
json = true
retention_days = 14
compress_after_days = 1

[[clients]]
name = "openbrain-agent"
secret_hash = "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
unix_user = "openbrain-agent"
"#
    }

    fn parse(toml_str: &str) -> Result<Config, super::ConfigError> {
        let config: Config =
            toml::from_str(toml_str).map_err(|source| super::ConfigError::Parse {
                path: "test.toml".into(),
                source: Box::new(source),
            })?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn valid_config_parses_and_validates() {
        let config = parse(valid_toml()).expect("valid config");
        assert_eq!(config.clients.len(), 1);
        assert_eq!(config.clients[0].unix_user, "openbrain-agent");
    }

    #[test]
    fn rejects_min_ttl_not_less_than_max_ttl() {
        let toml_str = valid_toml().replace("min_ttl_secs = 30", "min_ttl_secs = 3600");
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_zero_min_ttl() {
        let toml_str = valid_toml().replace("min_ttl_secs = 30", "min_ttl_secs = 0");
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_zero_max_clusters_per_user() {
        let toml_str =
            valid_toml().replace("max_clusters_per_user = 2", "max_clusters_per_user = 0");
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_zero_max_tar_bytes() {
        let toml_str = valid_toml().replace("max_tar_bytes = 52428800", "max_tar_bytes = 0");
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_zero_max_tar_entry_bytes() {
        let toml_str =
            valid_toml().replace("max_tar_entry_bytes = 10485760", "max_tar_entry_bytes = 0");
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_max_tar_entry_bytes_over_max_tar_bytes() {
        let toml_str = valid_toml().replace(
            "max_tar_entry_bytes = 10485760",
            "max_tar_entry_bytes = 999999999999",
        );
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_two_clients_sharing_a_unix_user() {
        let toml_str = format!(
            "{}\n{}",
            valid_toml(),
            r#"
[[clients]]
name = "openbrain-agent-2"
secret_hash = "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
unix_user = "openbrain-agent"
"#
        );
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_empty_clients_list() {
        let toml_str = valid_toml();
        let without_clients =
            &toml_str[..toml_str.find("[[clients]]").expect("has clients section")];
        let err = parse(without_clients).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_malformed_client_secret_hash() {
        let toml_str = valid_toml().replace(
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "not-a-hash",
        );
        let err = parse(&toml_str).expect_err("invalid");
        assert!(matches!(err, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_malformed_toml() {
        let result: Result<Config, super::ConfigError> =
            toml::from_str("this is not valid toml =====").map_err(|source| {
                super::ConfigError::Parse {
                    path: "test.toml".into(),
                    source: Box::new(source),
                }
            });
        assert!(matches!(result, Err(super::ConfigError::Parse { .. })));
    }

    #[test]
    fn client_registry_builds_from_valid_config() {
        let config = parse(valid_toml()).expect("valid config");
        let registry = config
            .client_registry()
            .expect("valid hashes build a registry");
        let client_id = registry
            .authenticate(Some(
                "Bearer openbrain-agent:this-is-not-the-real-secret-but-auth-will-just-fail",
            ))
            .unwrap_err();
        assert_eq!(client_id, crate::auth::AuthError::InvalidSecret);
    }

    #[tokio::test]
    async fn load_reports_read_error_for_missing_file() {
        let err = Config::load(std::path::Path::new("/nonexistent/app-salmon-config.toml"))
            .await
            .expect_err("missing file");
        assert!(matches!(err, super::ConfigError::Read { .. }));
    }

    #[tokio::test]
    async fn load_reports_parse_error_for_a_real_malformed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        tokio::fs::write(&path, "this is not valid toml =====")
            .await
            .expect("write malformed config");
        let err = Config::load(&path).await.expect_err("malformed file");
        assert!(matches!(err, super::ConfigError::Parse { .. }));
    }

    #[test]
    fn client_registry_rejects_a_malformed_hash_even_if_validate_was_skipped() {
        // Deserializes directly (bypassing `validate`) to exercise `client_registry`'s own
        // malformed-hash check, which is deliberately redundant with `validate`'s.
        let toml_str = valid_toml().replace(
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "not-a-hash",
        );
        let config: Config =
            toml::from_str(&toml_str).expect("parses even though the hash is malformed");
        match config.client_registry() {
            Err(super::ConfigError::Invalid(_)) => {}
            Ok(_) => panic!("expected client_registry to reject the malformed hash"),
            Err(other) => panic!("expected ConfigError::Invalid, got {other}"),
        }
    }

    #[tokio::test]
    async fn load_reads_parses_and_validates_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        tokio::fs::write(&path, valid_toml())
            .await
            .expect("write config");
        let config = Config::load(&path).await.expect("valid config loads");
        assert_eq!(config.clients.len(), 1);
    }
}
