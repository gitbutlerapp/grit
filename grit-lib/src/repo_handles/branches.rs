//! Local branches: create, list, delete, rename, and upstream configuration.

use crate::branch_inventory::{
    list_branches as list_branches_detailed, upstream_status, BranchDetail, BranchListQuery,
    BranchSummary,
};
use crate::branch_tracking::{
    apply_push_remote_tracking_updates_in, compare_with_target_optional,
    upstream_tracking_full_ref, TargetComparison, TargetComparisonOptions,
};
use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::branch::{
    create_branch, delete_branch, list_branches as list_branches_simple, preview_unset_upstream,
    rename_branch, resolve_create_tracking, set_upstream, unset_upstream, validate_set_upstream,
    BranchCreateOptions, BranchDeleteOptions, BranchList, BranchRenameOptions, CreateBranchOutcome,
    DeletedBranch, RenameBranchOutcome, TrackMode,
};
use crate::porcelain::switch::{resolve_create_branch, SwitchMode};
use crate::push_report::PushRefResult;
use crate::repo::Repository;

/// Branch create, delete, list, rename, upstream, and comparison helpers for one repository.
#[derive(Debug, Copy, Clone)]
pub struct Branches<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Branches<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Create or reset `refs/heads/<name>` without checking it out.
    ///
    /// # Errors
    ///
    /// Returns [`crate::porcelain::branch::BranchError`] for invalid names, existing branches,
    /// missing commits, checked-out conflicts, and upstream tracking failures.
    pub fn create(&self, opts: &BranchCreateOptions) -> Result<CreateBranchOutcome> {
        create_branch(self.repo, opts)
    }

    /// Resolve upstream `(remote, merge)` for branch creation.
    pub fn resolve_create_tracking(
        &self,
        branch_short: &str,
        start_label: &str,
        start_oid: ObjectId,
        track: TrackMode,
    ) -> Result<Option<(String, String)>> {
        resolve_create_tracking(self.repo, branch_short, start_label, start_oid, track)
    }

    /// Resolve how `switch` would create a branch (`SwitchMode::Create` fields).
    pub fn resolve_create_for_switch(
        &self,
        name: &str,
        start: Option<&str>,
        track: TrackMode,
    ) -> Result<SwitchMode> {
        resolve_create_branch(self.repo, name, start, track)
    }

    /// Minimal local branch list (name, oid, current) without commit reads.
    pub fn list_simple(&self) -> Result<BranchList> {
        list_branches_simple(self.repo)
    }

    /// List branches with patterns, upstream metadata, and optional commit fields.
    pub fn list(
        &self,
        query: &BranchListQuery,
        detail: BranchDetail,
    ) -> Result<Vec<BranchSummary>> {
        list_branches_detailed(self.repo, query, detail)
    }

    /// Configured upstream for one local branch, including ahead/behind when it resolves.
    pub fn upstream_status(
        &self,
        branch: &str,
    ) -> Result<Option<crate::branch_inventory::UpstreamStatus>> {
        upstream_status(self.repo, branch)
    }

    /// Delete `refs/heads/<name>` when checks pass.
    pub fn delete(&self, name: &str, opts: &BranchDeleteOptions) -> Result<DeletedBranch> {
        delete_branch(self.repo, name, opts.mode, opts.dry_run)
    }

    /// Rename a local branch, updating config and worktree `HEAD`s.
    pub fn rename(&self, opts: &BranchRenameOptions) -> Result<RenameBranchOutcome> {
        rename_branch(self.repo, opts)
    }

    /// Validate `branch.<branch>.merge` / `.remote` for `upstream` without writing config.
    pub fn validate_set_upstream(&self, branch: &str, upstream: &str) -> Result<String> {
        validate_set_upstream(self.repo, branch, upstream)
    }

    /// Write `branch.<branch>` upstream keys for `upstream`.
    pub fn set_upstream(&self, branch: &str, upstream: &str) -> Result<()> {
        set_upstream(self.repo, branch, upstream)
    }

    /// Ensure unsetting upstream would succeed (branch exists and has upstream).
    pub fn preview_unset_upstream(&self, branch: &str) -> Result<()> {
        preview_unset_upstream(self.repo, branch)
    }

    /// Remove `branch.<branch>` upstream configuration.
    pub fn unset_upstream(&self, branch: &str) -> Result<()> {
        unset_upstream(self.repo, branch)
    }

    /// Full upstream ref name for `branch_short` when configured.
    #[must_use]
    pub fn upstream_tracking_full_ref(&self, branch_short: &str) -> Option<String> {
        upstream_tracking_full_ref(self.repo, branch_short)
    }

    /// Compare the current branch tip against the repository target (for example `grit shortlog`).
    pub fn compare_with_target(
        &self,
        opts: &TargetComparisonOptions,
    ) -> Result<Option<TargetComparison>> {
        compare_with_target_optional(self.repo, opts)
    }

    /// Update remote-tracking refs after a successful push (internal transport helper).
    pub fn apply_push_remote_tracking_updates(
        &self,
        remote: &str,
        results: &[PushRefResult],
    ) -> Result<()> {
        apply_push_remote_tracking_updates_in(self.repo, remote, results)
    }
}
