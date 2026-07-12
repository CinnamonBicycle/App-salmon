//! Port for running the two filesystem operations `app_salmon` needs to perform as a worker
//! account instead of as itself. `PrivilegedCommand` is a closed enum, not an argv passthrough —
//! that's what lets the `/etc/sudoers.d` rule (see `docs/DESIGN.md`) name exactly these two
//! operations instead of granting an arbitrary-command escape hatch.

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
