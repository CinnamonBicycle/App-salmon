//! Validates and extracts the caller-supplied tar containing a Supabase project directory (see
//! `docs/DESIGN.md` §11) — never trusted, per `docs/DESIGN.md` §7(a)'s original requirement.
//!
//! Never calls `Archive::unpack()` or the raw `Entry::unpack()` — both are explicitly documented
//! by the `tar` crate itself as unsafe for untrusted input. Extraction goes through
//! [`tar::Entry::unpack_in`], the crate's own documented-safe primitive: reading its actual
//! implementation (not just its docs) confirms it rejects any entry path containing a `..`
//! component and correctly strips a leading root component rather than naively joining it (the
//! classic `Path::join`-with-an-absolute-path gotcha, which *would* let an absolute path escape
//! the destination — confirmed this crate does not have that bug).
//!
//! What `unpack_in` explicitly does not cover, per its own crate-level "Security" documentation:
//! hardlinks, device/fifo files, and size limits. This module adds exactly those checks, before
//! ever calling `unpack_in` on an entry — not a reimplementation of what the crate already does
//! correctly.
//!
//! It also normalizes every extracted entry's mode to a fixed, world-readable value
//! (`0o755`/`0o644`) after `unpack_in` places it, overriding whatever the untrusted tar's own
//! header specified. Extraction runs as the `app_salmon` process, but the extracted tree is later
//! read by a *different* uid (a privileged copy performed as the owning worker account, see
//! `service::spawn_task::adopt_project_tar`) — an attacker-chosen restrictive mode would otherwise
//! silently break that step.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tar::{Archive, EntryType};
use thiserror::Error;

/// Size limits applied while validating/extracting a tar — see `docs/DESIGN.md` §11 for why
/// these exist (bounding decompression-bomb-style abuse) and `Config`'s `[limits]` table for
/// where the configured values come from.
#[derive(Debug, Clone, Copy)]
pub struct TarLimits {
    /// Maximum size of any single entry, in bytes, checked from the entry's header before it is
    /// read or extracted.
    pub max_entry_bytes: u64,
    /// Maximum cumulative size across all entries, in bytes.
    pub max_total_bytes: u64,
}

#[derive(Debug, Error)]
pub enum TarValidationError {
    /// An entry is a type this module never allows, regardless of size or path — only regular
    /// files and directories are accepted. Symlinks and hardlinks are rejected outright (their
    /// targets are never validated, just refused — a Supabase project tree has no legitimate use
    /// for either) and device/fifo files are rejected as having no place in one at all.
    #[error("tar entry {index} ({path}) has a disallowed type: {entry_type:?}")]
    DisallowedEntryType {
        /// The entry's position in the archive (0-based), for locating it in a large tar.
        index: usize,
        /// The entry's path, best-effort (empty if even the path itself couldn't be read).
        path: String,
        /// The type that was rejected.
        entry_type: EntryType,
    },
    /// A single entry's declared size exceeds [`TarLimits::max_entry_bytes`]. Checked from the
    /// entry's header, before any of its content is read.
    #[error(
        "tar entry {index} ({path}) is {size_bytes} bytes, over the {limit_bytes}-byte per-entry limit"
    )]
    EntryTooLarge {
        /// The entry's position in the archive (0-based).
        index: usize,
        /// The entry's path, best-effort.
        path: String,
        /// The entry's declared size.
        size_bytes: u64,
        /// The configured per-entry limit that was exceeded.
        limit_bytes: u64,
    },
    /// The cumulative declared size across all entries seen so far exceeds
    /// [`TarLimits::max_total_bytes`]. Checked incrementally from entry headers, before
    /// extraction — an oversized archive is rejected without extracting everything first.
    #[error("tar contents exceed the cumulative {limit_bytes}-byte limit")]
    TotalTooLarge {
        /// The configured cumulative limit that was exceeded.
        limit_bytes: u64,
    },
    /// `unpack_in` itself declined to extract an entry (its own path-safety check failed) —
    /// distinct from the entry-type/size checks above, this is `tar`'s own safety mechanism
    /// firing, not this module's.
    #[error(
        "tar entry {index} ({path}) was rejected by the archive library's own path-safety check"
    )]
    UnsafeEntry {
        /// The entry's position in the archive (0-based).
        index: usize,
        /// The entry's path, best-effort.
        path: String,
    },
    /// Reading the archive itself failed — a malformed/truncated/corrupt tar, or (for the
    /// in-memory `&[u8]` source this module uses) something that shouldn't be possible short of
    /// a bug, but still an `io::Error` `tar`'s API surface can produce.
    #[error("failed to read tar entry {index}: {source}")]
    Read {
        /// The entry's position in the archive (0-based) — `usize::MAX` if the archive's entry
        /// iterator itself couldn't be constructed, before any entry was reached.
        index: usize,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// An entry passed every check above but still failed to extract (e.g. a filesystem error at
    /// the destination).
    #[error("failed to extract tar entry {index} ({path}): {source}")]
    Extract {
        /// The entry's position in the archive (0-based).
        index: usize,
        /// The entry's path, best-effort.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Best-effort path extraction for an error message — never fails the whole operation just
/// because a path itself couldn't be read (that's surfaced as its own distinct error case).
///
/// # Arguments
///
/// - `entry`: the entry to read the path from.
///
/// # Returns
///
/// The entry's path as a display string, or `"<unreadable path>"` if even that failed.
fn entry_path_for_display<R: std::io::Read>(entry: &tar::Entry<'_, R>) -> String {
    entry.path().map_or_else(
        |_| "<unreadable path>".to_string(),
        |path| path.display().to_string(),
    )
}

/// Validates every entry in `tar_bytes` (rejecting disallowed entry types and oversized content)
/// and extracts it into `dest`, entry by entry, stopping at the first violation — no partial
/// extraction is cleaned up on failure, since the caller owns `dest` (a freshly created,
/// exclusively-owned worker/slot directory — see `docs/DESIGN.md` §11) and is expected to
/// discard the whole thing on any error here, not attempt to salvage a partial extraction.
///
/// # Arguments
///
/// - `tar_bytes`: the raw, untrusted tar archive bytes, as uploaded by the caller.
/// - `dest`: the directory to extract into. Must already exist; not created by this function.
/// - `limits`: the per-entry and cumulative size caps to enforce.
///
/// # Returns
///
/// Nothing, on success — every entry was a regular file or directory, within both size limits,
/// and `tar::Entry::unpack_in` accepted its path as safe.
///
/// # Errors
///
/// Returns the first [`TarValidationError`] encountered, in archive order: a disallowed entry
/// type, an over-limit entry or cumulative size, a read failure, `unpack_in` itself declining an
/// entry, or an extraction I/O failure.
pub fn validate_and_extract(
    tar_bytes: &[u8],
    dest: &Path,
    limits: &TarLimits,
) -> Result<(), TarValidationError> {
    let mut archive = Archive::new(tar_bytes);
    let entries = archive
        .entries()
        .map_err(|source| TarValidationError::Read {
            index: usize::MAX,
            source,
        })?;

    let mut total_bytes: u64 = 0;
    for (index, entry) in entries.enumerate() {
        let mut entry = entry.map_err(|source| TarValidationError::Read { index, source })?;

        let entry_type = entry.header().entry_type();
        if !matches!(entry_type, EntryType::Regular | EntryType::Directory) {
            return Err(TarValidationError::DisallowedEntryType {
                index,
                path: entry_path_for_display(&entry),
                entry_type,
            });
        }

        let size = entry
            .header()
            .size()
            .map_err(|source| TarValidationError::Read { index, source })?;
        if size > limits.max_entry_bytes {
            return Err(TarValidationError::EntryTooLarge {
                index,
                path: entry_path_for_display(&entry),
                size_bytes: size,
                limit_bytes: limits.max_entry_bytes,
            });
        }
        total_bytes = total_bytes.saturating_add(size);
        if total_bytes > limits.max_total_bytes {
            return Err(TarValidationError::TotalTooLarge {
                limit_bytes: limits.max_total_bytes,
            });
        }

        let path_for_error = entry_path_for_display(&entry);
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|source| TarValidationError::Extract {
                index,
                path: path_for_error.clone(),
                source,
            })?;
        if !unpacked {
            return Err(TarValidationError::UnsafeEntry {
                index,
                path: path_for_error,
            });
        }

        // `unpack_in` preserves whatever mode bits the entry's own header specified — under
        // caller control, since the whole tar is untrusted input. A mode too restrictive to read
        // (or, for a directory, to traverse) would silently break the later worker-owned copy,
        // which runs as a *different* uid than this extraction — so force a known-safe mode here
        // rather than trust the upload. No secrets are expected in a project tree, so a uniform
        // world-readable mode is fine.
        let extracted_path = dest.join(
            entry
                .path()
                .map_err(|source| TarValidationError::Read { index, source })?,
        );
        let mode = if entry_type == EntryType::Directory {
            0o755
        } else {
            0o644
        };
        std::fs::set_permissions(&extracted_path, std::fs::Permissions::from_mode(mode)).map_err(
            |source| TarValidationError::Extract {
                index,
                path: path_for_error,
                source,
            },
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{TarLimits, TarValidationError, validate_and_extract};
    use tar::{Builder, Header};

    const GENEROUS_LIMITS: TarLimits = TarLimits {
        max_entry_bytes: 1024 * 1024,
        max_total_bytes: 10 * 1024 * 1024,
    };

    /// Appends one entry with an arbitrary header (letting the caller set fields the higher-level
    /// `append_data`/`append_dir` helpers don't expose, like entry type) and the given content —
    /// used directly for regular files/directories, and by [`append_link_entry`] for
    /// symlinks/hardlinks. Directories get an executable mode (needed to create anything inside
    /// them); everything else gets a plain read/write mode.
    fn append_entry(
        builder: &mut Builder<Vec<u8>>,
        path: &str,
        entry_type: tar::EntryType,
        content: &[u8],
    ) {
        let mut header = Header::new_gnu();
        header.set_path(path).expect("set path");
        header.set_entry_type(entry_type);
        header.set_size(content.len() as u64);
        header.set_mode(if entry_type == tar::EntryType::Directory {
            0o755
        } else {
            0o644
        });
        header.set_cksum();
        builder.append(&header, content).expect("append entry");
    }

    /// Like [`append_entry`], but writes `path` directly into the header's raw `name` bytes,
    /// bypassing `Header::set_path`'s own validation (which refuses to construct a header
    /// containing `..` or an absolute path at all). Real malicious tars aren't built through this
    /// crate's safe `Builder` API — an attacker hand-crafts the bytes directly — so this is what
    /// actually simulates one for testing this module's own defenses, rather than testing that
    /// `Builder` validates its inputs (a different, already-guaranteed property).
    fn append_entry_with_unsafe_path(
        builder: &mut Builder<Vec<u8>>,
        raw_path: &str,
        content: &[u8],
    ) {
        let mut header = Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        let name_field = &mut header.as_old_mut().name;
        let bytes = raw_path.as_bytes();
        name_field[..bytes.len()].copy_from_slice(bytes);
        header.set_cksum();
        builder.append(&header, content).expect("append entry");
    }

    fn append_link_entry(
        builder: &mut Builder<Vec<u8>>,
        path: &str,
        entry_type: tar::EntryType,
        link_target: &str,
    ) {
        let mut header = Header::new_gnu();
        header.set_path(path).expect("set path");
        header.set_entry_type(entry_type);
        header.set_size(0);
        header.set_mode(0o644);
        header.set_link_name(link_target).expect("set link name");
        header.set_cksum();
        builder
            .append(&header, std::io::empty())
            .expect("append link entry");
    }

    fn tar_with_entry(path: &str, entry_type: tar::EntryType, content: &[u8]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        append_entry(&mut builder, path, entry_type, content);
        builder.into_inner().expect("finish tar")
    }

    fn tar_with_unsafe_path(raw_path: &str) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        append_entry_with_unsafe_path(&mut builder, raw_path, b"data");
        builder.into_inner().expect("finish tar")
    }

    fn tar_with_link(path: &str, entry_type: tar::EntryType, link_target: &str) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        append_link_entry(&mut builder, path, entry_type, link_target);
        builder.into_inner().expect("finish tar")
    }

    #[test]
    fn extracts_a_well_formed_project_tree_intact() {
        let dest = tempfile::tempdir().expect("tempdir");
        let mut builder = Builder::new(Vec::new());
        append_entry(&mut builder, "functions", tar::EntryType::Directory, b"");
        append_entry(&mut builder, "migrations", tar::EntryType::Directory, b"");
        append_entry(
            &mut builder,
            "functions/hello",
            tar::EntryType::Directory,
            b"",
        );
        append_entry(
            &mut builder,
            "functions/hello/index.ts",
            tar::EntryType::Regular,
            b"export default () => new Response('ok');",
        );
        let tar_bytes = builder.into_inner().expect("finish tar");

        validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect("extraction succeeds");

        assert!(dest.path().join("functions").is_dir());
        assert!(dest.path().join("migrations").is_dir());
        let extracted = std::fs::read_to_string(dest.path().join("functions/hello/index.ts"))
            .expect("read extracted file");
        assert_eq!(extracted, "export default () => new Response('ok');");
    }

    #[test]
    fn normalizes_extracted_modes_regardless_of_the_tar_headers_own_mode() {
        // A worker-owned privileged copy (a different uid than this extraction) must be able to
        // read every extracted file and traverse every extracted directory afterward — see
        // `service::spawn_task::adopt_project_tar`. An untrusted upload could specify a
        // restrictive mode (accidentally, or adversarially); this must not survive extraction.
        let dest = tempfile::tempdir().expect("tempdir");
        let mut builder = Builder::new(Vec::new());
        let mut dir_header = Header::new_gnu();
        dir_header.set_path("secretdir").expect("set path");
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_size(0);
        dir_header.set_mode(0o700);
        dir_header.set_cksum();
        builder
            .append(&dir_header, std::io::empty())
            .expect("append dir entry");

        let content = b"shh";
        let mut file_header = Header::new_gnu();
        file_header
            .set_path("secretdir/secret.txt")
            .expect("set path");
        file_header.set_entry_type(tar::EntryType::Regular);
        file_header.set_size(content.len() as u64);
        file_header.set_mode(0o600);
        file_header.set_cksum();
        builder
            .append(&file_header, content.as_slice())
            .expect("append file entry");
        let tar_bytes = builder.into_inner().expect("finish tar");

        validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect("extraction succeeds");

        let dir_mode = std::fs::metadata(dest.path().join("secretdir"))
            .expect("stat dir")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(dest.path().join("secretdir/secret.txt"))
            .expect("stat file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o755);
        assert_eq!(file_mode, 0o644);
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let dest = tempfile::tempdir().expect("tempdir");
        let tar_bytes = tar_with_unsafe_path("../escape.txt");

        let error = validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect_err("traversal must be rejected");
        assert!(matches!(error, TarValidationError::UnsafeEntry { .. }));
    }

    #[test]
    fn rejects_absolute_paths_without_escaping_the_destination() {
        let dest = tempfile::tempdir().expect("tempdir");
        let real_passwd_before =
            std::fs::read("/etc/passwd").expect("this test host has a real /etc/passwd");
        let tar_bytes = tar_with_unsafe_path("/etc/passwd");

        // unpack_in strips the leading root component rather than escaping (see this module's
        // doc comment) — so this either extracts harmlessly to dest/etc/passwd, or is rejected;
        // either way, the real /etc/passwd on this host must be untouched.
        let _ = validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS);
        let real_passwd_after = std::fs::read("/etc/passwd").expect("still readable");
        assert_eq!(
            real_passwd_before, real_passwd_after,
            "the real /etc/passwd must never be touched by extracting an untrusted tar"
        );
        assert!(!dest.path().join("passwd").exists());
        assert!(!dest.path().join("..").join("etc").exists());
    }

    #[test]
    fn rejects_symlink_entries() {
        let dest = tempfile::tempdir().expect("tempdir");
        let tar_bytes = tar_with_link("evil-link", tar::EntryType::Symlink, "/etc/passwd");

        let error = validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect_err("symlinks must be rejected");
        assert!(matches!(
            error,
            TarValidationError::DisallowedEntryType {
                entry_type: tar::EntryType::Symlink,
                ..
            }
        ));
    }

    #[test]
    fn rejects_hardlink_entries() {
        let dest = tempfile::tempdir().expect("tempdir");
        let tar_bytes = tar_with_link("evil-hardlink", tar::EntryType::Link, "some-target");

        let error = validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect_err("hardlinks must be rejected");
        assert!(matches!(
            error,
            TarValidationError::DisallowedEntryType {
                entry_type: tar::EntryType::Link,
                ..
            }
        ));
    }

    #[test]
    fn rejects_device_files() {
        let dest = tempfile::tempdir().expect("tempdir");
        let tar_bytes = tar_with_entry("evil-device", tar::EntryType::Char, b"");

        let error = validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect_err("device files must be rejected");
        assert!(matches!(
            error,
            TarValidationError::DisallowedEntryType {
                entry_type: tar::EntryType::Char,
                ..
            }
        ));
    }

    #[test]
    fn rejects_fifo_files() {
        let dest = tempfile::tempdir().expect("tempdir");
        let tar_bytes = tar_with_entry("evil-fifo", tar::EntryType::Fifo, b"");

        let error = validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect_err("fifo files must be rejected");
        assert!(matches!(
            error,
            TarValidationError::DisallowedEntryType {
                entry_type: tar::EntryType::Fifo,
                ..
            }
        ));
    }

    #[test]
    fn rejects_an_entry_over_the_per_entry_limit() {
        let dest = tempfile::tempdir().expect("tempdir");
        let content = vec![b'x'; 100];
        let tar_bytes = tar_with_entry("big.txt", tar::EntryType::Regular, &content);
        let tight_limits = TarLimits {
            max_entry_bytes: 50,
            max_total_bytes: 10 * 1024 * 1024,
        };

        let error = validate_and_extract(&tar_bytes, dest.path(), &tight_limits)
            .expect_err("oversized entry must be rejected");
        assert!(matches!(
            error,
            TarValidationError::EntryTooLarge {
                size_bytes: 100,
                limit_bytes: 50,
                ..
            }
        ));
    }

    #[test]
    fn rejects_when_cumulative_size_exceeds_the_total_limit() {
        let dest = tempfile::tempdir().expect("tempdir");
        let mut builder = Builder::new(Vec::new());
        let content = vec![b'x'; 40];
        for name in ["a.txt", "b.txt", "c.txt"] {
            append_entry(&mut builder, name, tar::EntryType::Regular, &content);
        }
        let tar_bytes = builder.into_inner().expect("finish tar");
        let tight_limits = TarLimits {
            max_entry_bytes: 1024,
            max_total_bytes: 100,
        };

        let error = validate_and_extract(&tar_bytes, dest.path(), &tight_limits)
            .expect_err("cumulative size over the limit must be rejected");
        assert!(matches!(
            error,
            TarValidationError::TotalTooLarge { limit_bytes: 100 }
        ));
    }

    #[test]
    fn extraction_io_failure_is_reported_as_extract_error() {
        use std::os::unix::fs::PermissionsExt;

        // A real, reachable way to make unpack_in itself fail with an io::Error (as opposed to
        // this module's own pre-checks): make the destination unwritable.
        let dest = tempfile::tempdir().expect("tempdir");
        let original_mode = std::fs::metadata(dest.path())
            .expect("stat dest")
            .permissions()
            .mode();
        std::fs::set_permissions(dest.path(), std::fs::Permissions::from_mode(0o555))
            .expect("chmod dest read-only");

        let tar_bytes = tar_with_entry("file.txt", tar::EntryType::Regular, b"data");
        let error = validate_and_extract(&tar_bytes, dest.path(), &GENEROUS_LIMITS)
            .expect_err("extraction into a read-only directory must fail");
        assert!(matches!(error, TarValidationError::Extract { .. }));

        // Restore the exact original mode (not a blanket "world-writable") so tempfile can clean
        // up the directory on drop.
        std::fs::set_permissions(dest.path(), std::fs::Permissions::from_mode(original_mode))
            .expect("restore original permissions");
    }
}
