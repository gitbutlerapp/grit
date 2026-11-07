//! Rerere helpers scoped to [`Merge`](super::Merge).

use crate::config::ConfigSet;
use crate::environment::Environment;
use crate::repo::Repository;
use crate::rerere::{
    finish_merge_rerere, repo_rerere, rerere_clear, rerere_diff_for_path, rerere_enabled,
    rerere_forget_path, rerere_gc, rerere_post_commit, rerere_remaining_lines, rerere_status_lines,
    RerereAutoupdate, RerereEvent,
};

/// Recorded conflict resolution (`rerere`) for one repository.
///
/// Returned from [`Merge::rerere`](super::Merge::rerere).
#[derive(Debug, Copy, Clone)]
pub struct MergeRerere<'repo> {
    repo: &'repo Repository,
}

impl<'repo> MergeRerere<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Whether `rerere` is active for this repository.
    pub fn enabled(&self, config: &ConfigSet) -> bool {
        rerere_enabled(config, self.repo.git_dir())
    }

    /// Replay or record rerere resolutions for the current conflict set.
    pub fn apply(&self, autoupdate: RerereAutoupdate) -> crate::error::Result<Vec<RerereEvent>> {
        repo_rerere(self.repo, autoupdate)
    }

    /// Rerere after a merge commit (including autoupdate record).
    pub fn post_commit(&self) -> crate::error::Result<Vec<RerereEvent>> {
        rerere_post_commit(self.repo)
    }

    /// Rerere cleanup when finishing a merge without a new commit.
    pub fn finish_merge(&self) -> crate::error::Result<Vec<RerereEvent>> {
        finish_merge_rerere(self.repo)
    }

    /// Clear all recorded rerere state under `MERGE_RR` and `rr-cache`.
    pub fn clear(&self) -> crate::error::Result<()> {
        rerere_clear(self.repo.git_dir())
    }

    /// Remove stale rerere entries (same rules as `git rerere gc`).
    pub fn gc(&self, env: &Environment, now: i64) -> crate::error::Result<()> {
        rerere_gc(env, self.repo.git_dir(), now)
    }

    /// `git rerere status` lines for active conflicts.
    pub fn status_lines(&self) -> crate::error::Result<Vec<String>> {
        rerere_status_lines(self.repo)
    }

    /// Paths that still have unresolved rerere entries.
    pub fn remaining_lines(&self) -> crate::error::Result<Vec<String>> {
        rerere_remaining_lines(self.repo)
    }

    /// Unified diff for the recorded resolution of `path`, if any.
    pub fn diff_for_path(&self, path: &str) -> crate::error::Result<Option<String>> {
        rerere_diff_for_path(self.repo, path)
    }

    /// Drop rerere state for `path`.
    pub fn forget_path(&self, path: &str) -> crate::error::Result<Vec<RerereEvent>> {
        rerere_forget_path(self.repo, path)
    }
}
