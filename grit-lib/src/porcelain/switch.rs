//! Switch branches or detach `HEAD` at a commit, updating the working tree when needed.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::branch_tracking::write_branch_upstream_config;
use crate::check_ref_format::validate_branch_short_name;
use crate::diff::{DiffEntry, DiffStatus};
use crate::error::{Error, OverwriteReason, Result};
use crate::hooks::{run_post_checkout, HookName, PostOperationHooks};
use crate::index::{Index, MODE_GITLINK};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::branch::resolve_create_tracking;
use crate::porcelain::branch_error::{BranchError, TrackMode};
use crate::porcelain::checkout::{
    checkout_tree_changes_with_index_opts, precompute_wholesale_safe_directories,
    trusted_wholesale_directories_for_switch, CheckoutApplyOptions, WholesalePrecomputeResult,
    WholesaleWorktreeScan,
};
use crate::porcelain::checkout_reflog::{
    append_head_checkout_reflog, checkout_reflog_from_label, finish_switch_ref_updates,
    is_already_detached_at, is_already_on_branch, switch_create_reflog_source_label,
};
use crate::porcelain::orphans::{
    compute_orphan_info, OrphanComputation, OrphanedCommitSummary, DEFAULT_ORPHAN_LIMIT,
};
use crate::porcelain::worktree_guard::{
    collect_untracked_and_wholesale_trust_for_tree_changes, ensure_no_untracked_overwrite,
    load_worktree_index, load_worktree_snapshot, untracked_would_be_overwritten_by_tree_changes,
    TreeSwitchPlan, WorktreeSnapshot,
};
use crate::refs;
use crate::repo::Repository;
use crate::rev_parse::expand_at_minus_to_branch_name;

use crate::rev_parse::{peel_to_commit_for_merge_base, resolve_revision};
use crate::state::{resolve_head_for_repository, HeadState};

/// Resolved switch target after DWIM and revision parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchMode {
    /// Check out an existing local branch.
    Existing {
        /// Short branch name (`main`, `feature`, …).
        branch: String,
        /// Tip commit of that branch.
        oid: ObjectId,
    },
    /// Create `refs/heads/<branch>` at `start_oid`, then check it out.
    Create {
        /// New branch name to create under `refs/heads/`.
        branch: String,
        /// Commit the new branch should point at.
        start_oid: ObjectId,
        /// Label for `branch: Created from …` (e.g. `HEAD`, `main`, `refs/remotes/origin/topic`).
        create_source_label: String,
        /// When set, write `branch.<branch>.remote` and `branch.<branch>.merge` after a successful create.
        track_upstream: Option<(String, String)>,
    },
    /// Detached `HEAD` at `commit`.
    Detached {
        /// Commit oid for detached `HEAD`.
        commit: ObjectId,
        /// User-facing revision string for the reflog `to` label (e.g. `HEAD~1`).
        reflog_to_label: String,
    },
}

/// Inputs for [`switch_repository`].
#[derive(Debug, Clone)]
pub struct SwitchOptions {
    /// Resolved target branch, create, or detach mode.
    pub mode: SwitchMode,
    /// When true, run overwrite checks and report [`SwitchOutcome::changed_paths`] without
    /// updating refs, the index, or the worktree.
    pub dry_run: bool,
    /// Whether to run `post-checkout` after a real switch.
    pub hooks: PostOperationHooks,
}

/// Result of [`switch_repository`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    /// Short name of the branch `HEAD` is on after the switch, or `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// True when [`SwitchMode::Create`] created a new branch ref.
    pub created: bool,
    /// Commit `HEAD` resolves to after the switch.
    pub commit: ObjectId,
    /// When true, nothing was written to refs, the index, or the worktree.
    pub dry_run: bool,
    /// Paths that would differ after checkout; meaningful when [`Self::dry_run`] is true.
    pub changed_paths: usize,
    /// Hooks that ran after the switch ([`HookName::PostCheckout`] when installed).
    pub hooks_run: Vec<HookName>,
    /// Newest-first sample (at most [`DEFAULT_ORPHAN_LIMIT`]) of commits that only the
    /// previous detached `HEAD` reached and that no ref, stash, other worktree, or the switch
    /// target keeps. Empty when `HEAD` was on a branch, since that branch still holds its commits.
    /// Filled for dry runs too.
    pub stranded_commits: Vec<OrphanedCommitSummary>,
    /// Total stranded commit count (may exceed [`Self::stranded_commits`].len()).
    pub stranded_total: usize,
    /// True when [`Self::stranded_commits`] was capped at [`DEFAULT_ORPHAN_LIMIT`].
    pub stranded_truncated: bool,
}

impl SwitchOutcome {
    /// Whether `HEAD` is detached after the switch ([`Self::branch`] is `None`).
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.branch.is_none()
    }

    fn new(
        branch_label: &str,
        created: bool,
        detached: bool,
        commit: ObjectId,
        dry_run: bool,
        changed_paths: usize,
    ) -> Self {
        Self {
            branch: (!detached).then(|| branch_label.to_owned()),
            created,
            commit,
            dry_run,
            changed_paths,
            hooks_run: Vec::new(),
            stranded_commits: Vec::new(),
            stranded_total: 0,
            stranded_truncated: false,
        }
    }

    fn with_stranded(self, stranded: OrphanComputation) -> Self {
        Self {
            stranded_commits: stranded.commits,
            stranded_total: stranded.total,
            stranded_truncated: stranded.truncated,
            ..self
        }
    }
}

pub(crate) struct PreparedSwitch {
    plan: TreeSwitchPlan,
    /// Tree-diff paths whose unstaged worktree edits must not be overwritten.
    preserve_worktree_paths: HashSet<String>,
    /// Index loaded for switch guards; reused for checkout to avoid a second disk read.
    index: crate::index::Index,
    /// Tree OID after checkout completes (cache-tree root on the written index).
    target_tree: ObjectId,
    /// When true, checkout may remove wholesale directories without an on-disk safety walk.
    trust_wholesale_dir_removal: bool,
    preverified_wholesale_dirs: BTreeSet<String>,
    wholesale_safety_precomputed: bool,
}

impl PreparedSwitch {
    pub(crate) fn preserve_worktree_paths(&self) -> &HashSet<String> {
        &self.preserve_worktree_paths
    }
}

/// Resolve `-c <branch> [<start-point>]`.
pub fn resolve_create_branch(
    repo: &Repository,
    branch: &str,
    start_point: Option<&str>,
) -> Result<SwitchMode> {
    validate_branch_short_name(branch)
        .map_err(|e| Error::Message(format!("invalid branch name '{branch}': {e}")))?;
    let branch_ref = format!("refs/heads/{branch}");
    if repo.resolve_ref_oid(&branch_ref).is_ok() {
        return Err(Error::Message(format!("branch '{branch}' already exists")));
    }
    let start_oid = match start_point {
        Some(spec) => resolve_revision_spec(repo, spec)?,
        None => resolve_head_for_repository(repo)?
            .oid()
            .copied()
            .ok_or_else(|| Error::Message("no commits yet to create a branch from".into()))?,
    };
    Ok(SwitchMode::Create {
        branch: branch.to_owned(),
        start_oid,
        create_source_label: switch_create_reflog_source_label(start_point),
        track_upstream: None,
    })
}

/// Resolve a branch name, remote-tracking branch, or revision for `grit switch`.
pub fn resolve_switch_target(repo: &Repository, spec: &str) -> Result<SwitchMode> {
    let spec = normalize_switch_spec(spec);
    if is_at_minus_branch_spec(spec) {
        let branch = match expand_at_minus_to_branch_name(repo, spec) {
            Err(err) => return Err(map_at_minus_switch_error(spec, err)),
            Ok(None) => return Err(Error::Branch(BranchError::NoPreviousBranch)),
            Ok(Some(branch)) => branch,
        };
        let oid = repo.resolve_ref_oid(&format!("refs/heads/{branch}"))?;
        return Ok(SwitchMode::Existing { branch, oid });
    }

    if let Ok(oid) = repo.resolve_ref_oid(&format!("refs/heads/{spec}")) {
        return Ok(SwitchMode::Existing {
            branch: spec.to_owned(),
            oid,
        });
    }

    for candidate in [
        format!("refs/remotes/{spec}"),
        format!("refs/remotes/origin/{spec}"),
    ] {
        if let Ok(oid) = repo.resolve_ref_oid(&candidate) {
            let short = spec.strip_prefix("origin/").unwrap_or(spec).to_owned();
            validate_branch_short_name(&short)
                .map_err(|e| Error::Message(format!("invalid branch name '{short}': {e}")))?;
            let local = format!("refs/heads/{short}");
            if let Ok(local_oid) = repo.resolve_ref_oid(&local) {
                return Ok(SwitchMode::Existing {
                    branch: short,
                    oid: local_oid,
                });
            }
            return Ok(SwitchMode::Create {
                branch: short.clone(),
                start_oid: oid,
                create_source_label: candidate.clone(),
                track_upstream: resolve_create_tracking(
                    repo,
                    &short,
                    &candidate,
                    oid,
                    TrackMode::Auto,
                )?,
            });
        }
    }

    if let Some(mode) = resolve_unique_dwim_switch(repo, spec)? {
        return Ok(mode);
    }

    Err(Error::Branch(
        crate::porcelain::branch_error::BranchError::NotFound {
            name: spec.to_owned(),
        },
    ))
}

fn normalize_switch_spec(spec: &str) -> &str {
    if spec == "-" {
        "@{-1}"
    } else {
        spec
    }
}

fn is_at_minus_branch_spec(spec: &str) -> bool {
    spec.starts_with("@{-") && spec.ends_with('}')
}

fn map_at_minus_switch_error(spec: &str, err: Error) -> Error {
    if is_at_minus_branch_spec(spec) && matches!(err, Error::InvalidRef(_)) {
        return Error::Branch(BranchError::NoPreviousBranch);
    }
    err
}

fn resolve_unique_dwim_switch(repo: &Repository, spec: &str) -> Result<Option<SwitchMode>> {
    const DWIM_RULES: &[&str] = &[
        "{0}",
        "refs/{0}",
        "refs/tags/{0}",
        "refs/heads/{0}",
        "refs/remotes/{0}",
        "refs/remotes/{0}/HEAD",
    ];
    let mut hits: Vec<(String, ObjectId)> = Vec::new();
    for rule in DWIM_RULES {
        let candidate = rule.replace("{0}", spec);
        if let Ok(oid) = repo.resolve_ref_oid(&candidate) {
            hits.push((candidate, oid));
        }
    }
    if hits.len() != 1 {
        return Ok(None);
    }
    let Some((refname, oid)) = hits.into_iter().next() else {
        return Ok(None);
    };
    if let Some(short) = refname.strip_prefix("refs/heads/") {
        return Ok(Some(SwitchMode::Existing {
            branch: short.to_owned(),
            oid,
        }));
    }
    if refname.starts_with("refs/tags/") {
        return Err(Error::Message(format!(
            "cannot switch to tag '{spec}' on a branch — use `grit switch --detach {spec}`"
        )));
    }
    Ok(None)
}

/// Resolve a revision expression for `--detach` or `-c` start points.
pub fn resolve_revision_spec(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision(repo, spec)
        .map_err(|_| Error::Message(format!("invalid revision '{spec}'")))?;
    peel_to_commit_for_merge_base(repo, oid).map_err(|e| match e {
        Error::InvalidRef(msg) => Error::Message(msg),
        other => other,
    })
}

/// Perform a branch switch or detached checkout.
///
/// After a real switch (including switching to the branch or commit `HEAD` already names, as
/// Git does), runs `post-checkout <old> <new> 1` unless [`SwitchOptions::hooks`] is
/// [`PostOperationHooks::Skip`]. The hook's outcome never fails the switch.
///
/// When the effective index is missing (for example after a no-checkout clone), materializes
/// the entire target tree, even if `HEAD` already names the destination. Existing untracked
/// paths remain protected. A valid empty index instead preserves staged deletions. Dry runs
/// report all paths to materialize without writing the index, worktree, refs, or reflogs.
///
/// # Errors
///
/// Returns [`Error::OperationInProgress`] (also for [`SwitchOptions::dry_run`]) while a merge,
/// rebase, am, cherry-pick or revert is paused, whichever tool started it; overwrite refusals
/// and ref/object failures otherwise. On error the hook does not run.
pub fn switch_repository(
    repo: &Repository,
    opts: &SwitchOptions,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<SwitchOutcome> {
    if let Some(op) = crate::state::operation_blocking_switch(repo.git_dir())? {
        return Err(Error::OperationInProgress(op));
    }
    let run_hooks = opts.hooks.enabled() && !opts.dry_run;
    let old_head = if run_hooks {
        resolve_head_for_repository(repo)
            .ok()
            .and_then(|head| head.oid().copied())
    } else {
        None
    };
    let mut outcome = switch_without_hooks(repo, opts, progress, cancel)?;
    if run_hooks && run_post_checkout(repo, old_head, outcome.commit, true) {
        outcome.hooks_run.push(HookName::PostCheckout);
    }
    Ok(outcome)
}

fn switch_without_hooks(
    repo: &Repository,
    opts: &SwitchOptions,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<SwitchOutcome> {
    let (
        target_oid,
        branch_ref,
        created,
        detached,
        branch_label,
        pending_create,
        track_upstream,
        reflog_to,
    ) = match &opts.mode {
        SwitchMode::Existing { branch, oid } => (
            *oid,
            format!("refs/heads/{branch}"),
            false,
            false,
            branch.clone(),
            None,
            None,
            branch.clone(),
        ),
        SwitchMode::Create {
            branch,
            start_oid,
            create_source_label,
            track_upstream,
        } => (
            *start_oid,
            format!("refs/heads/{branch}"),
            true,
            false,
            branch.clone(),
            Some((
                format!("refs/heads/{branch}"),
                *start_oid,
                create_source_label.clone(),
            )),
            track_upstream.clone(),
            branch.clone(),
        ),
        SwitchMode::Detached {
            commit,
            reflog_to_label,
        } => (
            *commit,
            String::new(),
            false,
            true,
            String::new(),
            None,
            None,
            reflog_to_label.clone(),
        ),
    };

    if let SwitchMode::Existing { branch, .. } = &opts.mode {
        crate::worktree::ensure_branch_not_checked_out_elsewhere(repo, branch)?;
    }

    // A no-op still needs an existing index, but need not parse it. Real switches load it once.
    if !detached
        && is_already_on_branch(repo, &branch_ref, target_oid)?
        && repo.index_path_for_env()?.try_exists()?
    {
        if opts.dry_run {
            return Ok(SwitchOutcome::new(
                &branch_label,
                created,
                detached,
                target_oid,
                true,
                0,
            ));
        }
        let head_before = resolve_head_for_repository(repo)?;
        if let Some(from_label) = checkout_reflog_from_label(&head_before) {
            let old_oid = head_before
                .oid()
                .copied()
                .unwrap_or_else(crate::diff::zero_oid);
            append_head_checkout_reflog(repo, old_oid, target_oid, &from_label, &branch_label)?;
        }
        return Ok(SwitchOutcome::new(
            &branch_label,
            created,
            detached,
            target_oid,
            false,
            0,
        ));
    }
    if detached
        && is_already_detached_at(repo, target_oid)?
        && repo.index_path_for_env()?.try_exists()?
    {
        return Ok(SwitchOutcome::new(
            &branch_label,
            created,
            detached,
            target_oid,
            opts.dry_run,
            0,
        ));
    }

    let index = load_worktree_index(repo)?;
    switch_to_target(
        repo,
        index,
        &branch_ref,
        target_oid,
        created,
        detached,
        &branch_label,
        pending_create,
        track_upstream,
        &reflog_to,
        opts.dry_run,
        progress,
        cancel,
    )
}

// An existing empty index can record deliberate staged deletions. Only a missing index
// starts checkout with no tracked paths, independently of the previous HEAD.
fn needs_initial_checkout(index: &Index) -> bool {
    index.source_mtime.is_none() && index.entries.is_empty()
}

#[allow(clippy::too_many_arguments)]
fn switch_to_target(
    repo: &Repository,
    index: Index,
    branch_ref: &str,
    target_oid: ObjectId,
    created: bool,
    detached: bool,
    branch_label: &str,
    pending_create: Option<(String, ObjectId, String)>,
    track_upstream: Option<(String, String)>,
    reflog_to_label: &str,
    dry_run: bool,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<SwitchOutcome> {
    let head_before = resolve_head_for_repository(repo)?;
    let checkout_head = if needs_initial_checkout(&index) {
        None
    } else {
        head_before.oid().copied()
    };
    let (target_tree, head_tree) = match checkout_head {
        None => (commit_tree(repo, &target_oid)?, None),
        Some(head) if head == target_oid => {
            let tree = commit_tree(repo, &target_oid)?;
            (tree, Some(tree))
        }
        Some(head) => {
            let (target_res, head_res) = std::thread::scope(|scope| {
                let target = scope.spawn(|| commit_tree(repo, &target_oid));
                let head = scope.spawn(|| commit_tree(repo, &head));
                (target.join(), head.join())
            });
            (
                target_res
                    .map_err(|_| Error::Message("parallel commit tree read failed".into()))??,
                Some(
                    head_res
                        .map_err(|_| Error::Message("parallel commit tree read failed".into()))??,
                ),
            )
        }
    };
    let snapshot = WorktreeSnapshot { index, head_tree };

    let prepared = prepare_tree_switch_allow_local_with_snapshot(repo, &target_tree, snapshot)?;
    let changed_paths = count_unique_tree_change_paths(&prepared.plan.tree_changes);
    let stranded = stranded_by_leaving_detached_head(repo, &head_before, target_oid)?;
    if dry_run {
        return Ok(SwitchOutcome::new(
            branch_label,
            created,
            detached,
            target_oid,
            true,
            changed_paths,
        )
        .with_stranded(stranded));
    }
    checkout_prepared_tree_transition(repo, prepared, progress, cancel)?;

    if let Some(from_label) = checkout_reflog_from_label(&head_before) {
        finish_switch_ref_updates(
            repo,
            &head_before,
            pending_create,
            detached,
            branch_ref,
            target_oid,
            &from_label,
            reflog_to_label,
        )?;
    } else {
        // `HEAD` was missing or unreadable, so there is no checkout reflog entry to write,
        // but the switch must still install `HEAD`.
        if let Some((refname, oid, source)) = pending_create {
            crate::porcelain::checkout_reflog::create_branch_ref_with_reflog(
                repo, &refname, oid, &source,
            )?;
        }
        if detached {
            refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", &target_oid)?;
        } else {
            refs::write_symbolic_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", branch_ref)?;
        }
    }

    if created {
        if let Some((remote, merge)) = track_upstream {
            write_branch_upstream_config(repo, branch_label, &remote, &merge)?;
        }
    }

    Ok(
        SwitchOutcome::new(branch_label, created, detached, target_oid, false, 0)
            .with_stranded(stranded),
    )
}

/// Commits only the detached `head` reaches that nothing keeps once `HEAD` moves to `target`.
///
/// Skips the walk when `HEAD` is attached: the branch it leaves still holds its commits.
fn stranded_by_leaving_detached_head(
    repo: &Repository,
    head: &HeadState,
    target: ObjectId,
) -> Result<OrphanComputation> {
    let HeadState::Detached { oid: previous } = head else {
        return Ok(OrphanComputation::default());
    };
    compute_orphan_info(repo, head, *previous, target, DEFAULT_ORPHAN_LIMIT)
}

fn count_unique_tree_change_paths(tree_changes: &[DiffEntry]) -> usize {
    let mut paths: BTreeSet<&str> = BTreeSet::new();
    for change in tree_changes {
        if let Some(path) = path_from_diff(change) {
            paths.insert(path);
        }
    }
    paths.len()
}

fn tree_changes_contain_additions(tree_changes: &[DiffEntry]) -> bool {
    tree_changes.iter().any(|c| c.status == DiffStatus::Added)
}

fn prepare_tree_switch_allow_local_with_snapshot(
    repo: &Repository,
    to_tree: &ObjectId,
    snapshot: WorktreeSnapshot,
) -> Result<PreparedSwitch> {
    let prepared = prepare_tree_transition_allow_local_with_snapshot(
        repo,
        None,
        to_tree,
        snapshot,
        &HashSet::new(),
    )
    .map_err(map_switch_tree_transition_prepare_error)?;
    verify_prepared_switch_no_untracked_overwrite(&prepared.plan)?;
    Ok(prepared)
}

fn map_switch_tree_transition_prepare_error(err: TreeTransitionPrepareError) -> Error {
    match err {
        TreeTransitionPrepareError::OverwritePaths(paths) => {
            Error::would_overwrite(OverwriteReason::LocalChanges, paths)
        }
        TreeTransitionPrepareError::Failed(err) => err,
    }
}

fn verify_prepared_switch_no_untracked_overwrite(plan: &TreeSwitchPlan) -> Result<()> {
    if !tree_changes_contain_additions(&plan.tree_changes) {
        return Ok(());
    }
    let collisions =
        untracked_would_be_overwritten_by_tree_changes(&plan.untracked, &plan.tree_changes);
    if collisions.is_empty() {
        return Ok(());
    }
    Err(Error::would_overwrite(
        OverwriteReason::Untracked,
        collisions,
    ))
}

/// Local edits on tree-diff paths block a tree transition.
pub(crate) enum TreeTransitionPrepareError {
    /// Staged or unstaged paths that would be overwritten.
    OverwritePaths(Vec<String>),
    /// Any other failure while planning the transition.
    Failed(Error),
}

impl From<Error> for TreeTransitionPrepareError {
    fn from(value: Error) -> Self {
        Self::Failed(value)
    }
}

fn tree_transition_prepare_to_error(err: TreeTransitionPrepareError) -> Error {
    match err {
        TreeTransitionPrepareError::OverwritePaths(paths) => {
            Error::dirty_worktree(crate::error::WorktreeOperation::Switch, paths)
        }
        TreeTransitionPrepareError::Failed(err) => err,
    }
}

/// Plan a tree-to-tree checkout that keeps staged/unstaged edits on paths the target tree already matches.
pub(crate) fn prepare_tree_transition_allow_local(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    force_preserve_worktree: &HashSet<String>,
) -> std::result::Result<PreparedSwitch, TreeTransitionPrepareError> {
    let snapshot = load_worktree_snapshot(repo)?;
    prepare_tree_transition_allow_local_with_snapshot(
        repo,
        from_tree,
        to_tree,
        snapshot,
        force_preserve_worktree,
    )
}

struct AllowLocalTreeTransitionCore {
    tree_changes: Vec<DiffEntry>,
    untracked: Vec<String>,
    local_blocked_paths: Vec<String>,
    preserve_worktree_paths: HashSet<String>,
    index: crate::index::Index,
    trust_wholesale_dir_removal: bool,
    preverified_wholesale_dirs: BTreeSet<String>,
    wholesale_safety_precomputed: bool,
}

fn merge_keep_style_block_paths(core: &AllowLocalTreeTransitionCore) -> Vec<String> {
    let mut paths = core.local_blocked_paths.clone();
    if tree_changes_contain_additions(&core.tree_changes) {
        paths.extend(untracked_would_be_overwritten_by_tree_changes(
            &core.untracked,
            &core.tree_changes,
        ));
    }
    paths.sort();
    paths.dedup();
    paths
}

fn plan_allow_local_tree_transition_core(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    mut snapshot: WorktreeSnapshot,
    force_preserve_worktree: &HashSet<String>,
) -> std::result::Result<AllowLocalTreeTransitionCore, TreeTransitionPrepareError> {
    let from_tree_oid = from_tree.copied().or(snapshot.head_tree);
    snapshot.head_tree = from_tree_oid;
    let trust_index_for_old_tree =
        from_tree_oid.is_some() && !snapshot.index.entries.iter().any(|e| e.stage() != 0);
    let from_tree_ref = from_tree_oid.as_ref();
    let tree_changes = if from_tree_ref == Some(to_tree) {
        Vec::new()
    } else if trust_index_for_old_tree {
        crate::diff::diff_trees_with_index(
            repo.odb(),
            &snapshot.index,
            from_tree_ref,
            Some(to_tree),
            "",
            true,
        )?
    } else {
        crate::diff::diff_trees(repo.odb(), from_tree_ref, Some(to_tree), "")?
    };
    let (staged, unstaged, _index_changed) =
        crate::porcelain::worktree_guard::local_change_diffs_for_tree_changes(
            repo,
            &mut snapshot.index,
            snapshot.head_tree,
            &tree_changes,
            None,
        )?;
    let work_tree = repo
        .work_tree()
        .ok_or(TreeTransitionPrepareError::Failed(Error::RequiresWorkTree))?;
    let (local_blocked_paths, preserve_worktree_paths) =
        match ensure_local_changes_compatible_with_switch(
            work_tree,
            &snapshot.index,
            &staged,
            &unstaged,
            &tree_changes,
            force_preserve_worktree,
        ) {
            Ok(preserve) => (Vec::new(), preserve),
            Err(blocked) => (blocked, HashSet::new()),
        };
    let ignorecase = repo
        .config()
        .ok()
        .and_then(|c| c.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false);
    let delete_paths_set: HashSet<String> = tree_changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .filter_map(|c| c.old_path.clone())
        .collect();
    let (untracked, wholesale_ignored, trust_wholesale_dir_removal, traditional_ignore_scan) =
        collect_untracked_and_wholesale_trust_for_tree_changes(
            repo,
            &mut snapshot.index,
            work_tree,
            &tree_changes,
            Some(&delete_paths_set),
        )?;
    let delete_paths: Vec<String> = delete_paths_set.into_iter().collect();
    let gitlink_deletes: HashSet<String> = tree_changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .filter_map(|c| {
            let path = c.old_path.as_ref()?;
            let mode = u32::from_str_radix(&c.old_mode, 8).unwrap_or(0);
            (mode == MODE_GITLINK).then(|| path.clone())
        })
        .collect();
    let (preverified_wholesale_dirs, wholesale_safety_precomputed) = if delete_paths.is_empty() {
        (BTreeSet::new(), false)
    } else if trust_wholesale_dir_removal {
        (
            trusted_wholesale_directories_for_switch(
                work_tree,
                &snapshot.index,
                &delete_paths,
                &gitlink_deletes,
            ),
            true,
        )
    } else {
        let scan = traditional_ignore_scan.then(|| WholesaleWorktreeScan {
            untracked: &untracked,
            ignored: &wholesale_ignored,
            ignorecase,
        });
        let WholesalePrecomputeResult { safe_directories } = precompute_wholesale_safe_directories(
            repo,
            work_tree,
            &snapshot.index,
            &delete_paths,
            ignorecase,
            scan.as_ref(),
            &gitlink_deletes,
        );
        (safe_directories, true)
    };
    Ok(AllowLocalTreeTransitionCore {
        tree_changes,
        untracked,
        local_blocked_paths,
        preserve_worktree_paths,
        index: snapshot.index,
        trust_wholesale_dir_removal,
        preverified_wholesale_dirs,
        wholesale_safety_precomputed,
    })
}

pub(crate) fn prepare_tree_transition_allow_local_with_snapshot(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    snapshot: WorktreeSnapshot,
    force_preserve_worktree: &HashSet<String>,
) -> std::result::Result<PreparedSwitch, TreeTransitionPrepareError> {
    let core = plan_allow_local_tree_transition_core(
        repo,
        from_tree,
        to_tree,
        snapshot,
        force_preserve_worktree,
    )?;
    if !core.local_blocked_paths.is_empty() {
        return Err(TreeTransitionPrepareError::OverwritePaths(
            core.local_blocked_paths,
        ));
    }
    Ok(PreparedSwitch {
        plan: TreeSwitchPlan {
            untracked: core.untracked,
            tree_changes: core.tree_changes,
        },
        preserve_worktree_paths: core.preserve_worktree_paths,
        index: core.index,
        target_tree: *to_tree,
        trust_wholesale_dir_removal: core.trust_wholesale_dir_removal,
        preverified_wholesale_dirs: core.preverified_wholesale_dirs,
        wholesale_safety_precomputed: core.wholesale_safety_precomputed,
    })
}

/// Paths that would block a keep-style reset preview (local edits plus untracked collisions).
pub(crate) fn keep_style_transition_would_block_paths_for_preview(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<Vec<String>> {
    let snapshot = load_worktree_snapshot(repo)?;
    let core =
        plan_allow_local_tree_transition_core(repo, from_tree, to_tree, snapshot, &HashSet::new())
            .map_err(|err| match err {
                TreeTransitionPrepareError::Failed(err) => err,
                TreeTransitionPrepareError::OverwritePaths(_) => {
                    Error::Message("keep preview planning should not fail on local paths".into())
                }
            })?;
    Ok(merge_keep_style_block_paths(&core))
}

fn commit_tree(repo: &Repository, commit: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb().read(commit)?;
    Ok(parse_commit(&obj.data)?.tree)
}

fn path_from_diff(entry: &DiffEntry) -> Option<&str> {
    entry.new_path.as_deref().or(entry.old_path.as_deref())
}

#[derive(Clone, Copy)]
struct TargetTreeEntry {
    oid: ObjectId,
    mode: u32,
}

fn target_tree_entry_from_diff(change: &DiffEntry) -> Option<(&str, TargetTreeEntry)> {
    let path = path_from_diff(change)?;
    if change.status == DiffStatus::Deleted {
        return None;
    }
    let mode = u32::from_str_radix(&change.new_mode, 8).unwrap_or(0);
    Some((
        path,
        TargetTreeEntry {
            oid: change.new_oid,
            mode,
        },
    ))
}

fn index_matches_target_tree_entry(
    index: &crate::index::Index,
    path: &str,
    target: TargetTreeEntry,
) -> bool {
    index
        .get(path.as_bytes(), 0)
        .is_some_and(|entry| entry.oid == target.oid && entry.mode == target.mode)
}

/// True when a gitlink worktree's checked-out HEAD already matches the branch being switched to.
fn gitlink_worktree_head_matches_target(entry: &DiffEntry, target: TargetTreeEntry) -> bool {
    use crate::index::MODE_GITLINK;
    target.mode == MODE_GITLINK && entry.new_oid == target.oid
}

/// Failure while verifying a tree transition without overwriting local edits.
#[derive(Debug)]
pub(crate) enum TreeTransitionVerifyError {
    /// Staged or unstaged edits on paths that the target tree would overwrite.
    WouldOverwrite(Vec<String>),
    /// Preparation or diff computation failed.
    Failed(Error),
}

/// Run the checkout half of a prepared tree transition (index + worktree).
/// Refuse when staged/unstaged edits on tree-diff paths would be overwritten.
pub(crate) fn verify_tree_transition_allow_local(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> std::result::Result<(), TreeTransitionVerifyError> {
    let mut prepared =
        prepare_tree_transition_allow_local(repo, from_tree, to_tree, &HashSet::new()).map_err(
            |err| match err {
                TreeTransitionPrepareError::Failed(e) => TreeTransitionVerifyError::Failed(e),
                TreeTransitionPrepareError::OverwritePaths(paths) => {
                    TreeTransitionVerifyError::WouldOverwrite(paths)
                }
            },
        )?;
    let tree_changes = prepared.plan.tree_changes.clone();
    let untracked =
        crate::porcelain::worktree_guard::untracked_would_be_overwritten_by_tree_changes(
            &prepared.plan.untracked,
            &tree_changes,
        );
    if !untracked.is_empty() {
        return Err(TreeTransitionVerifyError::WouldOverwrite(untracked));
    }
    let (staged, unstaged, _) =
        crate::porcelain::worktree_guard::local_change_diffs_for_tree_changes(
            repo,
            &mut prepared.index,
            from_tree.copied(),
            &tree_changes,
            None,
        )
        .map_err(TreeTransitionVerifyError::Failed)?;
    let work_tree = repo
        .work_tree()
        .ok_or(TreeTransitionVerifyError::Failed(Error::RequiresWorkTree))?;
    ensure_local_changes_compatible_with_switch(
        work_tree,
        &prepared.index,
        &staged,
        &unstaged,
        &tree_changes,
        &HashSet::new(),
    )
    .map_err(TreeTransitionVerifyError::WouldOverwrite)?;
    Ok(())
}

/// Checkout from `from_tree` to `to_tree`, preserving unrelated local edits when possible.
pub(crate) fn checkout_tree_transition_allow_local(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<()> {
    checkout_tree_transition_allow_local_with_preserved(
        repo,
        from_tree,
        to_tree,
        &HashSet::new(),
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    )
}

/// Like [`checkout_tree_transition_allow_local`], but local-overwrite paths become [`Error::WouldOverwrite`].
pub(crate) fn checkout_tree_transition_allow_local_reset(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    let prepared = prepare_tree_transition_allow_local(repo, from_tree, to_tree, &HashSet::new())
        .map_err(map_switch_tree_transition_prepare_error)?;
    verify_prepared_switch_no_untracked_overwrite(&prepared.plan)?;
    checkout_prepared_tree_transition(repo, prepared, progress, cancel)
}

/// Like [`checkout_tree_transition_allow_local`], but always keeps worktree bytes on `force_preserve_worktree`.
pub(crate) fn checkout_tree_transition_allow_local_with_preserved(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    force_preserve_worktree: &HashSet<String>,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    let prepared =
        prepare_tree_transition_allow_local(repo, from_tree, to_tree, force_preserve_worktree)
            .map_err(tree_transition_prepare_to_error)?;
    checkout_prepared_tree_transition(repo, prepared, progress, cancel)
}

/// Run the checkout half of a prepared allow-local transition (for merge integration).
pub(crate) fn checkout_prepared_tree_transition_for_merge(
    repo: &Repository,
    prepared: PreparedSwitch,
) -> Result<()> {
    checkout_prepared_tree_transition(
        repo,
        prepared,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    )
}

fn checkout_prepared_tree_transition(
    repo: &Repository,
    mut prepared: PreparedSwitch,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    let overwrite_untracked = if tree_changes_contain_additions(&prepared.plan.tree_changes) {
        prepared.plan.untracked.as_slice()
    } else {
        &[][..]
    };
    ensure_no_untracked_overwrite(overwrite_untracked, &prepared.plan.tree_changes)?;
    let touches_gitattributes =
        crate::porcelain::checkout::tree_changes_touch_gitattributes(&prepared.plan.tree_changes);
    let wholesale_dirs = &prepared.preverified_wholesale_dirs;
    let filter_wholesale_deletes = !wholesale_dirs.is_empty();
    let attribute_changes = touches_gitattributes.then_some(prepared.plan.tree_changes.as_slice());

    let filtered_tree_changes;
    let checkout_changes: &[DiffEntry] = if prepared.preserve_worktree_paths.is_empty()
        && !filter_wholesale_deletes
        && !touches_gitattributes
    {
        &prepared.plan.tree_changes
    } else {
        let mut tree_changes = Vec::with_capacity(prepared.plan.tree_changes.len());
        for change in &prepared.plan.tree_changes {
            if path_from_diff(change)
                .is_some_and(|path| prepared.preserve_worktree_paths.contains(path))
            {
                continue;
            }
            // Always keep delete entries even under wholesale directory candidates: directory
            // removal can fail (permissions, open files) and checkout must still unlink each file.
            let include_in_checkout = change.status == DiffStatus::Deleted
                || !filter_wholesale_deletes
                || change.old_path.as_ref().is_none_or(|path| {
                    !crate::porcelain::checkout::path_under_wholesale_directory(
                        path,
                        wholesale_dirs,
                    )
                });
            if include_in_checkout {
                tree_changes.push(change.clone());
            }
        }
        filtered_tree_changes = tree_changes;
        &filtered_tree_changes
    };
    checkout_tree_changes_with_index_opts(
        repo,
        checkout_changes,
        &mut prepared.index,
        progress,
        cancel,
        CheckoutApplyOptions {
            parallel_prepare_blobs: true,
            checkout_parallel_threshold: None,
            trust_wholesale_dir_removal: prepared.trust_wholesale_dir_removal,
            preverified_wholesale_dirs: prepared.preverified_wholesale_dirs,
            wholesale_safety_precomputed: prepared.wholesale_safety_precomputed,
            skip_bulk_checkout_fast_path: false,
            checkout_result_tree: Some(prepared.target_tree),
            defer_index_disk_write: false,
            ..CheckoutApplyOptions::default()
        },
        attribute_changes,
    )
}

pub(crate) fn ensure_local_changes_compatible_with_switch(
    work_tree: &std::path::Path,
    index: &crate::index::Index,
    staged: &[DiffEntry],
    unstaged: &[DiffEntry],
    tree_changes: &[DiffEntry],
    force_preserve_worktree: &HashSet<String>,
) -> std::result::Result<HashSet<String>, Vec<String>> {
    let mut target_by_path: HashMap<&str, TargetTreeEntry> = HashMap::new();
    for change in tree_changes {
        if let Some((path, target)) = target_tree_entry_from_diff(change) {
            target_by_path.insert(path, target);
        }
    }
    let tree_changed_paths: HashSet<&str> =
        tree_changes.iter().filter_map(path_from_diff).collect();

    let mut preserve_worktree_paths = HashSet::new();
    let mut blocked_paths = Vec::new();

    for entry in staged {
        let Some(path) = path_from_diff(entry) else {
            continue;
        };
        if !tree_changed_paths.contains(path) {
            continue;
        }
        if force_preserve_worktree.contains(path) {
            preserve_worktree_paths.insert(path.to_owned());
            continue;
        }
        if target_by_path
            .get(path)
            .is_some_and(|target| index_matches_target_tree_entry(index, path, *target))
        {
            continue;
        }
        blocked_paths.push(path.to_owned());
    }

    for entry in unstaged {
        let Some(path) = path_from_diff(entry) else {
            continue;
        };
        if !tree_changed_paths.contains(path) {
            continue;
        }
        if force_preserve_worktree.contains(path) {
            preserve_worktree_paths.insert(path.to_owned());
            continue;
        }
        if entry.status == DiffStatus::Deleted {
            let scheduled = tree_changes.iter().find(|change| {
                change.status == DiffStatus::Deleted && change.old_path.as_deref() == Some(path)
            });
            if scheduled.is_some_and(|change| {
                index
                    .get(path.as_bytes(), 0)
                    .is_some_and(|ie| ie.oid == change.old_oid)
                    && crate::porcelain::worktree_guard::worktree_path_absent_for_trusted_delete(
                        work_tree, path,
                    )
            }) {
                continue;
            }
        }
        if let Some(target) = target_by_path.get(path) {
            if index_matches_target_tree_entry(index, path, *target)
                || gitlink_worktree_head_matches_target(entry, *target)
            {
                preserve_worktree_paths.insert(path.to_owned());
                continue;
            }
        }
        blocked_paths.push(path.to_owned());
    }

    if !blocked_paths.is_empty() {
        blocked_paths.sort();
        blocked_paths.dedup();
        return Err(blocked_paths);
    }

    Ok(preserve_worktree_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{serialize_commit, serialize_tag, CommitData, ObjectKind, TagData};
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::repo::{init_repository, Repository};
    use crate::state::resolve_head_for_repository;
    use crate::state::HeadState;
    use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
    use crate::RefStorageFormat;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        (dir, repo)
    }

    #[test]
    fn initial_checkout_distinguishes_missing_from_existing_empty_index() {
        let (_dir, repo) = init_repo();
        let mut index = load_worktree_index(&repo).expect("missing index");
        assert!(needs_initial_checkout(&index));
        repo.write_index(&mut index).expect("write empty index");
        let index = load_worktree_index(&repo).expect("existing empty index");
        assert!(index.entries.is_empty());
        assert!(!needs_initial_checkout(&index));
    }

    #[test]
    fn switch_wholesale_tree_delete_refuses_tracked_unstaged_edit() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        std::fs::create_dir_all(wt.join("d")).expect("mkdir");
        for i in 0..8 {
            std::fs::write(wt.join(format!("d/f{i}.txt")), format!("v{i}\n")).expect("write");
        }
        let mut index = repo.load_index().expect("index");
        for i in 0..8 {
            let rel = format!("d/f{i}.txt");
            let abs = wt.join(&rel);
            let oid = repo
                .odb()
                .write(ObjectKind::Blob, format!("v{i}\n").as_bytes())
                .expect("blob");
            index.add_or_replace(
                crate::index::entry_from_stat(
                    &abs,
                    rel.as_bytes(),
                    oid,
                    crate::index::MODE_REGULAR,
                )
                .expect("entry"),
            );
        }
        index.sort();
        repo.write_index(&mut index).expect("write index");
        let main_tip = {
            let tree =
                write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::default())
                    .expect("tree");
            let commit_data = CommitData {
                tree,
                parents: vec![],
                author: "Test <t@example.com>".to_owned(),
                committer: "Test <t@example.com>".to_owned(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: "main\n".into(),
                raw_message: None,
                extra_headers: Vec::new(),
            };
            let bytes = serialize_commit(&commit_data);
            let oid = repo
                .odb()
                .write(ObjectKind::Commit, &bytes)
                .expect("commit");
            refs::write_ref(repo.git_dir(), "refs/heads/main", &oid).expect("main");
            refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");
            oid
        };

        refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");
        refs::write_ref(repo.git_dir(), "refs/heads/target", &main_tip).expect("target");
        std::fs::remove_dir_all(wt.join("d")).expect("remove d");
        let mut index_target = repo.load_index().expect("index");
        index_target
            .entries
            .retain(|e| !e.path.starts_with(b"d/") && e.path.as_slice() != b"d");
        index_target.sort();
        repo.write_index(&mut index_target).expect("write index");
        let target_tip = {
            let tree = write_tree_update_index(
                repo.odb(),
                &mut index_target,
                "",
                WriteTreeFlags::default(),
            )
            .expect("tree");
            let commit_data = CommitData {
                tree,
                parents: vec![main_tip],
                author: "Test <t@example.com>".to_owned(),
                committer: "Test <t@example.com>".to_owned(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: "target\n".into(),
                raw_message: None,
                extra_headers: Vec::new(),
            };
            let bytes = serialize_commit(&commit_data);
            let oid = repo
                .odb()
                .write(ObjectKind::Commit, &bytes)
                .expect("commit");
            refs::write_ref(repo.git_dir(), "refs/heads/target", &oid).expect("target");
            oid
        };

        std::fs::create_dir_all(wt.join("d")).expect("restore d");
        for i in 0..8 {
            std::fs::write(wt.join(format!("d/f{i}.txt")), format!("v{i}\n")).expect("restore");
        }
        let mut index_main = repo.load_index().expect("index");
        for i in 0..8 {
            let rel = format!("d/f{i}.txt");
            let abs = wt.join(&rel);
            let oid = repo
                .odb()
                .write(ObjectKind::Blob, format!("v{i}\n").as_bytes())
                .expect("blob");
            index_main.add_or_replace(
                crate::index::entry_from_stat(
                    &abs,
                    rel.as_bytes(),
                    oid,
                    crate::index::MODE_REGULAR,
                )
                .expect("entry"),
            );
        }
        index_main.sort();
        repo.write_index(&mut index_main).expect("write index");
        refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        std::fs::write(wt.join("d/f0.txt"), "LOCAL EDIT\n").expect("unstaged edit");
        let head_before = resolve_head_for_repository(&repo).expect("head");

        let err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "target".into(),
                    oid: target_tip,
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("must refuse overwrite");
        assert!(matches!(
            err,
            Error::WouldOverwrite {
                reason: OverwriteReason::LocalChanges,
                ..
            }
        ));

        let head_after = resolve_head_for_repository(&repo).expect("head");
        assert_eq!(head_before, head_after);
        assert_eq!(
            std::fs::read_to_string(wt.join("d/f0.txt")).expect("read"),
            "LOCAL EDIT\n"
        );
    }

    fn commit_on_branch(
        repo: &Repository,
        branch: &str,
        path: &str,
        contents: &str,
        parent: Option<ObjectId>,
    ) -> ObjectId {
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join(path), contents).expect("write");
        let mut index = repo.load_index().expect("index");
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, contents.as_bytes())
            .expect("blob");
        let entry = crate::index::entry_from_stat(
            &wt.join(path),
            path.as_bytes(),
            oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree");
        repo.write_index(&mut index).expect("write index");
        let commit = CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "c\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        let refname = format!("refs/heads/{branch}");
        refs::write_ref(repo.git_dir(), &refname, &co).expect("branch");
        refs::write_symbolic_ref(repo.git_dir(), "HEAD", &refname).expect("symhead");
        co
    }

    #[cfg(unix)]
    #[test]
    fn switch_refuses_when_dirty_file_differs_only_in_mode_between_commits() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        fn commit_x_sh(repo: &Repository, mode: u32, parent: Option<ObjectId>) -> ObjectId {
            let wt = repo.work_tree().expect("wt");
            std::fs::write(wt.join("x.sh"), "#!/bin/sh\n").expect("write");
            #[cfg(unix)]
            {
                let mut perms = std::fs::metadata(wt.join("x.sh"))
                    .expect("meta")
                    .permissions();
                perms.set_mode(if mode == crate::index::MODE_EXECUTABLE {
                    0o100755
                } else {
                    0o100644
                });
                std::fs::set_permissions(wt.join("x.sh"), perms).expect("chmod");
            }
            let mut index = repo.load_index().expect("index");
            let oid = repo
                .odb()
                .write(ObjectKind::Blob, b"#!/bin/sh\n")
                .expect("blob");
            let entry =
                crate::index::entry_from_stat(&wt.join("x.sh"), b"x.sh", oid, mode).expect("entry");
            index.add_or_replace(entry);
            index.sort();
            let tree =
                write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
                    .expect("tree");
            repo.write_index(&mut index).expect("write index");
            let commit = CommitData {
                tree,
                parents: parent.into_iter().collect(),
                author: "T <t@t> 0 +0000".into(),
                committer: "T <t@t> 0 +0000".into(),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: "c\n".into(),
                raw_message: None,
                extra_headers: Vec::new(),
            };
            repo.odb()
                .write(ObjectKind::Commit, &serialize_commit(&commit))
                .expect("commit")
        }

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        let c1 = commit_x_sh(&repo, crate::index::MODE_EXECUTABLE, None);
        refs::write_ref(&repo.git_dir(), "refs/heads/main", &c1).expect("branch");
        let c2 = commit_x_sh(&repo, crate::index::MODE_REGULAR, Some(c1));
        refs::write_ref(&repo.git_dir(), "refs/heads/main", &c2).expect("branch");

        std::fs::OpenOptions::new()
            .append(true)
            .open(wt.join("x.sh"))
            .expect("open")
            .write_all(b"more\n")
            .expect("append");

        let err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Detached {
                    commit: c1,
                    reflog_to_label: "HEAD~1".into(),
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("local changes should block switch");
        assert!(matches!(
            err,
            Error::WouldOverwrite {
                reason: OverwriteReason::LocalChanges,
                ..
            }
        ));
    }

    #[test]
    fn switch_preserves_unstaged_over_staged_destination() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        commit_on_branch(&repo, "other", "f", "two\n", Some(main_tip));
        refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "two\n").expect("stage dest in index");
        let mut index = repo.load_index().expect("index");
        let oid = repo.odb().write(ObjectKind::Blob, b"two\n").expect("blob");
        index.add_or_replace(
            crate::index::entry_from_stat(&wt.join("f"), b"f", oid, crate::index::MODE_REGULAR)
                .expect("entry"),
        );
        repo.write_index(&mut index).expect("write index");
        std::fs::write(wt.join("f"), "precious\n").expect("unstaged");

        switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "other".into(),
                    oid: refs::resolve_ref(repo.git_dir(), "refs/heads/other").expect("oid"),
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("switch");

        assert_eq!(std::fs::read(wt.join("f")).expect("disk"), b"precious\n");
        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
        assert_eq!(st.unstaged.len(), 1);
    }

    #[test]
    fn failed_create_switch_leaves_no_branch_ref() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        commit_on_branch(&repo, "other", "f", "two\n", Some(main_tip));
        refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "three\n").expect("wt");
        let mut index = repo.load_index().expect("index");
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, b"three\n")
            .expect("blob");
        index.add_or_replace(
            crate::index::entry_from_stat(&wt.join("f"), b"f", oid, crate::index::MODE_REGULAR)
                .expect("entry"),
        );
        repo.write_index(&mut index).expect("write index");

        let err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Create {
                    branch: "leaked".into(),
                    start_oid: refs::resolve_ref(repo.git_dir(), "refs/heads/other").expect("oid"),
                    create_source_label: "HEAD".into(),
                    track_upstream: None,
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("conflict");
        assert!(matches!(
            err,
            Error::WouldOverwrite {
                reason: OverwriteReason::LocalChanges,
                ..
            }
        ));
        assert!(refs::resolve_ref(repo.git_dir(), "refs/heads/leaked").is_err());
    }

    #[test]
    fn remote_dwim_create_writes_upstream_branch_config() {
        let (_dir, repo) = init_repo();
        let tip = commit_on_branch(&repo, "main", "f", "1\n", None);
        refs::write_ref(repo.git_dir(), "refs/remotes/origin/topic", &tip).expect("remote");

        let mode = resolve_switch_target(&repo, "origin/topic").expect("resolve");
        match &mode {
            SwitchMode::Create {
                branch,
                track_upstream,
                ..
            } => {
                assert_eq!(branch, "topic");
                assert_eq!(
                    track_upstream.as_ref(),
                    Some(&("origin".to_owned(), "refs/heads/topic".to_owned()))
                );
            }
            other => panic!("expected create from remote, got {other:?}"),
        }

        switch_repository(
            &repo,
            &SwitchOptions {
                mode,
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("switch");
        let config = repo.config().expect("config");
        assert_eq!(config.get("branch.topic.remote").as_deref(), Some("origin"));
        assert_eq!(
            config.get("branch.topic.merge").as_deref(),
            Some("refs/heads/topic")
        );
    }

    #[test]
    fn remote_spec_uses_local_branch_oid_when_branch_exists() {
        let (_dir, repo) = init_repo();
        let local_tip = commit_on_branch(&repo, "main", "f", "local\n", None);
        let remote_tip = commit_on_branch(&repo, "other", "f", "remote\n", Some(local_tip));
        refs::write_ref(repo.git_dir(), "refs/remotes/origin/main", &remote_tip).expect("remote");

        let mode = resolve_switch_target(&repo, "origin/main").expect("resolve");
        match mode {
            SwitchMode::Existing { branch, oid } => {
                assert_eq!(branch, "main");
                assert_eq!(oid, local_tip);
            }
            other => panic!("expected existing local main, got {other:?}"),
        }
    }

    #[test]
    fn resolve_revision_spec_peels_annotated_tag() {
        let (_dir, repo) = init_repo();
        let tip = commit_on_branch(&repo, "main", "f", "x\n", None);
        let tag_bytes = serialize_tag(&TagData {
            object: tip,
            object_type: "commit".into(),
            tag: "v1".into(),
            tagger: Some("T <t@t> 0 +0000".into()),
            extra_headers: Vec::new(),
            message: "annotated\n".into(),
        });
        let tag_oid = repo
            .odb()
            .write(ObjectKind::Tag, &tag_bytes)
            .expect("tag object");
        refs::write_ref(repo.git_dir(), "refs/tags/v1", &tag_oid).expect("tag ref");

        let resolved = resolve_revision_spec(&repo, "v1").expect("peel tag");
        assert_eq!(resolved, tip);

        switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Detached {
                    commit: resolved,
                    reflog_to_label: "v1".into(),
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("detach at annotated tag");

        let head = resolve_head_for_repository(&repo).expect("head");
        assert!(matches!(head, HeadState::Detached { oid } if oid == tip));
    }

    #[test]
    fn switch_outcome_branch_is_none_only_when_detached() {
        let (_dir, repo) = init_repo();
        let tip = commit_on_branch(&repo, "main", "f", "x\n", None);

        let detached = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Detached {
                    commit: tip,
                    reflog_to_label: "main".into(),
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("detach");
        assert_eq!(detached.branch, None);
        assert!(detached.is_detached());
        assert_eq!(detached.commit, tip);

        let preview = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "main".into(),
                    oid: tip,
                },
                dry_run: true,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("preview");
        assert_eq!(preview.branch.as_deref(), Some("main"));
        assert!(!preview.is_detached());

        let attached = switch_repository(
            &repo,
            &SwitchOptions {
                mode: resolve_create_branch(&repo, "topic", None).expect("create mode"),
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("create");
        assert_eq!(attached.branch.as_deref(), Some("topic"));
        assert!(attached.created);
        assert!(!attached.is_detached());
    }

    #[test]
    fn create_branch_start_point_peels_annotated_tag() {
        let (_dir, repo) = init_repo();
        let tip = commit_on_branch(&repo, "main", "f", "x\n", None);
        let tag_bytes = serialize_tag(&TagData {
            object: tip,
            object_type: "commit".into(),
            tag: "v1".into(),
            tagger: Some("T <t@t> 0 +0000".into()),
            extra_headers: Vec::new(),
            message: "annotated\n".into(),
        });
        let tag_oid = repo
            .odb()
            .write(ObjectKind::Tag, &tag_bytes)
            .expect("tag object");
        refs::write_ref(repo.git_dir(), "refs/tags/v1", &tag_oid).expect("tag ref");

        let mode = resolve_create_branch(&repo, "from-tag", Some("v1")).expect("create mode");
        match mode {
            SwitchMode::Create { start_oid, .. } => assert_eq!(start_oid, tip),
            other => panic!("expected create, got {other:?}"),
        }
    }

    #[test]
    fn dwim_tag_is_rejected_for_attached_switch() {
        let (_dir, repo) = init_repo();
        let tip = commit_on_branch(&repo, "main", "f", "x\n", None);
        refs::write_ref(repo.git_dir(), "refs/tags/rel1", &tip).expect("tag");

        let err = resolve_switch_target(&repo, "rel1").expect_err("tag");
        assert!(matches!(err, Error::Message(_)));
        assert!(err.to_string().contains("--detach"));
    }

    /// Commit on top of `parent` with `HEAD` detached at the new commit and no ref holding it.
    fn commit_on_detached_head(repo: &Repository, contents: &str, parent: ObjectId) -> ObjectId {
        let oid = commit_on_branch(repo, "scratch", "f", contents, Some(parent));
        refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", &oid)
            .expect("detach");
        refs::delete_ref(repo.git_dir(), "refs/heads/scratch").expect("drop scratch");
        oid
    }

    fn switch_to(repo: &Repository, mode: SwitchMode, dry_run: bool) -> SwitchOutcome {
        switch_repository(
            repo,
            &SwitchOptions {
                mode,
                dry_run,
                hooks: Default::default(),
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("switch")
    }

    #[test]
    fn leaving_detached_head_reports_stranded_commit() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        let detached = commit_on_detached_head(&repo, "two\n", main_tip);

        let outcome = switch_to(
            &repo,
            SwitchMode::Existing {
                branch: "main".into(),
                oid: main_tip,
            },
            false,
        );

        assert_eq!(outcome.stranded_total, 1);
        assert!(!outcome.stranded_truncated);
        assert_eq!(outcome.stranded_commits.len(), 1);
        assert_eq!(outcome.stranded_commits[0].oid, detached);
        assert_eq!(outcome.stranded_commits[0].subject, "c");
        assert!(!outcome.stranded_commits[0].change_id.is_empty());
        assert!(matches!(
            resolve_head_for_repository(&repo).expect("head"),
            HeadState::Branch { refname, .. } if refname == "refs/heads/main"
        ));
    }

    #[test]
    fn detaching_elsewhere_reports_stranded_commits_newest_first() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        let first = commit_on_detached_head(&repo, "two\n", main_tip);
        let second = commit_on_detached_head(&repo, "three\n", first);

        let outcome = switch_to(
            &repo,
            SwitchMode::Detached {
                commit: main_tip,
                reflog_to_label: "main".into(),
            },
            false,
        );

        assert!(outcome.is_detached());
        assert_eq!(outcome.stranded_total, 2);
        let oids: Vec<_> = outcome.stranded_commits.iter().map(|c| c.oid).collect();
        assert_eq!(oids, vec![second, first]);
    }

    #[test]
    fn create_from_detached_head_strands_nothing() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        let detached = commit_on_detached_head(&repo, "two\n", main_tip);

        let outcome = switch_to(
            &repo,
            SwitchMode::Create {
                branch: "keep".into(),
                start_oid: detached,
                create_source_label: "HEAD".into(),
                track_upstream: None,
            },
            false,
        );

        assert!(outcome.created);
        assert_eq!(outcome.stranded_total, 0);
        assert!(outcome.stranded_commits.is_empty());
    }

    #[test]
    fn create_elsewhere_from_detached_head_reports_stranded_commit() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        let detached = commit_on_detached_head(&repo, "two\n", main_tip);

        let outcome = switch_to(
            &repo,
            SwitchMode::Create {
                branch: "fresh".into(),
                start_oid: main_tip,
                create_source_label: "main".into(),
                track_upstream: None,
            },
            false,
        );

        assert_eq!(outcome.stranded_total, 1);
        assert_eq!(outcome.stranded_commits[0].oid, detached);
    }

    /// Commit with `tree` and an explicit committer date.
    fn commit_dated(repo: &Repository, tree: ObjectId, parent: ObjectId, time: i64) -> ObjectId {
        let ident = format!("T <t@t> {time} +0000");
        let commit = CommitData {
            tree,
            parents: vec![parent],
            author: ident.clone(),
            committer: ident,
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: format!("at {time}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        repo.odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit")
    }

    #[test]
    fn skewed_dates_do_not_strand_a_commit_branches_hold() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        let tree = commit_tree(&repo, &main_tip).expect("tree");
        let skewed = commit_dated(&repo, tree, main_tip, 1_000);
        let old_tip = commit_dated(&repo, tree, skewed, 1_100);
        let behind = commit_dated(&repo, tree, skewed, 150);
        let held = commit_dated(&repo, tree, behind, 200);
        for i in 0..8 {
            refs::write_ref(repo.git_dir(), &format!("refs/heads/held-{i}"), &held).expect("ref");
        }
        let mut filler = main_tip;
        for i in 0..10 {
            filler = commit_dated(&repo, tree, filler, 500 + i);
        }
        refs::write_ref(repo.git_dir(), "refs/heads/filler", &filler).expect("ref");
        refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", &old_tip)
            .expect("detach");

        let outcome = switch_to(
            &repo,
            SwitchMode::Existing {
                branch: "main".into(),
                oid: main_tip,
            },
            true,
        );

        assert_eq!(outcome.stranded_total, 1);
        assert_eq!(outcome.stranded_commits[0].oid, old_tip);
    }

    #[test]
    fn detached_head_held_by_another_ref_strands_nothing() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        let detached = commit_on_detached_head(&repo, "two\n", main_tip);
        refs::write_ref(repo.git_dir(), "refs/tags/keep", &detached).expect("tag");

        let outcome = switch_to(
            &repo,
            SwitchMode::Existing {
                branch: "main".into(),
                oid: main_tip,
            },
            false,
        );

        assert_eq!(outcome.stranded_total, 0);
        assert!(outcome.stranded_commits.is_empty());
    }

    #[test]
    fn dry_run_reports_stranded_commit_without_moving_head() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        let detached = commit_on_detached_head(&repo, "two\n", main_tip);

        let outcome = switch_to(
            &repo,
            SwitchMode::Existing {
                branch: "main".into(),
                oid: main_tip,
            },
            true,
        );

        assert!(outcome.dry_run);
        assert_eq!(outcome.stranded_total, 1);
        assert_eq!(outcome.stranded_commits[0].oid, detached);
        assert_eq!(
            resolve_head_for_repository(&repo).expect("head"),
            HeadState::Detached { oid: detached }
        );
    }

    #[test]
    fn leaving_a_branch_strands_nothing() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        commit_on_branch(&repo, "topic", "f", "two\n", Some(main_tip));

        let outcome = switch_to(
            &repo,
            SwitchMode::Existing {
                branch: "main".into(),
                oid: main_tip,
            },
            false,
        );

        assert_eq!(outcome.stranded_total, 0);
        assert!(outcome.stranded_commits.is_empty());
    }

    #[test]
    fn stranded_listing_is_capped_at_the_default_limit() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "base\n", None);
        let mut tip = main_tip;
        for i in 0..=DEFAULT_ORPHAN_LIMIT {
            tip = commit_on_detached_head(&repo, &format!("{i}\n"), tip);
        }

        let outcome = switch_to(
            &repo,
            SwitchMode::Existing {
                branch: "main".into(),
                oid: main_tip,
            },
            true,
        );

        assert_eq!(outcome.stranded_total, DEFAULT_ORPHAN_LIMIT + 1);
        assert!(outcome.stranded_truncated);
        assert_eq!(outcome.stranded_commits.len(), DEFAULT_ORPHAN_LIMIT);
        assert_eq!(outcome.stranded_commits[0].oid, tip);
    }
}
