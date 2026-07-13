//! Resolves each configured client's Unix account uid/gid from `/etc/passwd`-format text. Needed
//! because Docker's `--user <uid>:<gid>` can't resolve a host-only account name from inside the
//! container — only numeric ids work — while `sudo -u <name>` (used elsewhere) works from the name
//! alone. The accounts themselves are provisioned by an admin ahead of time (see
//! `docs/DESIGN.md`); this just reads back what the OS already knows about them.
//!
//! The path to the passwd file is a parameter (default `/etc/passwd`, overridden in tests) so
//! this is testable by pointing it at a temp file shaped like `/etc/passwd`, without needing the
//! real client accounts to exist on the machine running the tests.

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;

use crate::domain::ids::{ClientId, WorkerUser};

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

/// Resolves each `(ClientId, unix account name)` pair's uid/gid against a passwd file, building
/// the mapping [`crate::client_workers::ClientWorkers`] is constructed from.
///
/// # Arguments
///
/// - `passwd_path`: path to a `/etc/passwd`-format file to read and parse. `/etc/passwd` in
///   production; a temp file shaped like it in tests.
/// - `clients`: one `(client id, configured unix account name)` pair per `[[clients]]` entry.
///
/// # Returns
///
/// A [`WorkerUser`] (carrying the resolved uid/gid) per client, keyed by [`ClientId`].
///
/// # Errors
///
/// Returns [`WorkerResolutionError::Read`] if `passwd_path` can't be read, or
/// [`WorkerResolutionError::AccountNotFound`] / [`WorkerResolutionError::MalformedEntry`] if any
/// configured client's account is missing or has an unparseable entry — a misconfigured client
/// account fails loudly at startup rather than silently running with a wrong/missing mapping.
pub async fn resolve_client_workers(
    passwd_path: &Path,
    clients: &[(ClientId, String)],
) -> Result<HashMap<ClientId, WorkerUser>, WorkerResolutionError> {
    let contents = tokio::fs::read_to_string(passwd_path)
        .await
        .map_err(|source| WorkerResolutionError::Read {
            path: passwd_path.display().to_string(),
            source,
        })?;

    clients
        .iter()
        .map(|(client_id, unix_user)| {
            let (uid, gid) = parse_uid_gid(&contents, unix_user)?;
            Ok((
                client_id.clone(),
                WorkerUser::new(unix_user.clone(), uid, gid),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{WorkerResolutionError, parse_uid_gid, resolve_client_workers};
    use crate::domain::ids::ClientId;

    const SAMPLE_PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
openbrain-agent:x:2000:2000::/home/openbrain-agent:/usr/sbin/nologin
rainqueue-agent:x:2001:2001::/home/rainqueue-agent:/usr/sbin/nologin
malformed-entry:x:notanumber:2002::/home/malformed-entry:/usr/sbin/nologin
";

    #[test]
    fn parses_uid_gid_for_known_account() {
        let (uid, gid) = parse_uid_gid(SAMPLE_PASSWD, "openbrain-agent").expect("account present");
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
    async fn resolve_client_workers_reads_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("passwd");
        tokio::fs::write(&path, SAMPLE_PASSWD)
            .await
            .expect("write fake passwd");

        let clients = vec![
            (ClientId::new("openbrain"), "openbrain-agent".to_string()),
            (ClientId::new("rainqueue"), "rainqueue-agent".to_string()),
        ];
        let workers = resolve_client_workers(&path, &clients)
            .await
            .expect("resolves");
        assert_eq!(workers.len(), 2);
        let openbrain = &workers[&ClientId::new("openbrain")];
        assert_eq!(openbrain.as_str(), "openbrain-agent");
        assert_eq!(openbrain.uid(), 2000);
        let rainqueue = &workers[&ClientId::new("rainqueue")];
        assert_eq!(rainqueue.as_str(), "rainqueue-agent");
        assert_eq!(rainqueue.uid(), 2001);
    }

    #[tokio::test]
    async fn resolve_client_workers_errors_when_an_account_is_not_provisioned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("passwd");
        tokio::fs::write(&path, SAMPLE_PASSWD)
            .await
            .expect("write fake passwd");

        let clients = vec![(ClientId::new("openbrain"), "nonexistent-agent".to_string())];
        let err = resolve_client_workers(&path, &clients)
            .await
            .expect_err("account not provisioned");
        assert!(matches!(err, WorkerResolutionError::AccountNotFound { .. }));
    }

    #[tokio::test]
    async fn resolve_client_workers_errors_on_missing_file() {
        let clients = vec![(ClientId::new("openbrain"), "openbrain-agent".to_string())];
        let err = resolve_client_workers(std::path::Path::new("/nonexistent/passwd"), &clients)
            .await
            .expect_err("missing file");
        assert!(matches!(err, WorkerResolutionError::Read { .. }));
    }

    #[tokio::test]
    async fn resolve_client_workers_with_no_clients_returns_empty_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("passwd");
        tokio::fs::write(&path, SAMPLE_PASSWD)
            .await
            .expect("write fake passwd");

        let workers = resolve_client_workers(&path, &[]).await.expect("resolves");
        assert!(workers.is_empty());
    }
}
