//! Linked worktree admin scoped to a [`Repository`](crate::repo::Repository).

use std::path::Path;

use crate::error::Result;
use crate::repo::Repository;
use crate::worktree::{
    add_linked_worktree, list_worktrees, remove_linked_worktree, AddLinkedWorktreeOptions,
    WorktreeEntry,
};

/// Options for [`RepositoryWorktrees::lock`].
#[derive(Debug, Clone, Default)]
pub struct LockWorktreeOptions {
    /// Optional reason stored in the `locked` file (shown by `git worktree list`).
    pub reason: Option<String>,
}

/// Worktree listing and linked-worktree mutations for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryWorktrees<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryWorktrees<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Enumerate the main and linked worktrees (main first, then linked by admin id).
    ///
    /// # Errors
    ///
    /// Propagates filesystem and HEAD resolution failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")))?;
    /// let rows = repo.worktrees().list()?;
    /// let _ = rows.first().map(|e| &e.path);
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self) -> Result<Vec<WorktreeEntry>> {
        list_worktrees(self.repo)
    }

    /// Register and populate a linked worktree at `options.path`.
    ///
    /// # Errors
    ///
    /// Propagates [`add_linked_worktree`] failures (bare repo, occupied path, branch conflicts).
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::worktree::AddLinkedWorktreeOptions;
    /// # use std::path::PathBuf;
    /// # fn demo() -> grit_lib::Result<()> {
    /// # let repo = grit_lib::repo::Repository::discover(None)?;
    /// let opts = AddLinkedWorktreeOptions {
    ///     path: PathBuf::from("/tmp/wt"),
    ///     branch: Some("feature".into()),
    ///     new_branch: true,
    ///     detach: false,
    ///     start_point: None,
    /// };
    /// let _ = repo.worktrees().add(&opts);
    /// # Ok(())
    /// # }
    /// ```
    pub fn add(&self, options: &AddLinkedWorktreeOptions) -> Result<WorktreeEntry> {
        add_linked_worktree(self.repo, options)
    }

    /// Remove a linked worktree at `path`.
    ///
    /// # Errors
    ///
    /// Propagates [`remove_linked_worktree`] failures.
    pub fn remove(&self, path: &Path, force: bool) -> Result<()> {
        remove_linked_worktree(self.repo, path, force)
    }

    /// Create or update the `locked` file for the worktree at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is not a registered worktree or is the main worktree.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::repo_handles::LockWorktreeOptions;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::Result<()> {
    /// # let repo = grit_lib::repo::Repository::discover(None)?;
    /// let opts = LockWorktreeOptions {
    ///     reason: Some("maintenance".into()),
    /// };
    /// let _ = repo.worktrees().lock(Path::new("/tmp/wt"), &opts);
    /// # Ok(())
    /// # }
    /// ```
    pub fn lock(&self, path: &Path, options: &LockWorktreeOptions) -> Result<()> {
        lock_linked_worktree(self.repo, path, options.reason.as_deref())
    }
}

/// Write `<admin>/locked` for the linked worktree at `path`.
///
/// # Errors
///
/// Fails when `path` is unknown, is the main worktree, or is already locked.
pub fn lock_linked_worktree(repo: &Repository, path: &Path, reason: Option<&str>) -> Result<()> {
    use crate::git_path::path_for_disk_compare;
    use crate::worktree::common_git_dir;
    use std::fs;

    let common = common_git_dir(&repo.git_dir);
    let target = path_for_disk_compare(path);
    let entry = list_worktrees(repo)?
        .into_iter()
        .find(|e| e.path == target)
        .ok_or_else(|| {
            crate::error::Error::Message(format!("'{target:?}' is not a registered worktree"))
        })?;
    if entry.admin_dir == common {
        return Err(crate::error::Error::Message(
            "cannot lock the main working tree".into(),
        ));
    }
    if entry.is_locked {
        return Err(crate::error::Error::Message(
            entry
                .lock_reason
                .map(|r| format!("worktree is already locked: {r}"))
                .unwrap_or_else(|| "worktree is already locked".into()),
        ));
    }
    let body = reason.unwrap_or("");
    fs::write(entry.admin_dir.join("locked"), body).map_err(crate::error::Error::Io)?;
    Ok(())
}
