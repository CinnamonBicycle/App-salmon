//! Structured logging: JSON to a daily-rotating file under `logging.log_dir` (for automated
//! analysis and `service::log_rotation`), plus a human-readable layer on stderr. Both honor
//! `RUST_LOG`/`tracing_subscriber::EnvFilter` (default: `info`).
//!
//! Returns the `tracing_appender` worker guard, which must be kept alive for the process
//! lifetime — dropping it stops the background thread that flushes buffered log writes to disk.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Why [`init`] failed.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// `log_dir` didn't exist and couldn't be created.
    #[error("failed to create log directory {path}: {source}")]
    CreateLogDir {
        /// The directory path that couldn't be created.
        path: std::path::PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// A global `tracing` subscriber was already installed (this must only be called once, at
    /// process startup).
    #[error("failed to install global tracing subscriber: {0}")]
    Init(#[from] tracing_subscriber::util::TryInitError),
}

/// Creates `log_dir` if needed, then installs the process-global `tracing` subscriber: a JSON
/// layer writing to a daily-rotating file under `log_dir`, and a human-readable layer on stderr,
/// both filtered by `RUST_LOG` (default `info`).
///
/// # Arguments
///
/// - `log_dir`: directory to create (if missing) and write daily-rotating JSON log files into.
///
/// # Returns
///
/// The `tracing_appender` [`WorkerGuard`] for the file layer's non-blocking writer. Must be kept
/// alive for the process lifetime — dropping it stops the background thread that flushes buffered
/// log writes to disk.
///
/// # Errors
///
/// [`TelemetryError::CreateLogDir`] if `log_dir` doesn't exist and can't be created, or
/// [`TelemetryError::Init`] if a global subscriber is already installed (this must only be called
/// once, at process startup).
pub fn init(log_dir: &Path) -> Result<WorkerGuard, TelemetryError> {
    std::fs::create_dir_all(log_dir).map_err(|source| TelemetryError::CreateLogDir {
        path: log_dir.to_path_buf(),
        source,
    })?;

    let file_appender = tracing_appender::rolling::daily(log_dir, "app_salmon.log");
    let (non_blocking_file, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let json_layer = fmt::layer()
        .json()
        .with_writer(non_blocking_file)
        .with_ansi(false);
    let human_layer = fmt::layer().with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .with(human_layer)
        .try_init()?;

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::init;

    // Installs the real global tracing subscriber — safe here because this is the only test in
    // the crate that calls `init`, so there's no risk of a second `try_init` call conflicting
    // with it. Every other test that logs just gets captured by whichever subscriber this
    // installs, which doesn't affect their assertions.
    #[test]
    fn creates_the_log_directory_and_installs_the_subscriber() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_dir = dir.path().join("nested").join("logs");
        assert!(!log_dir.exists());

        let _guard = init(&log_dir).expect("init succeeds");
        assert!(log_dir.is_dir());
    }

    #[test]
    fn reports_an_error_when_the_log_dir_path_is_unusable() {
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        // A path with a *file* as an ancestor component can never be created as a directory.
        let unusable = file.path().join("logs");

        let err = init(&unusable).expect_err("cannot create a dir under a file");
        assert!(matches!(err, super::TelemetryError::CreateLogDir { .. }));
    }
}
