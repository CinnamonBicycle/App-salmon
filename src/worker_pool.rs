//! A bounded free-list of pre-provisioned Unix worker accounts (`salmon-worker-00`, ...),
//! allocated 1:1 to a cluster for its lifetime.
//!
//! This has no I/O of its own — preparing/wiping a worker's directory goes through the injected
//! `PrivilegedExecutor` at the call site (`service::spawn_task` / `service::teardown_task`), not
//! through this type. `WorkerPool` only tracks which accounts are currently free vs. allocated,
//! which is why it's a concrete struct rather than a trait: there's nothing external to fake.
//!
//! Phase-1 security note (see `docs/DESIGN.md`): this is a file-ownership/attribution boundary,
//! not a container-escape boundary — the Docker daemon that actually runs containers still runs
//! as root either way.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::sync::Mutex;

use crate::domain::ids::WorkerUser;
use crate::ports::privileged_exec::PrivilegedExecError;

/// The on-disk directory a worker's containers bind-mount into, wiped and reused across
/// clusters. Shared by `service::spawn_task`/`service::teardown_task` (which prepare/wipe it via
/// `PrivilegedExecutor`) and `backends::postgres` (which bind-mounts it into the container), so
/// both sides agree on the path from the worker alone.
/// Computes the on-disk directory a worker's containers bind-mount into.
///
/// # Arguments
///
/// - `base`: the configured base directory all worker directories live under.
/// - `worker`: the worker account whose directory to compute.
///
/// # Returns
///
/// `base` joined with `worker`'s account name.
#[must_use]
pub fn worker_data_dir(base: &Path, worker: &WorkerUser) -> PathBuf {
    base.join(worker.as_str())
}

/// Errors from [`WorkerPool`] operations.
#[derive(Debug, Error)]
pub enum WorkerPoolError {
    /// Every configured worker account is currently allocated.
    #[error("worker pool exhausted (size {pool_size})")]
    PoolExhausted {
        /// The pool's total configured size.
        pool_size: usize,
    },
    /// The `PrivilegedExecutor` call to create/`chown` a newly acquired worker's directory
    /// failed.
    #[error("failed to prepare worker directory: {0}")]
    Prepare(#[source] PrivilegedExecError),
    /// The `PrivilegedExecutor` call to wipe a worker's directory on release failed.
    #[error("failed to wipe worker state on release: {0}")]
    Wipe(#[source] PrivilegedExecError),
    /// A caller tried to release a worker that wasn't currently allocated — a caller bug, since
    /// every acquired worker should be released exactly once.
    #[error("worker {worker} released while not allocated (bug: double release)")]
    DoubleRelease {
        /// The worker that was released without a matching prior acquisition.
        worker: WorkerUser,
    },
}

/// The pool's mutable bookkeeping, guarded by [`WorkerPool`]'s mutex.
struct PoolState {
    /// Accounts currently available to [`WorkerPool::acquire`].
    free: VecDeque<WorkerUser>,
    /// Accounts currently allocated to a cluster, awaiting [`WorkerPool::release`].
    allocated: HashSet<WorkerUser>,
}

/// A bounded free-list of pre-provisioned Unix worker accounts. See the module docs for the
/// security model this provides (a file-ownership boundary, not a container-escape boundary).
pub struct WorkerPool {
    /// The free/allocated bookkeeping.
    state: Mutex<PoolState>,
    /// The pool's total configured size (free + allocated, always constant after construction).
    size: usize,
}

impl WorkerPool {
    /// Constructs a pool starting from a given set of free accounts. `workers` is the full set of
    /// currently-free accounts to start from — at startup this is "the configured pool minus
    /// whatever reconciliation found still referenced by a non-absent row", so a freshly
    /// constructed pool already reflects reality after a restart.
    ///
    /// # Arguments
    ///
    /// - `workers`: the accounts to start the pool with, all initially free.
    ///
    /// # Returns
    ///
    /// The constructed pool, with `size()` equal to `workers.len()`.
    #[must_use]
    pub fn new(workers: Vec<WorkerUser>) -> Self {
        let size = workers.len();
        Self {
            state: Mutex::new(PoolState {
                free: workers.into(),
                allocated: HashSet::new(),
            }),
            size,
        }
    }

    /// Returns the pool's total configured size.
    ///
    /// # Returns
    ///
    /// The number of worker accounts this pool was constructed with (free + allocated).
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Takes one worker account from the free list and marks it allocated.
    ///
    /// # Returns
    ///
    /// The acquired worker account.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerPoolError::PoolExhausted`] if every worker is currently allocated.
    pub async fn acquire(&self) -> Result<WorkerUser, WorkerPoolError> {
        let mut state = self.state.lock().await;
        let worker = state
            .free
            .pop_front()
            .ok_or(WorkerPoolError::PoolExhausted {
                pool_size: self.size,
            })?;
        state.allocated.insert(worker.clone());
        Ok(worker)
    }

    /// Returns a previously acquired worker account to the free list.
    ///
    /// # Arguments
    ///
    /// - `worker`: the account to release; must have been returned by a prior [`Self::acquire`]
    ///   call that hasn't already been released.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerPoolError::DoubleRelease`] if `worker` was not currently allocated (a
    /// caller bug — every acquired worker should be released exactly once).
    pub async fn release(&self, worker: WorkerUser) -> Result<(), WorkerPoolError> {
        let mut state = self.state.lock().await;
        if !state.allocated.remove(&worker) {
            return Err(WorkerPoolError::DoubleRelease { worker });
        }
        state.free.push_back(worker);
        Ok(())
    }

    /// Returns how many worker accounts are currently free (not allocated).
    ///
    /// # Returns
    ///
    /// The number of accounts currently on the free list.
    #[must_use]
    pub async fn free_count(&self) -> usize {
        self.state.lock().await.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkerPool, WorkerPoolError};
    use crate::domain::ids::WorkerUser;

    fn workers(n: usize) -> Vec<WorkerUser> {
        (0..n)
            .map(|i| {
                let uid = 2000 + u32::try_from(i).unwrap_or(0);
                WorkerUser::new(format!("salmon-worker-{i:02}"), uid, uid)
            })
            .collect()
    }

    #[tokio::test]
    async fn acquire_returns_a_free_worker() {
        let pool = WorkerPool::new(workers(2));
        let worker = pool.acquire().await.expect("worker available");
        assert!(worker.as_str().starts_with("salmon-worker-"));
        assert_eq!(pool.free_count().await, 1);
    }

    #[tokio::test]
    async fn acquire_exhausts_and_errors() {
        let pool = WorkerPool::new(workers(1));
        pool.acquire().await.expect("first acquire succeeds");
        let err = pool.acquire().await.expect_err("pool exhausted");
        assert!(matches!(
            err,
            WorkerPoolError::PoolExhausted { pool_size: 1 }
        ));
    }

    #[tokio::test]
    async fn release_returns_worker_to_free_list() {
        let pool = WorkerPool::new(workers(1));
        let worker = pool.acquire().await.expect("acquire");
        assert_eq!(pool.free_count().await, 0);
        pool.release(worker).await.expect("release");
        assert_eq!(pool.free_count().await, 1);
    }

    #[tokio::test]
    async fn release_without_prior_acquire_is_double_release() {
        let pool = WorkerPool::new(workers(1));
        let phantom = WorkerUser::new("salmon-worker-99", 2099, 2099);
        let err = pool
            .release(phantom.clone())
            .await
            .expect_err("double release");
        assert!(matches!(err, WorkerPoolError::DoubleRelease { worker } if worker == phantom));
    }

    #[tokio::test]
    async fn releasing_the_same_worker_twice_is_double_release() {
        let pool = WorkerPool::new(workers(1));
        let worker = pool.acquire().await.expect("acquire");
        pool.release(worker.clone()).await.expect("first release");
        let err = pool.release(worker).await.expect_err("second release");
        assert!(matches!(err, WorkerPoolError::DoubleRelease { .. }));
    }

    #[test]
    fn size_reflects_initial_capacity() {
        let pool = WorkerPool::new(workers(5));
        assert_eq!(pool.size(), 5);
    }

    #[test]
    fn worker_pool_error_display_messages() {
        assert_eq!(
            WorkerPoolError::PoolExhausted { pool_size: 3 }.to_string(),
            "worker pool exhausted (size 3)"
        );
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        assert_eq!(
            WorkerPoolError::DoubleRelease {
                worker: worker.clone()
            }
            .to_string(),
            "worker salmon-worker-00 released while not allocated (bug: double release)"
        );
    }

    #[test]
    fn worker_data_dir_joins_base_and_worker_name() {
        let worker = WorkerUser::new("salmon-worker-07", 2007, 2007);
        let path =
            super::worker_data_dir(std::path::Path::new("/var/lib/app_salmon/workers"), &worker);
        assert_eq!(
            path,
            std::path::PathBuf::from("/var/lib/app_salmon/workers/salmon-worker-07")
        );
    }
}
