//! [`Commits`] — record and preview commits.

use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::commit::{
    commit_with_scope, create_commit, default_commit_scope, pending_commit_message, plan_commit,
    prepare_commit_bytes, write_commit_object, CommitOutcome, CommitPlan, CommitRequest,
    CommitScope,
};
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;

/// Request plus scope for [`Commits::create`] and [`Commits::plan`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CommitOptions {
    /// Author, message, hooks, amend flags, and related commit inputs.
    pub request: CommitRequest,
    /// Which paths and staging mode apply (`AutoStage`, `Staged`, pathspecs, or patch).
    pub scope: CommitScope,
}

impl CommitOptions {
    /// Start options from `request` and `scope`.
    #[must_use]
    pub fn new(request: CommitRequest, scope: CommitScope) -> Self {
        Self { request, scope }
    }

    /// Sets [`request`](Self::request).
    #[must_use]
    pub fn with_request(mut self, request: CommitRequest) -> Self {
        self.request = request;
        self
    }

    /// Sets [`scope`](Self::scope).
    #[must_use]
    pub fn with_scope(mut self, scope: CommitScope) -> Self {
        self.scope = scope;
        self
    }
}

/// Record and preview commits for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct Commits<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Commits<'repo> {
    /// Bind commit operations to `repo`.
    #[inline]
    #[must_use]
    pub fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[inline]
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Default [`CommitScope`] from `commit.autoStage` (default auto-stage).
    ///
    /// # Errors
    ///
    /// Propagates config read failures.
    pub fn default_scope(&self) -> Result<CommitScope> {
        default_commit_scope(self.repo)
    }

    /// Default message for the next commit when merge or squash state supplies one.
    ///
    /// # Errors
    ///
    /// Propagates I/O and merge-state read failures.
    pub fn pending_message(&self) -> Result<Option<String>> {
        pending_commit_message(self.repo)
    }

    /// Record a commit using `options.scope` to decide how the index is prepared.
    ///
    /// # Errors
    ///
    /// See [`crate::porcelain::commit::create_commit`] and [`crate::porcelain::CommitError`].
    pub fn create(
        &self,
        options: &CommitOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<CommitOutcome> {
        commit_with_scope(
            self.repo,
            &options.request,
            options.scope.clone(),
            progress,
            cancel,
        )
    }

    /// Preview a commit for `options.scope` without mutating the repository.
    ///
    /// # Errors
    ///
    /// Same as [`Self::create`].
    pub fn plan(
        &self,
        options: &CommitOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<CommitPlan> {
        plan_commit(
            self.repo,
            &options.request,
            options.scope.clone(),
            progress,
            cancel,
        )
    }

    /// Record a commit from the current index without auto-staging the worktree.
    ///
    /// Equivalent to [`Self::create`] with [`CommitScope::Staged`].
    pub fn create_staged(
        &self,
        request: &CommitRequest,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<CommitOutcome> {
        create_commit(self.repo, request, progress, cancel)
    }

    /// Serialize commit bytes from assembled [`crate::commit::CommitData`], honoring signing config.
    ///
    /// # Errors
    ///
    /// Propagates signing and config failures.
    pub fn prepare_bytes(
        &self,
        data: &crate::objects::CommitData,
        sign_override: Option<bool>,
    ) -> Result<Vec<u8>> {
        prepare_commit_bytes(self.repo, data, sign_override)
    }

    /// Hash and store a commit object from [`crate::commit::CommitData`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::prepare_bytes`], plus object-database write failures.
    pub fn write_object(
        &self,
        data: &crate::objects::CommitData,
        sign_override: Option<bool>,
    ) -> Result<ObjectId> {
        write_commit_object(self.repo, data, sign_override)
    }
}
