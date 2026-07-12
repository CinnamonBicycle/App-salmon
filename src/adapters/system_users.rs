//! Resolves worker account uid/gid from `/etc/passwd`-format text. Needed because Docker's
//! `--user <uid>:<gid>` can't resolve a host-only account name from inside the container — only
//! numeric ids work — while `sudo -u <name>` (used elsewhere) works from the name alone. The
//! worker accounts themselves are provisioned by an admin ahead of time (see `docs/DESIGN.md`);
//! this just reads back what the OS already knows about them.
//!
//! The path to the passwd file is a parameter (default `/etc/passwd`, overridden in tests) so
//! this is testable by pointing it at a temp file shaped like `/etc/passwd`, without needing the
//! real worker accounts to exist on the machine running the tests.

use std::path::Path;

use thiserror::Error;

use crate::domain::ids::WorkerUser;

#[derive(Debug, Error)]
pub enum WorkerResolutionError {
    /// The passwd file at `path` could not be read (e.g. missing, or permission denied).
    #[error("failed to read passwd file {path}: {source}")]
    Read {
        /// The path that was attempted, for the error message.
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// No line in the passwd file's contents had `name` as its first (username) field.
    #[error("account {name} not found in passwd file")]
    AccountNotFound {
        /// The account name that was looked up and not found.
        name: String,
    },
    /// A line for `name` was found, but its uid or gid field wasn't a parseable number.
    #[error("malformed passwd entry for {name}: uid/gid fields are not valid numbers")]
    MalformedEntry {
        /// The account name whose passwd entry failed to parse.
        name: String,
    },
}

/// Pure: parses already-loaded `/etc/passwd`-format text for one account's `uid:gid`. Each line
/// is `name:passwd:uid:gid:gecos:home:shell`.
///
/// # Arguments
///
/// - `contents`: the full text of a `/etc/passwd`-format file, already read into memory.
/// - `name`: the account (username) to look up.
///
/// # Returns
///
/// The `(uid, gid)` pair parsed from `name`'s passwd entry.
///
/// # Errors
///
/// Returns [`WorkerResolutionError::AccountNotFound`] if no line's first field matches `name`, or
/// [`WorkerResolutionError::MalformedEntry`] if a matching line's uid/gid fields aren't valid
/// numbers.
fn parse_uid_gid(contents: &str, name: &str) -> Result<(u32, u32), WorkerResolutionError> {
    let line = contents
        .lines()
        .find(|line| line.split(':').next() == Some(name))
        .ok_or_else(|| WorkerResolutionError::AccountNotFound {
            name: name.to_string(),
        })?;

    let malformed = || WorkerResolutionError::MalformedEntry {
        name: name.to_string(),
    };
    let mut fields = line.split(':').skip(2);
    let uid: u32 = fields
        .next()
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    let gid: u32 = fields
        .next()
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    Ok((uid, gid))
}

/// Resolves `{user_prefix}00`, `{user_prefix}01`, ... up to `count` accounts, in order.
///
/// # Arguments
///
/// - `passwd_path`: path to a `/etc/passwd`-format file to read and parse. `/etc/passwd` in
///   production; a temp file shaped like it in tests.
/// - `user_prefix`: the common prefix shared by every worker account name (e.g.
///   `"salmon-worker-"`).
/// - `count`: how many sequentially-numbered accounts to resolve, starting from `00`.
///
/// # Returns
///
/// A [`WorkerUser`] for each of the `count` accounts, in order (`00`, `01`, ...), each carrying
/// the uid/gid resolved from the passwd file.
///
/// # Errors
///
/// Returns [`WorkerResolutionError::Read`] if `passwd_path` can't be read, or
/// [`WorkerResolutionError::AccountNotFound`] / [`WorkerResolutionError::MalformedEntry`] if any
/// expected account is missing or has an unparseable entry — a misconfigured worker pool fails
/// loudly at startup rather than silently running with fewer workers than configured.
pub async fn resolve_worker_users(
    passwd_path: &Path,
    user_prefix: &str,
    count: usize,
) -> Result<Vec<WorkerUser>, WorkerResolutionError> {
    let contents = tokio::fs::read_to_string(passwd_path)
        .await
        .map_err(|source| WorkerResolutionError::Read {
            path: passwd_path.display().to_string(),
            source,
        })?;

    (0..count)
        .map(|i| {
            let name = format!("{user_prefix}{i:02}");
            let (uid, gid) = parse_uid_gid(&contents, &name)?;
            Ok(WorkerUser::new(name, uid, gid))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{WorkerResolutionError, parse_uid_gid, resolve_worker_users};

    const SAMPLE_PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
salmon-worker-00:x:2000:2000::/home/salmon-worker-00:/usr/sbin/nologin
salmon-worker-01:x:2001:2001::/home/salmon-worker-01:/usr/sbin/nologin
malformed-entry:x:notanumber:2002::/home/malformed-entry:/usr/sbin/nologin
";

    #[test]
    fn parses_uid_gid_for_known_account() {
        let (uid, gid) = parse_uid_gid(SAMPLE_PASSWD, "salmon-worker-00").expect("account present");
        assert_eq!((uid, gid), (2000, 2000));
    }

    #[test]
    fn errors_on_unknown_account() {
        let err = parse_uid_gid(SAMPLE_PASSWD, "nobody").expect_err("account missing");
        assert!(matches!(err, WorkerResolutionError::AccountNotFound { .. }));
    }

    #[test]
    fn errors_on_non_numeric_uid() {
        let err = parse_uid_gid(SAMPLE_PASSWD, "malformed-entry").expect_err("bad uid");
        assert!(matches!(err, WorkerResolutionError::MalformedEntry { .. }));
    }

    #[tokio::test]
    async fn resolve_worker_users_reads_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("passwd");
        tokio::fs::write(&path, SAMPLE_PASSWD)
            .await
            .expect("write fake passwd");

        let workers = resolve_worker_users(&path, "salmon-worker-", 2)
            .await
            .expect("resolves");
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0].as_str(), "salmon-worker-00");
        assert_eq!(workers[0].uid(), 2000);
        assert_eq!(workers[1].as_str(), "salmon-worker-01");
        assert_eq!(workers[1].uid(), 2001);
    }

    #[tokio::test]
    async fn resolve_worker_users_errors_when_pool_exceeds_provisioned_accounts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("passwd");
        tokio::fs::write(&path, SAMPLE_PASSWD)
            .await
            .expect("write fake passwd");

        let err = resolve_worker_users(&path, "salmon-worker-", 5)
            .await
            .expect_err("only 2 provisioned");
        assert!(matches!(err, WorkerResolutionError::AccountNotFound { .. }));
    }

    #[tokio::test]
    async fn resolve_worker_users_errors_on_missing_file() {
        let err = resolve_worker_users(
            std::path::Path::new("/nonexistent/passwd"),
            "salmon-worker-",
            1,
        )
        .await
        .expect_err("missing file");
        assert!(matches!(err, WorkerResolutionError::Read { .. }));
    }
}
