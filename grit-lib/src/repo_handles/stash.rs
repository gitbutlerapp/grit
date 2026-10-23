//! Stash stack operations scoped to a [`Repository`](crate::repo::Repository).

use std::path::Path;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::hunk_select::HunkSelection;
use crate::porcelain::stash::{
    apply_stash_entry, create_stash, drop_stash, list_stashes, pop_stash, push_stash,
    StashApplyOptions as StashEntryApplyOptions, StashCreateOptions, StashEntry,
};
use crate::repo::Repository;

/// Options for [`RepositoryStash::push`].
#[derive(Debug, Clone)]
pub struct StashPushOptions {
    /// Reflog/stash message override.
    pub message: Option<String>,
    /// Include untracked files as a third parent on the stash commit.
    pub include_untracked: bool,
    /// Full author/committer identity (`Name <email> timestamp tz`).
    pub identity: String,
    /// When set, stash only these unstaged hunks (index unchanged on push).
    pub selection: Option<HunkSelection>,
    /// Pathspecs narrowing hunk resolution.
    pub pathspecs: Vec<String>,
    /// Preview without updating refs, index, or worktree.
    pub dry_run: bool,
}

/// Options for [`RepositoryStash::apply`] and [`RepositoryStash::pop`].
#[derive(Debug, Clone)]
pub struct StashApplyOptions {
    /// Index in newest-first order (`0` is `stash@{0}`).
    pub index: usize,
    /// When true, restore the stashed index; otherwise keep the current index at touched paths.
    pub restore_index: bool,
    /// Reflog identity for drop-after-pop.
    pub identity: String,
}

/// Stash list/create/apply/pop/drop for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryStash<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryStash<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// List stash entries from `refs/stash` reflog (newest first).
    ///
    /// # Errors
    ///
    /// Propagates reflog read failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let entries = repo.stash().list()?;
    /// let _ = entries.len();
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self) -> Result<Vec<StashEntry>> {
        list_stashes(self.repo)
    }

    /// Create a stash commit and update `refs/stash`, resetting the worktree to `HEAD`.
    ///
    /// Returns `None` when there is nothing to stash.
    ///
    /// # Errors
    ///
    /// Propagates stash build, ref, and checkout failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::repo_handles::StashPushOptions;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let opts = StashPushOptions {
    ///     message: Some("wip".into()),
    ///     include_untracked: false,
    ///     identity: "User <u@example.com> 1000000000 +0000".into(),
    ///     selection: None,
    ///     pathspecs: Vec::new(),
    ///     dry_run: false,
    /// };
    /// let _ = repo.stash().push(&opts)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn push(&self, options: &StashPushOptions) -> Result<Option<ObjectId>> {
        let inner = StashCreateOptions {
            message: options.message.clone(),
            include_untracked: options.include_untracked,
            identity: options.identity.clone(),
            selection: options.selection.clone(),
            pathspecs: options.pathspecs.clone(),
            dry_run: options.dry_run,
        };
        push_stash(self.repo, &inner).map(|o| o.map(|r| r.oid))
    }

    /// Build a stash commit without updating `refs/stash`.
    ///
    /// # Errors
    ///
    /// Same as [`create_stash`].
    pub fn create(&self, options: &StashPushOptions) -> Result<Option<ObjectId>> {
        let inner = StashCreateOptions {
            message: options.message.clone(),
            include_untracked: options.include_untracked,
            identity: options.identity.clone(),
            selection: options.selection.clone(),
            pathspecs: options.pathspecs.clone(),
            dry_run: options.dry_run,
        };
        create_stash(self.repo, &inner)
    }

    /// Apply `stash@{options.index}` without dropping it.
    ///
    /// Returns `true` when merge conflicts occurred.
    ///
    /// # Errors
    ///
    /// Propagates apply failures.
    pub fn apply(&self, work_tree: &Path, options: &StashApplyOptions) -> Result<bool> {
        Ok(apply_stash_entry(
            self.repo,
            work_tree,
            options.index,
            &StashEntryApplyOptions {
                restore_index: options.restore_index,
            },
        )?
        .has_conflicts())
    }

    /// Apply `stash@{options.index}` and drop it when there are no conflicts.
    ///
    /// Returns `true` when conflicts occurred (entry is kept).
    ///
    /// # Errors
    ///
    /// Propagates apply and drop failures.
    pub fn pop(&self, work_tree: &Path, options: &StashApplyOptions) -> Result<bool> {
        pop_stash(
            self.repo,
            work_tree,
            options.index,
            options.restore_index,
            &options.identity,
        )
    }

    /// Remove `stash@{index}` from the reflog.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::StashNotFound`] when `index` is out of range.
    pub fn drop(&self, index: usize, identity: &str) -> Result<()> {
        drop_stash(self.repo, index, identity)
    }
}
