//! [`Merge`]: branch integration, pick/revert/replay and merge operation state.

use crate::error::RefError;
use crate::ident::Signature;
use crate::merge_file::ConflictStyle;
use crate::objects::ObjectId;
use crate::porcelain::merge::{
    abort_branch_merge, continue_merge, integrate_branch_with, integrate_detached_head_with,
    plan_branch_merge, predict_merge, resolve_merge_conflict_style, resolve_merge_source,
    write_merge_tree, ContinueOptions, ContinueOutcome, IntegrateBranchOutcome, MergeDryRunPlan,
    MergeOptions, MergePrediction, MergePredictionOptions, MergeSource, WrittenMergeTree,
};
use crate::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use crate::porcelain::replay_sequence::{
    abort_replay_sequence, commits_to_replay, continue_replay_sequence, predict_replay_sequence,
    skip_replay_step, start_replay_sequence, ReplayAbortOutcome, ReplaySequenceRequest,
    SequenceOutcome, SequencePrediction,
};
use crate::porcelain::revert::commits_to_revert;
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};

use super::rerere::MergeRerere;

/// Integrate into an explicit branch tip (for example during `pull`).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct BranchIntegrateRequest<'a> {
    /// Ref updated on success (for example `refs/heads/main`).
    pub into_ref: &'a str,
    /// Expected oid of `into_ref` before the merge.
    pub into_oid: ObjectId,
    /// Commit being merged in.
    pub source: &'a MergeSource,
    /// Label for merge messages and reflogs.
    pub label: &'a str,
}

impl<'a> BranchIntegrateRequest<'a> {
    /// Creates a [`BranchIntegrateRequest`] from its required fields.
    ///
    /// # Parameters
    ///
    /// - `into_ref`: ref updated on success (for example `refs/heads/main`).
    /// - `into_oid`: expected oid of `into_ref` before the merge.
    /// - `source`: commit being merged in.
    /// - `label`: label for merge messages and reflogs.
    #[must_use]
    pub const fn new(
        into_ref: &'a str,
        into_oid: ObjectId,
        source: &'a MergeSource,
        label: &'a str,
    ) -> Self {
        Self {
            into_ref,
            into_oid,
            source,
            label,
        }
    }

    /// Sets [`into_ref`](Self::into_ref).
    #[must_use]
    pub const fn with_into_ref(mut self, into_ref: &'a str) -> Self {
        self.into_ref = into_ref;
        self
    }

    /// Sets [`into_oid`](Self::into_oid).
    #[must_use]
    pub const fn with_into_oid(mut self, into_oid: ObjectId) -> Self {
        self.into_oid = into_oid;
        self
    }

    /// Sets [`source`](Self::source).
    #[must_use]
    pub const fn with_source(mut self, source: &'a MergeSource) -> Self {
        self.source = source;
        self
    }

    /// Sets [`label`](Self::label).
    #[must_use]
    pub const fn with_label(mut self, label: &'a str) -> Self {
        self.label = label;
        self
    }
}

/// Merge, pick, revert, replay and merge `--continue` / `--abort` for one repository.
///
/// # Examples
///
/// ```rust
/// use grit_lib::merge::{MergeOptions, MergeSource};
/// use grit_lib::progress::{CancelToken, NullProgress};
/// use grit_lib::repo::{InitOptions, Repository};
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
/// let merge = repo.merge();
/// let _style = merge.resolve_conflict_style(None);
/// let _ = merge.rerere();
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Copy, Clone)]
pub struct Merge<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Merge<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Rerere read/write for this repository.
    #[must_use]
    pub fn rerere(&self) -> MergeRerere<'repo> {
        MergeRerere::new(self.repo)
    }

    /// Resolve conflict marker style: explicit override, then config, then the default merge style.
    #[must_use]
    pub fn resolve_conflict_style(&self, override_style: Option<ConflictStyle>) -> ConflictStyle {
        resolve_merge_conflict_style(self.repo, override_style)
    }

    /// Turn a merge argument (branch, tag, object id, revision) into the commit to merge.
    ///
    /// # Errors
    ///
    /// Propagates revision and object errors; see [`crate::porcelain::merge::resolve_merge_source`].
    pub fn resolve_source(&self, spec: &str) -> crate::error::Result<MergeSource> {
        resolve_merge_source(self.repo, spec)
    }

    /// Predict merging `other` into the current tip without mutating the repository.
    pub fn predict(
        &self,
        into_oid: ObjectId,
        other: ObjectId,
        options: &MergePredictionOptions,
    ) -> crate::error::Result<MergePrediction> {
        predict_merge(self.repo, into_oid, other, options)
    }

    /// Predict integrating `other` into `into_oid` using [`MergeOptions`].
    pub fn plan_integration(
        &self,
        into_oid: ObjectId,
        other: ObjectId,
        label: &str,
        options: &MergeOptions,
    ) -> crate::error::Result<MergeDryRunPlan> {
        plan_branch_merge(self.repo, into_oid, other, label, options)
    }

    /// Store a three-way merge result tree in the object database ([`write_merge_tree`]).
    pub fn write_tree(
        &self,
        into_oid: ObjectId,
        other: ObjectId,
        options: &MergePredictionOptions,
    ) -> crate::error::Result<WrittenMergeTree> {
        write_merge_tree(self.repo, into_oid, other, options)
    }

    /// Integrate `source` into the current branch or detached `HEAD`.
    ///
    /// `label` is the human-facing name used in merge messages and reflogs (typically the same
    /// string the user passed on the command line).
    ///
    /// # Errors
    ///
    /// Same as [`integrate_branch_with`](crate::porcelain::merge::integrate_branch_with) and
    /// [`integrate_detached_head_with`](crate::porcelain::merge::integrate_detached_head_with).
    pub fn integrate(
        &self,
        source: &MergeSource,
        label: &str,
        options: &MergeOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<IntegrateBranchOutcome> {
        match resolve_head_for_repository(self.repo)? {
            HeadState::Branch {
                refname,
                oid: Some(into_oid),
                ..
            } => integrate_branch_with(
                self.repo, &refname, into_oid, source.oid, label, options, progress, cancel,
            ),
            HeadState::Branch { .. } => Err(crate::error::Error::Ref(RefError::UnbornHead)),
            HeadState::Detached { oid: into_oid } => integrate_detached_head_with(
                self.repo, into_oid, source.oid, label, options, progress, cancel,
            ),
            HeadState::Invalid => Err(crate::error::Error::Ref(RefError::InvalidHead)),
        }
    }

    /// Integrate `source` into an explicit branch tip (for example during `pull`).
    ///
    /// # Errors
    ///
    /// Same as [`integrate_branch_with`](crate::porcelain::merge::integrate_branch_with).
    pub fn integrate_into_branch(
        &self,
        req: &BranchIntegrateRequest<'_>,
        options: &MergeOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<IntegrateBranchOutcome> {
        integrate_branch_with(
            self.repo,
            req.into_ref,
            req.into_oid,
            req.source.oid,
            req.label,
            options,
            progress,
            cancel,
        )
    }

    /// Finish an in-progress merge after conflicts are resolved or after `--no-commit`.
    pub fn continue_merge(
        &self,
        options: &ContinueOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<ContinueOutcome> {
        continue_merge(self.repo, options, progress, cancel)
    }

    /// Abort an in-progress merge and restore `ORIG_HEAD`.
    pub fn abort(&self) -> crate::error::Result<()> {
        abort_branch_merge(self.repo)
    }

    /// Cherry-pick one commit onto the current branch.
    pub fn pick(
        &self,
        mut req: ReplayRequest,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<ReplayOutcome> {
        req.direction = ReplayDirection::Pick;
        replay_commit(self.repo, &req, progress, cancel)
    }

    /// Revert one commit on the current branch.
    pub fn revert_commit(
        &self,
        mut req: ReplayRequest,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<ReplayOutcome> {
        req.direction = ReplayDirection::Revert;
        replay_commit(self.repo, &req, progress, cancel)
    }

    /// Resolve revision specs to commits for a pick sequence (oldest-first apply order).
    pub fn commits_to_pick(&self, specs: &[String]) -> crate::error::Result<Vec<ObjectId>> {
        commits_to_replay(self.repo, specs)
    }

    /// Resolve revision specs to commits for a revert sequence.
    pub fn commits_to_revert(&self, specs: &[String]) -> crate::error::Result<Vec<ObjectId>> {
        commits_to_revert(self.repo, specs)
    }

    /// Start a multi-commit pick or revert sequence.
    pub fn start_sequence(
        &self,
        req: &ReplaySequenceRequest,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<SequenceOutcome> {
        start_replay_sequence(self.repo, req, progress, cancel)
    }

    /// Continue an in-progress pick/revert sequence.
    pub fn continue_sequence(
        &self,
        committer: &Signature,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<SequenceOutcome> {
        continue_replay_sequence(self.repo, committer, progress, cancel)
    }

    /// Skip the current step of an in-progress pick/revert sequence.
    pub fn skip_sequence_step(
        &self,
        committer: &Signature,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<SequenceOutcome> {
        skip_replay_step(self.repo, committer, progress, cancel)
    }

    /// Abort an in-progress pick/revert sequence.
    pub fn abort_sequence(&self) -> crate::error::Result<ReplayAbortOutcome> {
        abort_replay_sequence(self.repo)
    }

    /// Predict a pick/revert sequence without touching refs or the worktree.
    pub fn predict_sequence(
        &self,
        req: &ReplaySequenceRequest,
    ) -> crate::error::Result<SequencePrediction> {
        predict_replay_sequence(self.repo, req)
    }
}
