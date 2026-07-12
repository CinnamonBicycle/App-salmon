//! `tracing_appender::rolling::daily` handles time-based file rotation (a new file each day) but
//! does not compress rotated files or prune old ones — there's no retention story built in. This
//! fills exactly that gap: a periodic sweep of the log directory that gzips plain log files past
//! `compress_after` and deletes `.gz` archives past `retention`, and nothing else.
//!
//! Generic over [`Filesystem`] (see `ports::filesystem` for why that's a generic bound rather than
//! `dyn`) so directory-iteration and per-entry I/O failures — genuinely possible races against a
//! real filesystem, not just theoretical — are unit-testable via a fake, the same way every other
//! adapter in this crate is.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::ports::filesystem::{DirEntry, Filesystem, ReadDir};

const GZ_EXTENSION: &str = "gz";

/// Checks whether `path` is a `.gz` archive by its file extension alone (no I/O).
///
/// # Arguments
///
/// - `path`: the path to check.
///
/// # Returns
///
/// `true` if `path`'s extension is exactly `gz`, `false` otherwise (including paths with no
/// extension at all).
fn is_gz(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some(GZ_EXTENSION)
}

/// Checks whether a file's age (relative to `now`) has reached `threshold`.
///
/// # Arguments
///
/// - `modified`: the file's last-modified time.
/// - `now`: the current time to measure age against.
/// - `threshold`: the minimum age for this to report `true`.
///
/// # Returns
///
/// `true` if `now - modified >= threshold`. `false` if `threshold` hasn't been reached yet, or if
/// `modified` is later than `now` (a clock skew/adjustment case `duration_since` reports as an
/// error rather than a negative duration).
fn older_than(modified: SystemTime, now: SystemTime, threshold: Duration) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age >= threshold)
}

/// One sweep of `log_dir`. Logs and continues past any single file's failure rather than
/// aborting — a single unreadable/locked file shouldn't stop the rest from being processed. A
/// failure reading the directory stream itself (as opposed to one entry) aborts the rest of the
/// sweep, since there's no way to know what entries were skipped.
///
/// # Arguments
///
/// - `fs`: the filesystem to sweep — the real `tokio::fs`-backed adapter in production, a fake in
///   tests.
/// - `log_dir`: the directory to scan (non-recursively; subdirectories are skipped).
/// - `compress_after`: how old a plain (non-`.gz`) file must be, by mtime, before it's compressed.
/// - `retention`: how old a `.gz` archive must be, by mtime, before it's deleted.
pub async fn run_once<FS: Filesystem>(
    fs: &FS,
    log_dir: &Path,
    compress_after: Duration,
    retention: Duration,
) {
    let mut entries = match fs.read_dir(log_dir).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::error!(path = %log_dir.display(), error = %error, "log rotation failed to read log directory");
            return;
        }
    };

    let now = SystemTime::now();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                tracing::error!(error = %error, "log rotation failed to read a directory entry");
                break;
            }
        };

        let is_file = match entry.is_file().await {
            Ok(is_file) => is_file,
            Err(error) => {
                tracing::warn!(path = %entry.path().display(), error = %error, "log rotation failed to stat entry");
                continue;
            }
        };
        if !is_file {
            continue;
        }

        let modified = match entry.modified().await {
            Ok(modified) => modified,
            Err(error) => {
                tracing::warn!(path = %entry.path().display(), error = %error, "log rotation failed to read mtime");
                continue;
            }
        };

        let path = entry.path();
        if is_gz(&path) {
            if older_than(modified, now, retention)
                && let Err(error) = fs.remove_file(&path).await
            {
                tracing::warn!(path = %path.display(), error = %error, "log rotation failed to delete expired archive");
            }
        } else if older_than(modified, now, compress_after)
            && let Err(error) = fs.compress(&path).await
        {
            tracing::warn!(path = %path.display(), error = %error, "log rotation failed to compress log file");
        }
    }
}

/// Drives [`run_once`] on a fixed interval for the life of the process.
///
/// # Arguments
///
/// - `fs`: the filesystem to sweep on each tick — see [`run_once`].
/// - `log_dir`: the directory to scan on each tick — see [`run_once`].
/// - `interval`: how long to wait between sweeps.
/// - `compress_after`: how old a plain file must be before it's compressed — see [`run_once`].
/// - `retention`: how old a `.gz` archive must be before it's deleted — see [`run_once`].
pub async fn run_forever<FS: Filesystem>(
    fs: FS,
    log_dir: PathBuf,
    interval: Duration,
    compress_after: Duration,
    retention: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        run_once(&fs, &log_dir, compress_after, retention).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{run_forever, run_once};
    use crate::adapters::tokio_filesystem::TokioFilesystem;
    use crate::ports::filesystem::{CompressError, DirEntry, Filesystem, ReadDir};
    use std::collections::VecDeque;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    fn set_mtime(path: &std::path::Path, age: Duration) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for mtime set");
        file.set_modified(SystemTime::now() - age)
            .expect("set mtime");
    }

    #[tokio::test]
    async fn old_plain_log_file_is_compressed_and_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app_salmon.log.2026-01-01");
        tokio::fs::write(&path, b"hello log")
            .await
            .expect("write log");
        set_mtime(&path, Duration::from_hours(1));

        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        assert!(!path.exists(), "original file should be removed");
        let gz_path = dir.path().join("app_salmon.log.2026-01-01.gz");
        assert!(gz_path.exists(), "compressed archive should exist");

        let mut decoder =
            flate2::read::GzDecoder::new(std::fs::File::open(&gz_path).expect("open gz"));
        let mut contents = String::new();
        decoder.read_to_string(&mut contents).expect("decompress");
        assert_eq!(contents, "hello log");
    }

    #[tokio::test]
    async fn recent_plain_log_file_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app_salmon.log.2026-07-11");
        tokio::fs::write(&path, b"still writing")
            .await
            .expect("write log");

        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_hours(1),
            Duration::from_hours(24),
        )
        .await;

        assert!(path.exists(), "recent file should not be compressed yet");
        assert!(!dir.path().join("app_salmon.log.2026-07-11.gz").exists());
    }

    #[tokio::test]
    async fn old_gz_archive_past_retention_is_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app_salmon.log.2025-01-01.gz");
        tokio::fs::write(&path, b"archived")
            .await
            .expect("write archive");
        set_mtime(&path, Duration::from_secs(200_000));

        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        assert!(!path.exists(), "expired archive should be deleted");
    }

    #[tokio::test]
    async fn recent_gz_archive_within_retention_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app_salmon.log.2026-07-10.gz");
        tokio::fs::write(&path, b"archived")
            .await
            .expect("write archive");

        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        assert!(path.exists(), "archive within retention should be kept");
    }

    #[tokio::test]
    async fn empty_directory_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;
    }

    #[tokio::test]
    async fn missing_directory_does_not_panic() {
        run_once(
            &TokioFilesystem,
            std::path::Path::new("/nonexistent/log/dir"),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;
    }

    #[tokio::test]
    async fn subdirectories_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let subdir = dir.path().join("app_salmon.log.not-a-file");
        tokio::fs::create_dir(&subdir).await.expect("create subdir");

        // Would panic/error if `run_once` tried to treat the directory as a compressible file.
        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;
        assert!(subdir.is_dir(), "subdirectory should be untouched");
    }

    #[tokio::test]
    async fn unreadable_file_fails_to_compress_and_is_left_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app_salmon.log.unreadable");
        tokio::fs::write(&path, b"data").await.expect("write log");
        set_mtime(&path, Duration::from_hours(1));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");

        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        // Restore permissions so the tempdir can be cleaned up.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore permissions");
        assert!(
            path.exists(),
            "unreadable file should be left in place, not silently lost"
        );
        assert!(!dir.path().join("app_salmon.log.unreadable.gz").exists());
    }

    #[tokio::test]
    async fn deleting_an_expired_archive_that_cannot_be_removed_is_logged_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app_salmon.log.old.gz");
        tokio::fs::write(&path, b"archived")
            .await
            .expect("write archive");
        set_mtime(&path, Duration::from_secs(200_000));
        // Removing a file requires write permission on its *containing directory*, not the file.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
            .expect("make dir read-only");

        run_once(
            &TokioFilesystem,
            dir.path(),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("restore permissions");
        assert!(
            path.exists(),
            "archive should survive a failed delete attempt"
        );
    }

    #[tokio::test]
    async fn run_forever_performs_a_sweep_on_each_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app_salmon.log.old");
        tokio::fs::write(&path, b"data").await.expect("write log");
        set_mtime(&path, Duration::from_hours(1));

        let log_dir = dir.path().to_path_buf();
        let handle = tokio::spawn(run_forever(
            TokioFilesystem,
            log_dir,
            Duration::from_millis(20),
            Duration::from_millis(1),
            Duration::from_hours(24),
        ));

        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.abort();

        assert!(
            !path.exists(),
            "run_forever should have compressed the old file by now"
        );
        assert!(dir.path().join("app_salmon.log.old.gz").exists());
    }

    // --- Fake filesystem, for the error-injection branches a real filesystem can't reliably hit
    // on demand: a directory-entry stream failing mid-iteration, and a single entry's stat calls
    // failing without the whole sweep aborting. ---

    #[derive(Clone)]
    struct FakeEntry {
        path: PathBuf,
        is_file: bool,
        modified: SystemTime,
        fail_is_file: bool,
        fail_modified: bool,
    }

    impl DirEntry for FakeEntry {
        fn path(&self) -> PathBuf {
            self.path.clone()
        }

        async fn is_file(&self) -> std::io::Result<bool> {
            if self.fail_is_file {
                Err(std::io::Error::other("simulated file_type failure"))
            } else {
                Ok(self.is_file)
            }
        }

        async fn modified(&self) -> std::io::Result<SystemTime> {
            if self.fail_modified {
                Err(std::io::Error::other("simulated metadata failure"))
            } else {
                Ok(self.modified)
            }
        }
    }

    struct FakeReadDir {
        items: VecDeque<Result<FakeEntry, String>>,
    }

    impl ReadDir for FakeReadDir {
        type Entry = FakeEntry;

        async fn next_entry(&mut self) -> std::io::Result<Option<FakeEntry>> {
            match self.items.pop_front() {
                None => Ok(None),
                Some(Ok(entry)) => Ok(Some(entry)),
                Some(Err(message)) => Err(std::io::Error::other(message)),
            }
        }
    }

    struct FakeFilesystem {
        items: Vec<Result<FakeEntry, String>>,
        fail_compress: bool,
        compressed: std::sync::Mutex<Vec<PathBuf>>,
        removed: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl Filesystem for FakeFilesystem {
        type Entry = FakeEntry;
        type ReadDir = FakeReadDir;

        async fn read_dir(&self, _dir: &Path) -> std::io::Result<FakeReadDir> {
            Ok(FakeReadDir {
                items: self.items.clone().into(),
            })
        }

        async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.removed.lock().expect("lock").push(path.to_path_buf());
            Ok(())
        }

        async fn compress(&self, path: &Path) -> Result<(), CompressError> {
            self.compressed
                .lock()
                .expect("lock")
                .push(path.to_path_buf());
            if self.fail_compress {
                Err(CompressError::TaskFailed(
                    "simulated compression task failure".to_string(),
                ))
            } else {
                Ok(())
            }
        }
    }

    impl Default for FakeFilesystem {
        fn default() -> Self {
            Self {
                items: Vec::new(),
                fail_compress: false,
                compressed: std::sync::Mutex::new(Vec::new()),
                removed: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    fn old_file(name: &str) -> FakeEntry {
        FakeEntry {
            path: PathBuf::from(name),
            is_file: true,
            modified: SystemTime::now() - Duration::from_hours(1),
            fail_is_file: false,
            fail_modified: false,
        }
    }

    #[tokio::test]
    async fn a_next_entry_failure_aborts_the_rest_of_the_sweep_but_not_earlier_entries() {
        let fs = FakeFilesystem {
            items: vec![
                Ok(old_file("app_salmon.log.a")),
                Err("simulated readdir failure".to_string()),
                Ok(old_file("app_salmon.log.b")),
            ],
            ..Default::default()
        };

        run_once(
            &fs,
            Path::new("/fake"),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        let compressed = fs.compressed.lock().expect("lock");
        assert_eq!(
            *compressed,
            vec![PathBuf::from("app_salmon.log.a")],
            "the entry before the failure is processed; the one after it is not"
        );
    }

    #[tokio::test]
    async fn an_entry_whose_file_type_check_fails_is_skipped_not_fatal() {
        let mut entry = old_file("app_salmon.log.bad-stat");
        entry.fail_is_file = true;
        let fs = FakeFilesystem {
            items: vec![Ok(entry), Ok(old_file("app_salmon.log.fine"))],
            ..Default::default()
        };

        run_once(
            &fs,
            Path::new("/fake"),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        assert_eq!(
            *fs.compressed.lock().expect("lock"),
            vec![PathBuf::from("app_salmon.log.fine")],
            "the entry with a failing file_type check is skipped, not fatal to the sweep"
        );
    }

    #[tokio::test]
    async fn an_entry_whose_mtime_check_fails_is_skipped_not_fatal() {
        let mut entry = old_file("app_salmon.log.bad-mtime");
        entry.fail_modified = true;
        let fs = FakeFilesystem {
            items: vec![Ok(entry), Ok(old_file("app_salmon.log.fine"))],
            ..Default::default()
        };

        run_once(
            &fs,
            Path::new("/fake"),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        assert_eq!(
            *fs.compressed.lock().expect("lock"),
            vec![PathBuf::from("app_salmon.log.fine")],
            "the entry with a failing mtime check is skipped, not fatal to the sweep"
        );
    }

    #[tokio::test]
    async fn a_compression_task_that_fails_to_complete_is_logged_not_fatal() {
        let fs = FakeFilesystem {
            items: vec![Ok(old_file("app_salmon.log.a"))],
            fail_compress: true,
            ..Default::default()
        };

        // Must not panic even though the (simulated) blocking compression task never completed.
        run_once(
            &fs,
            Path::new("/fake"),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;
    }

    #[tokio::test]
    async fn an_expired_gz_archive_is_removed_via_the_filesystem_port() {
        let mut entry = old_file("app_salmon.log.old.gz");
        entry.modified = SystemTime::now() - Duration::from_hours(48);
        let fs = FakeFilesystem {
            items: vec![Ok(entry)],
            ..Default::default()
        };

        run_once(
            &fs,
            Path::new("/fake"),
            Duration::from_mins(1),
            Duration::from_hours(24),
        )
        .await;

        assert_eq!(
            *fs.removed.lock().expect("lock"),
            vec![PathBuf::from("app_salmon.log.old.gz")]
        );
        assert!(
            fs.compressed.lock().expect("lock").is_empty(),
            "an already-.gz file should be removed, not re-compressed"
        );
    }
}
