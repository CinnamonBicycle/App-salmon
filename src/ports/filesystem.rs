//! Port for the directory-walking + compression I/O `service::log_rotation` needs. A generic
//! (non-`dyn`) trait — unlike the other ports, `log_rotation` has exactly one production
//! implementation wired in at the single call site in `main.rs`, so there's no runtime
//! polymorphism to buy with a trait object, and a generic bound monomorphizes to the same code
//! `tokio::fs` calls made directly would produce: no vtable, no boxed futures.
//!
//! Split into three traits (`Filesystem`, `ReadDir`, `DirEntry`) rather than one, to mirror
//! `tokio::fs::read_dir`'s own shape: a fallible call that returns a stream whose *individual*
//! `next_entry` calls are separately fallible — that distinction is what lets
//! `adapters::tokio_filesystem`'s fake inject a failure on one entry without failing the whole
//! sweep, matching `service::log_rotation`'s real behavior (a `read_dir` failure aborts the sweep;
//! a `next_entry` failure aborts the *rest* of the sweep but not what's already been processed; an
//! `is_file`/`modified` failure on one entry only skips that entry).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompressError {
    /// A filesystem operation inside the compression task (open/read/write/remove) failed.
    #[error(transparent)]
    Io(
        /// The underlying I/O error.
        #[from]
        std::io::Error,
    ),
    /// The blocking compression task didn't run to completion (panicked or was cancelled) —
    /// distinct from [`CompressError::Io`], which means the task ran but a filesystem op inside
    /// it failed.
    #[error("compression task did not complete: {0}")]
    TaskFailed(
        /// A description of why the task didn't complete.
        String,
    ),
}

// Every method below is written as `fn(..) -> impl Future<..> + Send` rather than `async fn`.
// Plain `async fn` in a trait doesn't bound the returned future as `Send` — for a trait used only
// generically (as here, never as `dyn Filesystem`) that's invisible until a caller tries to
// `tokio::spawn` a generic function built on it, where it fails to compile with no indication the
// trait itself is the problem. Spelling out `+ Send` here makes it part of the trait's contract:
// any future implementation must satisfy it, checked at the `impl` site, not at every spawn site.
// Implementations may still just write `async fn` — the sugar desugars to a concrete future type,
// and `impl Trait for Type` doesn't need to repeat the explicit `-> impl Future` spelling.

pub trait DirEntry: Send + Sync {
    /// The full path of this directory entry.
    ///
    /// # Returns
    ///
    /// The entry's path, as reported by the directory listing it came from.
    fn path(&self) -> PathBuf;

    /// Whether this entry is a regular file (as opposed to a directory, symlink, etc.).
    ///
    /// # Returns
    ///
    /// `true` if the entry is a regular file.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry's type can't be determined (e.g. it was removed after the
    /// directory listing was taken but before this call).
    fn is_file(&self) -> impl Future<Output = std::io::Result<bool>> + Send;

    /// This entry's last-modified time.
    ///
    /// # Returns
    ///
    /// The entry's modification time.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry's metadata can't be read (e.g. it was removed after the
    /// directory listing was taken but before this call), or if the platform doesn't support
    /// modification-time metadata.
    fn modified(&self) -> impl Future<Output = std::io::Result<SystemTime>> + Send;
}

pub trait ReadDir: Send {
    /// The concrete [`DirEntry`] type this stream yields.
    type Entry: DirEntry;

    /// Advances the directory stream and returns its next entry, if any.
    ///
    /// # Returns
    ///
    /// `Some(entry)` for the next entry in the directory, or `None` once every entry has been
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns an error if reading the next entry from the underlying directory stream fails
    /// (e.g. an I/O error mid-iteration) — distinct from a single entry's own `is_file`/`modified`
    /// failing, which only affects that one entry, not the stream as a whole.
    fn next_entry(&mut self) -> impl Future<Output = std::io::Result<Option<Self::Entry>>> + Send;
}

pub trait Filesystem: Send + Sync + 'static {
    /// The concrete [`DirEntry`] type produced by this filesystem's directory listings — must
    /// match [`Filesystem::ReadDir`]'s own `Entry` type, so a caller iterating a `ReadDir` gets
    /// back the same entry type this trait's other methods expect.
    type Entry: DirEntry;
    /// The concrete [`ReadDir`] stream type returned by [`Filesystem::read_dir`].
    type ReadDir: ReadDir<Entry = Self::Entry>;

    /// Opens a directory for listing.
    ///
    /// # Arguments
    ///
    /// - `dir`: the directory to list.
    ///
    /// # Returns
    ///
    /// A stream of the directory's entries, to be advanced via [`ReadDir::next_entry`].
    ///
    /// # Errors
    ///
    /// Returns an error if `dir` doesn't exist or can't be opened for listing.
    fn read_dir(&self, dir: &Path) -> impl Future<Output = std::io::Result<Self::ReadDir>> + Send;

    /// Deletes a single file.
    ///
    /// # Arguments
    ///
    /// - `path`: the file to delete.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` doesn't exist or can't be removed.
    fn remove_file(&self, path: &Path) -> impl Future<Output = std::io::Result<()>> + Send;

    /// Gzips `path` in place (writing `path` + `.gz`, then removing the original), off the async
    /// executor since it's CPU-bound synchronous I/O.
    ///
    /// # Arguments
    ///
    /// - `path`: the file to compress.
    ///
    /// # Returns
    ///
    /// Nothing, on success — the compressed file has been written alongside `path` and the
    /// original has been removed.
    ///
    /// # Errors
    ///
    /// Returns [`CompressError::Io`] if a filesystem operation involved in compressing the file
    /// fails, or [`CompressError::TaskFailed`] if the underlying blocking task didn't run to
    /// completion.
    fn compress(&self, path: &Path) -> impl Future<Output = Result<(), CompressError>> + Send;
}
