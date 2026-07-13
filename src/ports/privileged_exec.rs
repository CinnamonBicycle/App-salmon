//! Port for running the small, closed set of filesystem operations `app_salmon` needs to perform
//! as a worker account instead of as itself. `PrivilegedCommand` is a closed enum, not an argv
//! passthrough — that's what lets the `/etc/sudoers.d` rule (see `docs/DESIGN.md`) name exactly
//! these operations instead of granting an arbitrary-command escape hatch.

use async_trait::async_trait;
use thiserror::Error;

use crate::domain::ids::WorkerUser;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegedCommand {
    /// Create (if absent) and `chown` the given directory to the worker.
    PrepareWorkerDir {
        /// The absolute path of the directory to create/own.
        path: String,
    },
    /// Recursively delete the contents of the given directory, run as the worker so only files
    /// that worker owns can be touched.
    WipeWorkerDir {
        /// The absolute path of the directory to wipe.
        path: String,
    },
    /// Copies `staging_path`'s contents into `dest_path` (which must already exist, e.g. via a
    /// prior [`PrivilegedCommand::PrepareWorkerDir`]), run as the worker so the copies land
    /// worker-owned. A copy, not a rename/move: `staging_path` is owned by the `app_salmon`
    /// process itself (see `domain::tar_validation`, which extracts an uploaded tar there first,
    /// safely, before this command ever runs) — running the copy as the worker means every byte
    /// written to `dest_path` is a fresh write under the worker's own uid, which is what makes it
    /// worker-owned without a separate `chown` step (and sidesteps `mv`'s cross-filesystem
    /// `EXDEV` failure mode and its preservation of the *original* uid). `staging_path` itself is
    /// left for the caller to clean up — it's `app_salmon`-owned, so no privilege is needed for
    /// that part.
    AdoptStagedTree {
        /// Absolute path of the `app_salmon`-owned source directory to copy from.
        staging_path: String,
        /// Absolute path of the pre-existing, worker-owned destination directory to copy into.
        dest_path: String,
    },
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// The command's captured standard output.
    pub stdout: String,
    /// The command's captured standard error.
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum PrivilegedExecError {
    /// The helper process (e.g. `sudo`) couldn't be launched at all.
    #[error("failed to launch privileged-exec helper for {worker}: {source}")]
    Spawn {
        /// The worker account the command was to run as.
        worker: WorkerUser,
        /// The underlying process-spawn error.
        #[source]
        source: std::io::Error,
    },
    /// The command launched but exited with a non-zero status.
    #[error("command as {worker} exited with status {status}: {stderr}")]
    NonZeroExit {
        /// The worker account the command ran as.
        worker: WorkerUser,
        /// The command's exit status code.
        status: i32,
        /// The command's captured standard error.
        stderr: String,
    },
    /// The command didn't complete within the allotted time and was killed.
    #[error("command as {worker} timed out after {waited_secs}s")]
    Timeout {
        /// The worker account the command was running as.
        worker: WorkerUser,
        /// How long the command was allowed to run before being killed, in seconds.
        waited_secs: u64,
    },
}

#[async_trait]
pub trait PrivilegedExecutor: Send + Sync {
    /// Runs a closed-set privileged command as the given worker account.
    ///
    /// # Arguments
    ///
    /// - `worker`: the worker account to run the command as.
    /// - `command`: which of the closed set of privileged operations to run.
    ///
    /// # Returns
    ///
    /// The command's captured stdout/stderr, on a zero exit status.
    ///
    /// # Errors
    ///
    /// Returns [`PrivilegedExecError::Spawn`] if the helper process can't be launched,
    /// [`PrivilegedExecError::NonZeroExit`] if it runs but exits non-zero, or
    /// [`PrivilegedExecError::Timeout`] if it doesn't complete in time.
    async fn run_as(
        &self,
        worker: &WorkerUser,
        command: PrivilegedCommand,
    ) -> Result<CommandOutput, PrivilegedExecError>;
}
