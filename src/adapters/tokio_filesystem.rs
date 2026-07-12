//! Real [`Filesystem`] impl, backed directly by `tokio::fs`. The only production implementation —
//! see `ports::filesystem` for why this port is generic rather than `dyn`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::ports::filesystem::{CompressError, DirEntry, Filesystem, ReadDir};

/// Computes the `.gz` archive path for a plain log file, by appending `.gz` to its filename.
///
/// # Arguments
///
/// - `path`: the plain (uncompressed) file's path.
///
/// # Returns
///
/// `path` with `.gz` appended to its final path component (e.g. `app_salmon.log.2026-01-01`
/// becomes `app_salmon.log.2026-01-01.gz`).
fn gz_path_for(path: &Path) -> PathBuf {
    let mut os_string = path.as_os_str().to_os_string();
    os_string.push(".gz");
    PathBuf::from(os_string)
}

/// Gzips `path` in place: reads it, writes `path` + `.gz`, then deletes the original. Synchronous
/// (blocking) I/O throughout — must only be called from inside `tokio::task::spawn_blocking`, not
/// directly from an async context.
///
/// # Arguments
///
/// - `path`: the plain log file to compress and remove.
///
/// # Returns
///
/// Nothing on success; the compressed archive is left at `path` + `.gz` and the original file is
/// removed.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if opening `path`, creating the archive, copying,
/// finishing the gzip stream, or removing the original file fails.
fn compress_file_blocking(path: &Path) -> std::io::Result<()> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let gz_path = gz_path_for(path);
    let output = std::fs::File::create(&gz_path)?;
    let mut encoder = GzEncoder::new(output, Compression::default());
    std::io::copy(&mut reader, &mut encoder)?;
    encoder.finish()?;
    std::fs::remove_file(path)?;
    Ok(())
}

/// Real [`Filesystem`] implementation, backed directly by `tokio::fs` — the only production
/// implementation of this port (see `ports::filesystem` for why the port is a generic bound
/// rather than `dyn`).
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioFilesystem;

/// Real [`DirEntry`] implementation, wrapping a `tokio::fs::DirEntry`.
pub struct TokioDirEntry(
    /// The underlying directory entry this type delegates every method to.
    tokio::fs::DirEntry,
);

impl DirEntry for TokioDirEntry {
    /// # Returns
    ///
    /// The wrapped directory entry's path.
    fn path(&self) -> PathBuf {
        self.0.path()
    }

    /// # Returns
    ///
    /// `true` if the entry is a regular file (not a directory, symlink, or other special file).
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the entry's file type can't be determined (e.g.
    /// it was removed after being listed but before this call).
    async fn is_file(&self) -> std::io::Result<bool> {
        Ok(self.0.file_type().await?.is_file())
    }

    /// # Returns
    ///
    /// The entry's last-modified time, as reported by the filesystem.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the entry's metadata can't be read, or if the
    /// platform doesn't support a modified-time field.
    async fn modified(&self) -> std::io::Result<SystemTime> {
        self.0.metadata().await?.modified()
    }
}

/// Real [`ReadDir`] implementation, wrapping a `tokio::fs::ReadDir` stream.
pub struct TokioReadDir(
    /// The underlying directory stream this type delegates every method to.
    tokio::fs::ReadDir,
);

impl ReadDir for TokioReadDir {
    /// Real directory streams yield real, `tokio::fs`-backed entries.
    type Entry = TokioDirEntry;

    /// Advances the directory stream by one entry.
    ///
    /// # Returns
    ///
    /// `Some` with the next entry, or `None` once every entry in the directory has been
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if reading the next directory entry fails (e.g.
    /// the directory itself was removed mid-iteration).
    async fn next_entry(&mut self) -> std::io::Result<Option<TokioDirEntry>> {
        Ok(self.0.next_entry().await?.map(TokioDirEntry))
    }
}

impl Filesystem for TokioFilesystem {
    /// Real listings yield real, `tokio::fs`-backed entries.
    type Entry = TokioDirEntry;
    /// Real listings are streamed via a real `tokio::fs::ReadDir` wrapper.
    type ReadDir = TokioReadDir;

    /// Opens a directory for streaming iteration.
    ///
    /// # Arguments
    ///
    /// - `dir`: the directory to list.
    ///
    /// # Returns
    ///
    /// A [`TokioReadDir`] stream; call its `next_entry` to walk the directory's contents.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if `dir` doesn't exist or can't be read.
    async fn read_dir(&self, dir: &Path) -> std::io::Result<TokioReadDir> {
        Ok(TokioReadDir(tokio::fs::read_dir(dir).await?))
    }

    /// Deletes a file.
    ///
    /// # Arguments
    ///
    /// - `path`: the file to remove.
    ///
    /// # Returns
    ///
    /// Nothing on success.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if `path` doesn't exist or can't be removed.
    async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::remove_file(path).await
    }

    /// Gzips `path` in place, off the async executor (via `tokio::task::spawn_blocking`) since
    /// compression is CPU-bound synchronous work.
    ///
    /// # Arguments
    ///
    /// - `path`: the plain log file to compress and remove; see [`compress_file_blocking`].
    ///
    /// # Returns
    ///
    /// Nothing on success.
    ///
    /// # Errors
    ///
    /// Returns [`CompressError::Io`] if the blocking compression itself fails (e.g. a permission
    /// error opening or removing the file), or [`CompressError::TaskFailed`] if the blocking task
    /// panicked or was cancelled before completing.
    async fn compress(&self, path: &Path) -> Result<(), CompressError> {
        let owned_path = path.to_path_buf();
        match tokio::task::spawn_blocking(move || compress_file_blocking(&owned_path)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(io_error)) => Err(CompressError::Io(io_error)),
            Err(join_error) => Err(CompressError::TaskFailed(join_error.to_string())),
        }
    }
}
