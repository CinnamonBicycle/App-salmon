//! `PrivilegedExecutor` via `sudo -u <worker> -- <program> <args...>`, run through
//! `tokio::process::Command` (argv-based, never a shell) so `path` values can never trigger
//! shell injection regardless of their content.
//!
//! The `sudo` executable path is configurable (default `"sudo"`, resolved via `PATH`) so tests
//! can point it at a fake stand-in for `sudo` instead of the real, root-requiring binary — this
//! is what lets this adapter's own argv-construction and exit-code/stderr-handling code be
//! exercised by `cargo test` without root.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::domain::ids::WorkerUser;
use crate::ports::privileged_exec::{
    CommandOutput, PrivilegedCommand, PrivilegedExecError, PrivilegedExecutor,
};

pub struct SudoExecutor {
    /// Path (or bare name, resolved via `PATH`) of the `sudo` executable to invoke. Configurable
    /// so tests can point it at a fake stand-in instead of the real, root-requiring binary.
    sudo_path: String,
    /// Maximum time to wait for a privileged command to complete before treating it as hung and
    /// returning [`PrivilegedExecError::Timeout`].
    timeout: Duration,
}

impl SudoExecutor {
    /// Builds a new executor that will shell out to `sudo_path` for every [`Self::run_as`] call.
    ///
    /// # Arguments
    ///
    /// - `sudo_path`: path or bare name of the `sudo` executable to invoke; resolved via `PATH`
    ///   if not an absolute/relative path. In production this is `"sudo"`; tests pass the path to
    ///   a fake stand-in script instead.
    /// - `timeout`: how long to wait for a privileged command to finish before it's treated as
    ///   hung.
    ///
    /// # Returns
    ///
    /// A new [`SudoExecutor`] ready to have [`Self::run_as`] called on it.
    #[must_use]
    pub fn new(sudo_path: impl Into<String>, timeout: Duration) -> Self {
        Self {
            sudo_path: sudo_path.into(),
            timeout,
        }
    }
}

/// Maps a closed [`PrivilegedCommand`] variant to the actual program and argv to run as the
/// target worker — the only place this adapter decides what `sudo` is allowed to execute, so the
/// `/etc/sudoers.d` rule (see `docs/DESIGN.md`) can name exactly these two operations instead of
/// granting an arbitrary-command escape hatch.
///
/// # Arguments
///
/// - `command`: the privileged operation to translate into a program name and argv.
///
/// # Returns
///
/// A tuple of the program name to execute (e.g. `"mkdir"`) and the argv to pass it (not including
/// the program name itself).
fn program_and_args(command: &PrivilegedCommand) -> (&'static str, Vec<String>) {
    match command {
        PrivilegedCommand::PrepareWorkerDir { path } => {
            ("mkdir", vec!["-p".to_string(), path.clone()])
        }
        PrivilegedCommand::WipeWorkerDir { path } => (
            "find",
            vec![
                path.clone(),
                "-mindepth".to_string(),
                "1".to_string(),
                "-delete".to_string(),
            ],
        ),
    }
}

#[async_trait]
impl PrivilegedExecutor for SudoExecutor {
    /// Runs `command` as `worker` via `sudo -u <worker> -- <program> <args...>`, built and
    /// spawned through `tokio::process::Command`'s argv API (never a shell), so `path` values
    /// embedded in `command` can never trigger shell injection regardless of their content.
    ///
    /// # Arguments
    ///
    /// - `worker`: the account to run `command` as, via `sudo -u`.
    /// - `command`: the closed, pre-validated operation to perform — translated to a concrete
    ///   program and argv by [`program_and_args`].
    ///
    /// # Returns
    ///
    /// The child process's captured stdout/stderr on a zero exit status.
    ///
    /// # Errors
    ///
    /// Returns [`PrivilegedExecError::Timeout`] if the command doesn't complete within
    /// `self.timeout`, [`PrivilegedExecError::Spawn`] if the `sudo` process itself couldn't be
    /// launched (e.g. the binary doesn't exist), or [`PrivilegedExecError::NonZeroExit`] if it ran
    /// but exited with a non-zero status.
    async fn run_as(
        &self,
        worker: &WorkerUser,
        command: PrivilegedCommand,
    ) -> Result<CommandOutput, PrivilegedExecError> {
        let (program, args) = program_and_args(&command);

        let mut cmd = Command::new(&self.sudo_path);
        cmd.arg("-u")
            .arg(worker.as_str())
            .arg("--")
            .arg(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_elapsed| PrivilegedExecError::Timeout {
                worker: worker.clone(),
                waited_secs: self.timeout.as_secs(),
            })?
            .map_err(|source| PrivilegedExecError::Spawn {
                worker: worker.clone(),
                source,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if output.status.success() {
            Ok(CommandOutput { stdout, stderr })
        } else {
            Err(PrivilegedExecError::NonZeroExit {
                worker: worker.clone(),
                status: output.status.code().unwrap_or(-1),
                stderr,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SudoExecutor;
    use crate::domain::ids::WorkerUser;
    use crate::ports::privileged_exec::{
        PrivilegedCommand, PrivilegedExecError, PrivilegedExecutor,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    async fn write_fake_sudo(dir: &std::path::Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("fake-sudo.sh");
        tokio::fs::write(&path, contents)
            .await
            .expect("write fake sudo script");
        let mut perms = tokio::fs::metadata(&path)
            .await
            .expect("stat")
            .permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&path, perms)
            .await
            .expect("chmod");
        path
    }

    fn worker() -> WorkerUser {
        WorkerUser::new("salmon-worker-00", 2000, 2000)
    }

    #[tokio::test]
    async fn prepare_worker_dir_creates_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Passthrough fake: drops "-u <worker> --" and execs the real program, so this exercises
        // our real argv construction against real `mkdir`, without needing real sudo/root.
        let sudo = write_fake_sudo(dir.path(), "#!/bin/sh\nshift 3\nexec \"$@\"\n").await;
        let executor =
            SudoExecutor::new(sudo.to_string_lossy().to_string(), Duration::from_secs(5));

        let target = dir.path().join("workers").join("salmon-worker-00");
        executor
            .run_as(
                &worker(),
                PrivilegedCommand::PrepareWorkerDir {
                    path: target.to_string_lossy().to_string(),
                },
            )
            .await
            .expect("prepare succeeds");

        assert!(target.is_dir());
    }

    #[tokio::test]
    async fn wipe_worker_dir_removes_contents_but_keeps_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sudo = write_fake_sudo(dir.path(), "#!/bin/sh\nshift 3\nexec \"$@\"\n").await;
        let executor =
            SudoExecutor::new(sudo.to_string_lossy().to_string(), Duration::from_secs(5));

        let target = dir.path().join("workers").join("salmon-worker-00");
        tokio::fs::create_dir_all(&target)
            .await
            .expect("create target");
        tokio::fs::write(target.join("leftover.txt"), b"stale data")
            .await
            .expect("write leftover file");

        executor
            .run_as(
                &worker(),
                PrivilegedCommand::WipeWorkerDir {
                    path: target.to_string_lossy().to_string(),
                },
            )
            .await
            .expect("wipe succeeds");

        assert!(target.is_dir());
        let mut entries = tokio::fs::read_dir(&target).await.expect("read dir");
        assert!(entries.next_entry().await.expect("read entry").is_none());
    }

    #[tokio::test]
    async fn non_zero_exit_is_reported_with_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sudo = write_fake_sudo(
            dir.path(),
            "#!/bin/sh\necho 'permission denied' >&2\nexit 7\n",
        )
        .await;
        let executor =
            SudoExecutor::new(sudo.to_string_lossy().to_string(), Duration::from_secs(5));

        let err = executor
            .run_as(
                &worker(),
                PrivilegedCommand::PrepareWorkerDir {
                    path: "/irrelevant".to_string(),
                },
            )
            .await
            .expect_err("non-zero exit");

        match err {
            PrivilegedExecError::NonZeroExit { status, stderr, .. } => {
                assert_eq!(status, 7);
                assert!(stderr.contains("permission denied"));
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_is_reported_when_command_hangs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sudo = write_fake_sudo(dir.path(), "#!/bin/sh\nsleep 5\n").await;
        let executor = SudoExecutor::new(
            sudo.to_string_lossy().to_string(),
            Duration::from_millis(50),
        );

        let err = executor
            .run_as(
                &worker(),
                PrivilegedCommand::PrepareWorkerDir {
                    path: "/irrelevant".to_string(),
                },
            )
            .await
            .expect_err("timeout");

        assert!(matches!(err, PrivilegedExecError::Timeout { .. }));
    }

    #[tokio::test]
    async fn spawn_failure_is_reported_when_sudo_binary_is_missing() {
        let executor = SudoExecutor::new(
            "/nonexistent/sudo-binary".to_string(),
            Duration::from_secs(5),
        );
        let err = executor
            .run_as(
                &worker(),
                PrivilegedCommand::PrepareWorkerDir {
                    path: "/irrelevant".to_string(),
                },
            )
            .await
            .expect_err("spawn failure");
        assert!(matches!(err, PrivilegedExecError::Spawn { .. }));
    }
}
