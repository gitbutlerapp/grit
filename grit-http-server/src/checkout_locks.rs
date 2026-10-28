//! What the server serializes for snapshot checkouts ([`crate::checkouts`]).
//!
//! Each served repository gets a [`RepoLocks`]:
//!
//! - its branches ([`RepoLocks::branches`]), held by a check-in from reading
//!   a branch to setting it and while a new checkout's work-branch name is
//!   given out, so two check-ins of one branch take turns and the second sees
//!   the first's commits;
//! - one lock per checkout ([`RepoLocks::checkout`]), held for a whole
//!   request on that checkout (check-in, refresh), so its record changes one
//!   request at a time. Different checkouts don't wait for each other;
//! - its target branch ([`RepoLocks::target`]), held by every request that
//!   moves it (integrate, slice changes), so they take turns: each merges
//!   onto the target the one before it left, instead of all but one losing
//!   the race and being asked to try again.
//!
//! The branch and record writes are compare-and-swap and atomic in the
//! library as well (which starts a change over when the target moved under
//! it), so these locks keep requests in one server from tripping over each
//! other; they are not what keeps the data safe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use grit_lib::objects::ObjectId;
use grit_lib::remote::snapshot_checkout::{CheckoutRecord, CheckoutStore, TargetChange};

/// How many checkout locks a repository keeps before dropping the ones no
/// request holds or waits for.
const KEPT_CHECKOUT_LOCKS: usize = 1024;

/// The checkout locks of every served repository: what checkout requests
/// on one repository take turns on. A server hosting the checkout endpoints
/// keeps one for as long as it runs (see
/// [`CheckoutHost::checkout_locks`](crate::checkouts::CheckoutHost::checkout_locks)).
#[derive(Debug, Default)]
pub struct CheckoutLocks {
    repos: Mutex<HashMap<PathBuf, Arc<RepoLocks>>>,
}

impl CheckoutLocks {
    /// The locks of the repository at `path` (a served repository's directory).
    pub(crate) fn repo(&self, path: &Path) -> Arc<RepoLocks> {
        let mut repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
        repos.entry(path.to_path_buf()).or_default().clone()
    }
}

/// One served repository's checkout locks.
#[derive(Debug, Default)]
pub(crate) struct RepoLocks {
    branches: Mutex<BranchNames>,
    checkouts: Mutex<HashMap<String, Arc<tokio::sync::Mutex<CheckoutSlot>>>>,
    target: Arc<tokio::sync::Mutex<TargetSlot>>,
}

impl RepoLocks {
    /// Hold the repository's branches: while setting a branch from a check-in,
    /// or giving out a work-branch name. Blocks; call it from blocking code.
    pub(crate) fn branches(&self) -> MutexGuard<'_, BranchNames> {
        self.branches.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait for checkout `id` to be free, and hold it.
    pub(crate) async fn checkout(&self, id: &str) -> tokio::sync::OwnedMutexGuard<CheckoutSlot> {
        let lock = {
            let mut all = self.checkouts.lock().unwrap_or_else(|e| e.into_inner());
            if all.len() >= KEPT_CHECKOUT_LOCKS {
                // A lock nobody holds or waits for is only the map's.
                all.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            all.entry(id.to_owned()).or_default().clone()
        };
        lock.lock_owned().await
    }

    /// Wait for the repository's target branch to be free, and hold it while
    /// moving it.
    pub(crate) async fn target(&self) -> tokio::sync::OwnedMutexGuard<TargetSlot> {
        self.target.clone().lock_owned().await
    }
}

/// What the server last did to a repository's target branch, kept by whoever
/// holds [`RepoLocks::target`].
#[derive(Debug, Default)]
pub(crate) struct TargetSlot {
    /// Where the server last left the target.
    last: Option<ObjectId>,
}

impl TargetSlot {
    /// Note that a change of the target was made on `change.previous` and left
    /// it at `change.head`.
    ///
    /// # Returns
    ///
    /// Where the server had left the target before, when something else
    /// (a process outside this server) moved it since.
    pub(crate) fn changed(&mut self, change: &TargetChange) -> Option<ObjectId> {
        let moved_outside = self.last.filter(|last| *last != change.previous);
        self.last = Some(change.head);
        moved_outside
    }
}

/// Where numbering of each asked-for work-branch name got to, so a name
/// asked for many times isn't counted up from the start again. The
/// reservations in the [`CheckoutStore`] decide; this only skips ahead.
#[derive(Debug, Default)]
pub(crate) struct BranchNames {
    next: HashMap<String, u64>,
}

impl BranchNames {
    /// Most names kept before the hints start over.
    const KEPT: usize = 4096;

    /// Where to start numbering `wanted`.
    pub(crate) fn start(&self, wanted: &str) -> u64 {
        self.next.get(wanted).copied().unwrap_or(1)
    }

    /// Numbering of `wanted` got to `next`.
    pub(crate) fn reached(&mut self, wanted: &str, next: u64) {
        if self.next.len() >= Self::KEPT {
            self.next.clear();
        }
        self.next.insert(wanted.to_owned(), next);
    }
}

/// One checkout's record, opened by the first request that needs it and
/// written only by whoever holds the slot.
#[derive(Debug, Default)]
pub(crate) struct CheckoutSlot {
    record: Option<CheckoutRecord>,
}

impl CheckoutSlot {
    /// The record of checkout `id` in `store`; `None` for an id that can't
    /// name a checkout.
    pub(crate) fn record(
        &mut self,
        store: &CheckoutStore,
        id: &str,
    ) -> Option<&mut CheckoutRecord> {
        if self.record.is_none() {
            self.record = store.record(id);
        }
        self.record.as_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_hints_skip_ahead_per_name() {
        let mut names = BranchNames::default();
        assert_eq!(names.start("alice/x"), 1);
        names.reached("alice/x", 3);
        assert_eq!(names.start("alice/x"), 3);
        assert_eq!(names.start("bob/y"), 1);
    }

    #[tokio::test]
    async fn target_changes_take_turns_and_notice_outside_moves() {
        let locks = CheckoutLocks::default();
        let repo = locks.repo(Path::new("/srv/demo"));
        let oid = |n: u8| ObjectId::from_bytes(&[n; 20]).unwrap_or_else(|_| ObjectId::zero());
        let change = |previous, head| TargetChange {
            previous: oid(previous),
            head: oid(head),
            worktree_moved: false,
        };
        let mut held = repo.target().await;
        assert_eq!(held.changed(&change(1, 2)), None);
        assert_eq!(held.changed(&change(2, 3)), None);
        assert_eq!(held.changed(&change(9, 10)), Some(oid(3)));
        let waiting = {
            let repo = repo.clone();
            tokio::spawn(async move { repo.target().await.last })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(held);
        assert_eq!(waiting.await.ok().flatten(), Some(oid(10)));
    }

    #[tokio::test]
    async fn one_checkout_waits_for_itself_but_not_for_others() {
        let locks = CheckoutLocks::default();
        let repo = locks.repo(Path::new("/srv/demo"));
        assert!(Arc::ptr_eq(&repo, &locks.repo(Path::new("/srv/demo"))));
        let held = repo.checkout("a").await;
        // Another checkout is free.
        let other = repo.checkout("b").await;
        drop(other);
        // The same one waits until the first is released.
        let waiting = {
            let repo = repo.clone();
            tokio::spawn(async move { repo.checkout("a").await.record.is_none() })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(held);
        assert!(waiting.await.unwrap_or(false));
    }
}
