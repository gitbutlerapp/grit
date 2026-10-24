//! `git merge` index/head algorithm core.
//!
//! The full merge command in the `grit` binary is a large stateful sequencer:
//! it parses args, launches the merge-message editor, runs hooks, prints
//! progress (`"Trying merge strategy ..."`, `"Already up to date."`), and
//! writes the working tree. Those responsibilities — argv parsing, terminal
//! output, editor/hook subprocess dispatch, and exit-code mapping — stay in the
//! CLI.
//!
//! What lives here is the self-contained, presentation-free part of that
//! sequencer: the pure data transforms over trees, indexes, and the commit
//! graph that the CLI calls but that compute results from repository data
//! alone.
//!
//! # What this module owns
//!
//! - [`reduce_octopus_merge_heads`] — Git's "reduce parents": drop any merge
//!   head that is an ancestor of another listed head, preserving input order.
//! - [`compose_fast_forward_index`] — build the post-fast-forward index from the
//!   target tree, carrying forward staged additions that the fast-forward does
//!   not touch.
//! - [`compose_octopus_final_index`] — fold staged paths from before an octopus
//!   merge back into the merge result.
//! - [`tree_to_index_entries`] / [`tree_to_map`] — flatten a tree object into a
//!   recursive list (or path-keyed map) of [`IndexEntry`] values; the shared
//!   building block the three transforms above (and most of the CLI merge code)
//!   are written against.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::change_id::{change_id, ChangeId, ChangeIdPolicy};
use crate::config::ConfigSet;
use crate::diagnostics::Warning;
use crate::diff::diff_trees;
use crate::error::{Error, Result};
use crate::ident::parse_signature_times;
use crate::ident_resolve::{
    resolve_identity_with, resolve_loose_committer_identity_with, IdentRole,
};
use crate::index::{Index, IndexEntry};
use crate::merge_base::{
    is_ancestor, merge_bases_first_vs_rest, walk_commits_reachable_excluding_ancestors_of,
    ReachableWalkLimit,
};
use crate::merge_file::{ConflictStyle, MergeFavor};
use crate::merge_trees::{
    apply_tree_merge_output_to_index, materialized_merge_result_tree, merge_trees_three_way,
    TreeMergeConflictPresentation, TreeMergeOutput, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind};
use crate::path_population::validate_tree_walk_entry;
use crate::path_validate::ProtectOptions;
use crate::porcelain::checkout;
use crate::porcelain::commit::{
    normalize_commit_message, stamp_change_id_on_new_commit, write_commit_object,
};
use crate::porcelain::index_from_tree::{
    index_from_tree_with_worktree_stats, refresh_stage0_index_stats_from_worktree,
};
use crate::porcelain::restore::conflict_style_from_config;
use crate::porcelain::switch::{
    checkout_prepared_tree_transition_for_merge, prepare_tree_transition_allow_local,
    verify_tree_transition_allow_local, TreeTransitionPrepareError, TreeTransitionVerifyError,
};
use crate::porcelain::tree_merge_checkout::finalize_clean_tree_merge_index;
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::progress::ProgressSink;
use crate::refs;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
use crate::repo::Repository;
use crate::state::{clear_merge_state, read_merge_heads, read_merge_msg, read_orig_head};
use crate::stripspace::{process as stripspace_process, Mode as StripspaceMode};
use thiserror::Error;

use crate::porcelain::add::stage_for_commit_on_index;
use crate::porcelain::commit::{
    create_commit_with_loaded_index, default_commit_scope_from_config, CommitRequest, CommitScope,
};
use crate::worktree_rules::WorktreeRules;
use crate::write_tree::write_tree_from_index;

/// Flatten a tree object into a recursive list of [`IndexEntry`] values.
///
/// Sub-trees are walked depth-first and their paths are prefixed with `prefix`
/// joined by `/`. The returned entries carry stage 0, zeroed stat fields, and
/// the tree's mode/oid — exactly what a freshly read-tree index holds.
///
/// # Errors
///
/// Returns an error if `oid` cannot be read or does not name a tree object.
pub fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<IndexEntry>> {
    let protect = ProtectOptions::from_config(&*repo.config()?);
    let prefix_bytes = crate::repo_relpath::storage_bytes(prefix);
    tree_to_index_entries_with_protect(repo, oid, &prefix_bytes, protect)
}

fn tree_to_index_entries_with_protect(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &[u8],
    protect: ProtectOptions,
) -> Result<Vec<IndexEntry>> {
    let obj = repo.odb().read(oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::Message(format!("expected tree, got {}", obj.kind)));
    }
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();

    for te in entries {
        let path_bytes = crate::repo_relpath::join_rel(prefix, &te.name);
        validate_tree_walk_entry(prefix, &te.name, te.mode, protect)?;

        if te.mode == 0o040000 {
            let sub = tree_to_index_entries_with_protect(repo, &te.oid, &path_bytes, protect)?;
            result.extend(sub);
        } else {
            result.push(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: te.mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid: te.oid,
                flags: path_bytes.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path_bytes,
                base_index_pos: 0,
            });
        }
    }
    Ok(result)
}

/// Index a flat entry list by path, keeping the last entry for any duplicate.
#[must_use]
pub fn tree_to_map(entries: Vec<IndexEntry>) -> HashMap<Vec<u8>, IndexEntry> {
    let mut out = HashMap::new();
    for e in entries {
        out.insert(e.path.clone(), e);
    }
    out
}

/// Build the index that results from fast-forwarding `current_index` to
/// `target_tree`.
///
/// Starts from `target_tree`'s flattened entries and carries forward any
/// stage-0 path that is staged in `current_index`, is absent from
/// `target_tree`, and is also absent from `head_tree` — i.e. a staged addition
/// that the fast-forward should not discard.
///
/// # Errors
///
/// Returns an error if `target_tree` or `head_tree` cannot be read as trees.
pub fn compose_fast_forward_index(
    repo: &Repository,
    target_tree: ObjectId,
    head_tree: ObjectId,
    current_index: &Index,
) -> Result<Index> {
    let mut new_entries = tree_to_index_entries(repo, &target_tree, "")?;
    let target_paths: BTreeSet<Vec<u8>> = new_entries.iter().map(|e| e.path.clone()).collect();
    let head_entries = tree_to_map(tree_to_index_entries(repo, &head_tree, "")?);
    for e in &current_index.entries {
        if e.stage() != 0 {
            continue;
        }
        if target_paths.contains(&e.path) {
            continue;
        }
        // Staged addition: not in HEAD — keep alongside the fast-forwarded tree.
        if !head_entries.contains_key(&e.path) {
            new_entries.push(e.clone());
        }
    }
    let mut index = Index::new();
    index.entries = new_entries;
    index.sort();
    // A duplicate-entry tree (t4058) flattens to several identical-path entries; Git's index keeps
    // only one per path. Restore that invariant so status/diff against HEAD is consistent.
    index.dedup_paths_keep_last();
    Ok(index)
}

/// Preserve staged paths from before an octopus merge that the merge result does not touch
/// (e.g. unrelated `git add`), matching Git's index composition.
pub fn compose_octopus_final_index(pre_merge_index: &Index, final_index: &mut Index) {
    let final_paths: BTreeSet<Vec<u8>> = final_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.clone())
        .collect();
    for e in &pre_merge_index.entries {
        if e.stage() != 0 {
            continue;
        }
        if final_paths.contains(&e.path) {
            continue;
        }
        final_index.push_entry_unsorted(e.clone());
    }
    final_index.sort();
}

/// Drop merge heads that are ancestors of another listed head (Git's "reduce parents").
///
/// Order of the remaining heads matches the first occurrence in the input (t7603).
///
/// # Errors
///
/// Returns an error if a commit-graph walk required for an ancestry test fails.
pub fn reduce_octopus_merge_heads(
    repo: &Repository,
    merge_oids: &[ObjectId],
    merge_names: &[String],
) -> Result<(Vec<ObjectId>, Vec<String>)> {
    debug_assert_eq!(merge_oids.len(), merge_names.len());
    let mut out_oids = Vec::with_capacity(merge_oids.len());
    let mut out_names = Vec::with_capacity(merge_names.len());
    for i in 0..merge_oids.len() {
        let oid = merge_oids[i];
        let redundant = merge_oids
            .iter()
            .enumerate()
            .any(|(j, &other)| j != i && is_ancestor(repo, oid, other).unwrap_or(false));
        if !redundant {
            out_oids.push(oid);
            out_names.push(merge_names[i].clone());
        }
    }
    Ok((out_oids, out_names))
}

pub use crate::operation::{conflicts_from_tree_merge, tree_merge_has_conflicts};
pub use crate::operation::{Conflict as MergeConflictDetail, ConflictKind as MergeConflictKind};

/// Fast-forward policy for branch integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FastForwardPolicy {
    /// Fast-forward when `other_oid` is a descendant of `into_oid`.
    #[default]
    Allow,
    /// Refuse when a fast-forward is not possible; nothing changes.
    Only,
    /// Always record a merge commit when a real merge is required, even when a fast-forward is possible.
    Never,
}

/// Whether integration stops before creating a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MergeCommitMode {
    /// Create a merge commit (or fast-forward) on success without conflicts.
    #[default]
    Commit,
    /// Apply the merge to the index and worktree but leave `MERGE_HEAD` for a follow-up commit.
    NoCommit,
    /// Apply the merged tree and write `SQUASH_MSG` without updating the branch ref.
    Squash,
}

/// Options for [`integrate_branch_with`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MergeOptions {
    /// When to fast-forward versus creating a merge commit.
    pub fast_forward: FastForwardPolicy,
    /// Whether to stop before committing.
    pub commit: MergeCommitMode,
    /// Overrides the default merge commit / `MERGE_MSG` text when set.
    pub message: Option<String>,
    /// Prefer ours or theirs when both sides changed the same region (`-X ours|theirs`).
    pub favor: MergeFavor,
    /// Conflict marker style; when unset, [`resolve_merge_conflict_style`] reads config.
    pub conflict_style: Option<ConflictStyle>,
    /// When histories share no commits, use the empty tree as merge base (Git `--allow-unrelated-histories`).
    pub allow_unrelated: bool,
}

/// Resolve conflict marker style: explicit option, then `merge.conflictStyle`, then `merge`.
#[must_use]
pub fn resolve_merge_conflict_style(
    repo: &Repository,
    override_style: Option<ConflictStyle>,
) -> ConflictStyle {
    override_style.unwrap_or_else(|| conflict_style_from_config(repo))
}

/// Options for [`integrate_branch_with_options`] (pull fast-forward toggle).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegrateBranchOptions {
    /// When `false`, a fast-forward is not performed even when `other_oid` is a
    /// descendant of `into_oid`; a merge commit is recorded instead.
    pub allow_fast_forward: bool,
}

impl Default for IntegrateBranchOptions {
    fn default() -> Self {
        Self {
            allow_fast_forward: true,
        }
    }
}

/// Options for [`predict_merge`].
#[derive(Clone, Debug)]
pub struct MergePredictionOptions {
    /// When `false`, a fast-forward is not considered even when `other_oid` is a
    /// descendant of `into_oid`; a three-way merge is simulated instead.
    pub allow_fast_forward: bool,
    /// When `false`, histories with no merge base yield [`MergePrediction::Unrelated`].
    /// When `true`, an empty merge base is treated like Git's `--allow-unrelated-histories`
    /// (reserved for a later step; unrelated histories still report `Unrelated` today).
    pub allow_unrelated_histories: bool,
    /// Merge-favor policy for the simulated three-way merge (ours/theirs/normal).
    pub favor: MergeFavor,
    /// Whitespace handling for the simulated three-way merge.
    pub whitespace: WhitespaceMergeOptions,
    /// Label for the incoming side in conflict-marker metadata (presentation only).
    pub theirs_label: String,
}

impl Default for MergePredictionOptions {
    fn default() -> Self {
        Self {
            allow_fast_forward: true,
            allow_unrelated_histories: false,
            favor: MergeFavor::default(),
            whitespace: WhitespaceMergeOptions::default(),
            theirs_label: "branch".into(),
        }
    }
}

/// In-memory prediction of merging `other_oid` into the commit `into_oid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergePrediction {
    /// The current tip already contains the other side.
    UpToDate,
    /// The current tip could fast-forward to the other side.
    FastForward,
    /// A three-way merge would succeed; `tree` is the merged result tree OID.
    Clean {
        /// Merged tree OID (matches `git merge-tree --write-tree` on success).
        tree: ObjectId,
    },
    /// A three-way merge would stop with conflicts.
    Conflicts {
        /// Conflicted paths and kinds (same classification as a real merge).
        conflicts: Vec<MergeConflictDetail>,
    },
    /// The two commits share no common history.
    Unrelated,
}

/// Predict merging `other_oid` into `into_oid` without touching refs, the index,
/// merge state files, or the working tree.
///
/// Object writes from the merge are routed through the ODB in-memory overlay and
/// discarded before return, so the on-disk object store is unchanged.
///
/// # Errors
///
/// Propagates object read and merge failures. Does not require a clean worktree.
pub fn predict_merge(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    options: &MergePredictionOptions,
) -> Result<MergePrediction> {
    if into_oid == other_oid || is_ancestor(repo, other_oid, into_oid)? {
        return Ok(MergePrediction::UpToDate);
    }

    if options.allow_fast_forward && is_ancestor(repo, into_oid, other_oid)? {
        return Ok(MergePrediction::FastForward);
    }

    let base_oid = merge_bases_first_vs_rest(repo, into_oid, &[other_oid])?
        .into_iter()
        .next();
    let Some(base_oid) = base_oid else {
        return Ok(MergePrediction::Unrelated);
    };
    let _ = options.allow_unrelated_histories;

    let into_tree = repo.odb().commit_tree(&into_oid)?;
    let other_tree = repo.odb().commit_tree(&other_oid)?;
    let base_tree = repo.odb().commit_tree(&base_oid)?;

    let presentation = TreeMergeConflictPresentation {
        label_theirs: crate::merge_trees::TheirsConflictLabel::Fixed(&options.theirs_label),
        df_directory_ours_commit: Some(into_oid),
        df_directory_theirs_commit: Some(other_oid),
        ..TreeMergeConflictPresentation::default()
    };

    let _overlay = MemOverlayGuard::enter(repo.odb());
    let merged = merge_trees_three_way(
        repo,
        base_tree,
        into_tree,
        other_tree,
        options.favor,
        options.whitespace,
        None,
        presentation,
    )?;
    if tree_merge_has_conflicts(&merged) {
        return Ok(MergePrediction::Conflicts {
            conflicts: conflicts_from_tree_merge(&merged),
        });
    }
    let tree = materialized_merge_result_tree(repo.odb(), &merged)?;
    Ok(MergePrediction::Clean { tree })
}

/// Planned merge of `other_oid` into `into_oid` without touching refs, the index, merge state,
/// or the working tree (same in-memory three-way merge as [`predict_merge`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeDryRunPlan {
    /// The current tip already contains the other side.
    UpToDate,
    /// The branch ref would move to `tip` (fast-forward).
    FastForward {
        /// Tip commit the current branch would become.
        tip: ObjectId,
        /// Stored `change-id` on `tip` when present.
        change_id: Option<ChangeId>,
    },
    /// `--ff-only` would refuse because histories diverged.
    RefusedNotFf,
    /// A two-parent merge commit would be recorded (tree merge already succeeded).
    MergeCommit {
        /// Parent commits of the merge commit (current tip, then merged branch).
        parents: Vec<ObjectId>,
    },
    /// The three-way merge would stop with conflicts.
    Conflicts {
        /// Conflicted paths and kinds (same classification as a real merge).
        conflicts: Vec<MergeConflictDetail>,
    },
}

/// Predict integrating `other_oid` into the commit at `into_oid` using [`MergeOptions`], without
/// mutating the repository.
///
/// # Errors
///
/// Propagates object read and merge failures. Returns [`Error::Message`] when histories are
/// unrelated and [`MergeOptions::allow_unrelated`] is false.
pub fn plan_branch_merge(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
) -> Result<MergeDryRunPlan> {
    if into_oid == other_oid || is_ancestor(repo, other_oid, into_oid)? {
        return Ok(MergeDryRunPlan::UpToDate);
    }

    let ff_possible = is_ancestor(repo, into_oid, other_oid)?;
    if matches!(options.fast_forward, FastForwardPolicy::Only) && !ff_possible {
        return Ok(MergeDryRunPlan::RefusedNotFf);
    }

    if should_take_fast_forward(options, ff_possible) {
        return Ok(MergeDryRunPlan::FastForward {
            tip: other_oid,
            change_id: stored_change_id_for_commit(repo, other_oid)?,
        });
    }

    let prediction_opts = MergePredictionOptions {
        allow_fast_forward: false,
        allow_unrelated_histories: options.allow_unrelated,
        favor: options.favor,
        whitespace: WhitespaceMergeOptions::default(),
        theirs_label: label.to_owned(),
    };
    match predict_merge(repo, into_oid, other_oid, &prediction_opts)? {
        MergePrediction::UpToDate | MergePrediction::FastForward => Ok(MergeDryRunPlan::UpToDate),
        MergePrediction::Clean { .. } => Ok(MergeDryRunPlan::MergeCommit {
            parents: vec![into_oid, other_oid],
        }),
        MergePrediction::Conflicts { conflicts } => Ok(MergeDryRunPlan::Conflicts { conflicts }),
        MergePrediction::Unrelated => Err(Error::Message(format!(
            "'{label}' has no common history with the current branch"
        ))),
    }
}

/// Enable the ODB mem overlay for the scope of a [`predict_merge`] call without disturbing
/// an overlay already active on the shared [`Repository`] handle.
pub(crate) struct MemOverlayGuard<'a> {
    odb: &'a crate::objects::odb::Odb,
    enabled_here: bool,
}

impl<'a> MemOverlayGuard<'a> {
    pub(crate) fn enter(odb: &'a crate::objects::odb::Odb) -> Self {
        let already_active = odb.is_mem_overlay_active();
        if !already_active {
            odb.enable_mem_overlay();
        }
        Self {
            odb,
            enabled_here: !already_active,
        }
    }
}

impl Drop for MemOverlayGuard<'_> {
    fn drop(&mut self) {
        if self.enabled_here {
            self.odb.disable_mem_overlay();
        }
    }
}

impl From<IntegrateBranchOptions> for MergeOptions {
    fn from(options: IntegrateBranchOptions) -> Self {
        Self {
            fast_forward: if options.allow_fast_forward {
                FastForwardPolicy::Allow
            } else {
                FastForwardPolicy::Never
            },
            ..Self::default()
        }
    }
}

/// Result of integrating another branch into the current one ([`integrate_branch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrateBranchOutcome {
    /// The current branch already contains the other tip.
    UpToDate,
    /// The current branch fast-forwarded to the other tip.
    FastForward {
        /// New branch tip after the fast-forward.
        oid: ObjectId,
        /// Stored `change-id` on the fast-forward tip when present.
        change_id: Option<ChangeId>,
    },
    /// A merge commit was created without conflicts.
    Merged {
        /// New merge commit OID.
        oid: ObjectId,
        /// Stored `change-id` on the merge commit when present.
        change_id: Option<ChangeId>,
    },
    /// The merge has conflicts; the repository is left in a merge-in-progress state.
    Conflicts {
        /// Conflicted paths in repository-relative form, sorted and deduplicated.
        paths: Vec<String>,
        /// Per-path conflict classification (same order as `paths` after sorting).
        details: Vec<MergeConflictDetail>,
    },
    /// A `--squash` merge applied the merged tree and wrote `SQUASH_MSG`.
    Squashed,
    /// A `--no-commit` merge applied the merged tree and recorded merge state.
    Stopped,
}

/// Integrate `other_oid` into the branch `into_ref` (currently at `into_oid`).
///
/// On success without conflicts, updates the branch ref, index, and worktree. On
/// conflicts, records `MERGE_HEAD` / `ORIG_HEAD` / `MERGE_MSG`, writes conflict
/// markers and a partially merged index, and leaves the branch ref unchanged.
///
/// # Errors
///
/// Propagates merge, checkout, ref, and I/O failures. Returns [`Error::Message`] when
/// the histories are unrelated, the worktree is not clean, or `into_ref` no longer points
/// at `into_oid` (checked before the worktree is touched, and again when the ref moves).
pub fn integrate_branch(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
) -> Result<IntegrateBranchOutcome> {
    integrate_branch_with(
        repo,
        into_ref,
        into_oid,
        other_oid,
        label,
        &MergeOptions::default(),
    )
}

/// Integrate `other_oid` into the branch at `into_ref` with explicit merge options.
///
/// # Errors
///
/// Same as [`integrate_branch`], plus [`Error::NotFastForward`] when
/// [`FastForwardPolicy::Only`] cannot fast-forward.
pub fn integrate_branch_with(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
) -> Result<IntegrateBranchOutcome> {
    if into_oid == other_oid || is_ancestor(repo, other_oid, into_oid)? {
        return Ok(IntegrateBranchOutcome::UpToDate);
    }
    ensure_branch_tip(repo, into_ref, Some(into_oid))?;

    let into_tree = repo.odb().commit_tree(&into_oid)?;
    let other_tree = repo.odb().commit_tree(&other_oid)?;
    let ff_possible = is_ancestor(repo, into_oid, other_oid)?;

    if matches!(options.fast_forward, FastForwardPolicy::Only) && !ff_possible {
        return Err(Error::NotFastForward);
    }

    if should_take_fast_forward(options, ff_possible) {
        if matches!(options.commit, MergeCommitMode::Squash) {
            merge_checkout_tree_transition(repo, Some(&into_tree), &other_tree)?;
            let mut snapshot = crate::porcelain::worktree_guard::load_worktree_snapshot(repo)?;
            let ff_index =
                compose_fast_forward_index(repo, other_tree, into_tree, &snapshot.index)?;
            snapshot.index = ff_index;
            repo.write_index(&mut snapshot.index)?;
            write_squash_msg_file(repo, into_oid, other_oid)?;
            return Ok(IntegrateBranchOutcome::Squashed);
        }

        merge_checkout_tree_transition(repo, Some(&into_tree), &other_tree)?;
        move_branch(
            repo,
            into_ref,
            into_oid,
            other_oid,
            &format!("merge {label}: fast-forward"),
        )?;
        return Ok(IntegrateBranchOutcome::FastForward {
            oid: other_oid,
            change_id: stored_change_id_for_commit(repo, other_oid)?,
        });
    }

    let merge_base = merge_bases_first_vs_rest(repo, into_oid, &[other_oid])?;
    let label_base = if let Some(base_oid) = merge_base.first().copied() {
        crate::commit_pretty::abbrev_hex(&base_oid, 7)
    } else if options.allow_unrelated {
        "empty tree".to_owned()
    } else {
        return Err(Error::Message(format!(
            "'{label}' has no common history with the current branch"
        )));
    };
    let base_tree = if let Some(base_oid) = merge_base.into_iter().next() {
        repo.odb().commit_tree(&base_oid)?
    } else {
        empty_tree_oid(repo)?
    };

    let style = resolve_merge_conflict_style(repo, options.conflict_style);
    let presentation = TreeMergeConflictPresentation {
        label_theirs: crate::merge_trees::TheirsConflictLabel::Fixed(label),
        label_base: &label_base,
        df_directory_ours_commit: Some(into_oid),
        df_directory_theirs_commit: Some(other_oid),
        style,
        ..TreeMergeConflictPresentation::default()
    };
    let merged = merge_trees_three_way(
        repo,
        base_tree,
        into_tree,
        other_tree,
        options.favor,
        WhitespaceMergeOptions::default(),
        None,
        presentation,
    )?;

    if tree_merge_has_conflicts(&merged) {
        let details = conflicts_from_tree_merge(&merged);
        let paths: Vec<String> = details.iter().map(|c| c.path.clone()).collect();
        let materialized = materialized_merge_result_tree(repo.odb(), &merged)?;
        ensure_merge_tree_transition_allowed(repo, Some(&into_tree), &materialized)?;
        apply_conflict_merge_worktree(repo, &into_tree, &merged)?;
        let merge_msg = merge_message_text(label, options.message.as_deref());
        if matches!(options.commit, MergeCommitMode::Squash) {
            write_squash_msg_file(repo, into_oid, other_oid)?;
            write_orig_head(repo.git_dir(), into_oid)?;
            write_squash_conflict_merge_msg(repo.git_dir(), &paths, options.message.as_deref())?;
        } else {
            record_merge_in_progress(repo.git_dir(), into_oid, other_oid, merge_msg)?;
        }
        return Ok(IntegrateBranchOutcome::Conflicts { paths, details });
    }

    let commit_preflight = if matches!(options.commit, MergeCommitMode::Commit) {
        let config = repo.config()?;
        Some(MergeCommitPreflight {
            author: strict_commit_identity(repo, config.as_ref(), IdentRole::Author)?,
            committer: strict_commit_identity(repo, config.as_ref(), IdentRole::Committer)?,
            config,
        })
    } else {
        None
    };

    merge_checkout_tree_transition(repo, Some(&into_tree), &merged.result_tree)?;
    let mut snapshot = crate::porcelain::worktree_guard::load_worktree_snapshot(repo)?;
    finalize_clean_tree_merge_index(repo, &mut snapshot.index, &merged)?;
    repo.write_index(&mut snapshot.index)?;

    match options.commit {
        MergeCommitMode::Squash => {
            write_squash_msg_file(repo, into_oid, other_oid)?;
            Ok(IntegrateBranchOutcome::Squashed)
        }
        MergeCommitMode::NoCommit => {
            record_merge_in_progress(
                repo.git_dir(),
                into_oid,
                other_oid,
                merge_message_text(label, options.message.as_deref()),
            )?;
            write_merge_mode_for_no_commit(
                repo.git_dir(),
                matches!(options.fast_forward, FastForwardPolicy::Never),
            )?;
            Ok(IntegrateBranchOutcome::Stopped)
        }
        MergeCommitMode::Commit => {
            let preflight = commit_preflight
                .ok_or_else(|| Error::Message("merge commit preflight missing".into()))?;
            let config = preflight.config;
            let mut commit = CommitData {
                tree: merged.result_tree,
                parents: vec![into_oid, other_oid],
                author: preflight.author,
                committer: preflight.committer,
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: merge_message_text(label, options.message.as_deref()),
                raw_message: None,
                extra_headers: Vec::new(),
            };
            let policy = ChangeIdPolicy::from_config(config.as_ref());
            stamp_change_id_on_new_commit(repo, &policy, &mut commit)?;
            let oid = write_commit_object(repo, &commit, None)?;
            move_branch(repo, into_ref, into_oid, oid, &format!("merge {label}"))?;
            Ok(IntegrateBranchOutcome::Merged {
                oid,
                change_id: change_id(&commit),
            })
        }
    }
}

/// Like [`integrate_branch`], with explicit fast-forward policy for [`pull`](crate::porcelain::pull).
///
/// # Errors
///
/// Same as [`integrate_branch_with`].
pub fn integrate_branch_with_options(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: IntegrateBranchOptions,
) -> Result<IntegrateBranchOutcome> {
    integrate_branch_with(repo, into_ref, into_oid, other_oid, label, &options.into())
}

struct MergeCommitPreflight {
    author: String,
    committer: String,
    config: std::sync::Arc<ConfigSet>,
}

fn should_take_fast_forward(options: &MergeOptions, ff_possible: bool) -> bool {
    if !ff_possible {
        return false;
    }
    if matches!(options.fast_forward, FastForwardPolicy::Never) {
        return false;
    }
    !matches!(options.commit, MergeCommitMode::NoCommit)
}

fn merge_message_text(label: &str, override_message: Option<&str>) -> String {
    if let Some(msg) = override_message {
        return normalize_commit_message(msg).unwrap_or_else(|_| format!("Merge {label}\n"));
    }
    format!("Merge {label}\n")
}

/// Write `MERGE_MODE` for `--no-commit`, matching Git's bytes (empty file or `no-ff` without newline).
fn write_merge_mode_for_no_commit(git_dir: &Path, no_ff: bool) -> Result<()> {
    let bytes = if no_ff { b"no-ff".as_slice() } else { b"" };
    fs::write(git_dir.join("MERGE_MODE"), bytes).map_err(Error::Io)
}

fn write_squash_conflict_merge_msg(
    git_dir: &Path,
    paths: &[String],
    override_message: Option<&str>,
) -> Result<()> {
    let mut body = String::new();
    if let Some(msg) = override_message.filter(|m| !m.trim().is_empty()) {
        body.push_str(msg.trim());
        if !body.ends_with('\n') {
            body.push('\n');
        }
    } else {
        body.push('\n');
    }
    body.push_str("# Conflicts:\n");
    for path in paths {
        body.push_str("#\t");
        body.push_str(path);
        body.push('\n');
    }
    fs::write(git_dir.join("MERGE_MSG"), body).map_err(Error::Io)
}

fn write_squash_msg_file(repo: &Repository, into_oid: ObjectId, other_oid: ObjectId) -> Result<()> {
    let body = format_git_squash_msg(repo, into_oid, other_oid)?;
    fs::write(repo.git_dir().join("SQUASH_MSG"), body).map_err(Error::Io)
}

fn format_git_squash_msg(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
) -> Result<String> {
    let (_, mut commits) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        std::slice::from_ref(&other_oid),
        std::slice::from_ref(&into_oid),
        ReachableWalkLimit::Unlimited,
    )?;
    commits.reverse();
    let mut out = String::from("Squashed commit of the following:\n\n");
    for oid in commits {
        let obj = repo.odb().read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        out.push_str(&format!("commit {}\n", oid.to_hex()));
        out.push_str(&format!("Author: {}\n", squash_author_line(&commit.author)));
        out.push_str(&format!("Date:   {}\n\n", squash_date_line(&commit.author)));
        for line in commit.message.lines() {
            out.push_str(&format!("    {line}\n"));
        }
        out.push('\n');
    }
    Ok(out)
}

fn squash_author_line(author: &str) -> String {
    let Some(lt) = author.find('<') else {
        return author.trim().to_owned();
    };
    let Some(gt) = author[lt..].find('>') else {
        return author.trim().to_owned();
    };
    let name = author[..lt].trim();
    let email = &author[lt..=lt + gt];
    format!("{name} {email}")
}

fn squash_date_line(author: &str) -> String {
    let Some(parsed) = parse_signature_times(author) else {
        return String::new();
    };
    let offset_str = &author[parsed.tz_hhmm_range.clone()];
    let Ok(dt) =
        time::OffsetDateTime::from_unix_timestamp(parsed.unix_seconds + parsed.tz_offset_secs)
    else {
        return String::new();
    };
    let weekday = match dt.weekday() {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    };
    let month = match dt.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    format!(
        "{weekday} {month} {} {:02}:{:02}:{:02} {} {offset_str}",
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.year()
    )
}

fn stored_change_id_for_commit(repo: &Repository, oid: ObjectId) -> Result<Option<ChangeId>> {
    let obj = repo.odb().read(&oid)?;
    Ok(change_id(&parse_commit(&obj.data)?))
}

/// Options for [`continue_merge`].
#[derive(Debug, Clone)]
pub struct ContinueOptions {
    /// When set, replaces the message from `MERGE_MSG` (comment lines stripped).
    pub message: Option<String>,
    /// Author identity in Git header form: `Name <email> <unix-time> <tz>`.
    pub author: String,
    /// Committer identity in the same form as [`Self::author`].
    pub committer: String,
}

/// Result of [`continue_merge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueOutcome {
    /// New merge commit OID.
    pub oid: ObjectId,
    /// Stored `change-id` on the merge commit when present.
    pub change_id: Option<ChangeId>,
}

/// Failure modes for [`continue_merge`].
#[derive(Debug, Error)]
pub enum ContinueMergeError {
    /// No `MERGE_HEAD` (or other merge sentinel) is recorded.
    #[error("there is no merge in progress")]
    NoMergeInProgress,
    /// Stage-1/2/3 entries remain in the index.
    #[error("you still have unmerged paths: {paths:?}")]
    UnmergedPaths {
        /// Repository-relative paths still in conflict stages.
        paths: Vec<String>,
    },
    /// Work tree content is resolved but the index still has conflict stages (`commit.autoStage=false`).
    #[error(
        "resolved in the work tree but not staged: {paths:?} — run `grit add` on those paths first"
    )]
    ResolvedNotStaged {
        /// Repository-relative paths cleared of conflict markers but not staged.
        paths: Vec<String>,
    },
    /// The resolved message would be empty after cleanup.
    #[error("empty commit message")]
    EmptyMessage,
    /// Underlying repository or commit failure.
    #[error(transparent)]
    Repository(#[from] Error),
}

/// Finish an in-progress merge after conflicts are resolved or after `--no-commit`.
///
/// Builds a merge commit from the current index tree with parents `HEAD` and each
/// `MERGE_HEAD`, clears merge state (including autostash and rerere), and updates
/// the checked-out branch.
///
/// # Errors
///
/// Returns [`ContinueMergeError::NoMergeInProgress`] when no merge is active,
/// [`ContinueMergeError::UnmergedPaths`] when conflict stages remain, or propagates
/// commit and I/O failures as [`ContinueMergeError::Repository`].
pub fn continue_merge(
    repo: &Repository,
    options: &ContinueOptions,
    progress: &mut dyn ProgressSink,
) -> std::result::Result<ContinueOutcome, ContinueMergeError> {
    let merge_heads = read_merge_heads(repo.git_dir())?;
    if merge_heads.is_empty() {
        return Err(ContinueMergeError::NoMergeInProgress);
    }

    let config = repo.config().map_err(ContinueMergeError::Repository)?;
    let auto_stage = default_commit_scope_from_config(config.as_ref()) == CommitScope::AutoStage;

    let mut index = repo.load_index().map_err(ContinueMergeError::Repository)?;
    if auto_stage {
        stage_for_commit_on_index(repo, &mut index, progress, false)
            .map_err(ContinueMergeError::Repository)?;
    }
    if index.has_unmerged_entries() {
        let paths = unmerged_index_paths(&index);
        if !auto_stage {
            let resolved = resolved_not_staged_paths(repo, &index, &paths);
            if !resolved.is_empty() {
                return Err(ContinueMergeError::ResolvedNotStaged { paths: resolved });
            }
        }
        return Err(ContinueMergeError::UnmergedPaths { paths });
    }

    let message = resolve_continue_merge_message(repo.git_dir(), options.message.as_deref())?;
    let req = CommitRequest {
        message,
        author: options.author.clone(),
        committer: options.committer.clone(),
        allow_empty: false,
        ..Default::default()
    };
    let outcome = create_commit_with_loaded_index(repo, &mut index, &req, progress)
        .map_err(ContinueMergeError::Repository)?;
    Ok(ContinueOutcome {
        oid: outcome.oid,
        change_id: outcome.change_id,
    })
}

fn resolve_continue_merge_message(
    git_dir: &Path,
    override_message: Option<&str>,
) -> std::result::Result<String, ContinueMergeError> {
    if let Some(raw) = override_message {
        return normalize_commit_message(raw).map_err(|_| ContinueMergeError::EmptyMessage);
    }

    let Some(raw) = read_merge_msg(git_dir).map_err(ContinueMergeError::Repository)? else {
        return Err(ContinueMergeError::EmptyMessage);
    };
    let processed = stripspace_process(
        raw.as_bytes(),
        &StripspaceMode::StripComments("#".to_owned()),
    );
    let message = String::from_utf8(processed).map_err(|_| ContinueMergeError::EmptyMessage)?;
    if message.trim().is_empty() {
        return Err(ContinueMergeError::EmptyMessage);
    }
    let mut message = message;
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(message)
}

fn unmerged_index_paths(index: &Index) -> Vec<String> {
    let mut paths: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() != 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn resolved_not_staged_paths(repo: &Repository, index: &Index, unmerged: &[String]) -> Vec<String> {
    let work_tree = match repo.work_tree() {
        Some(wt) => wt,
        None => return Vec::new(),
    };
    let Ok(rules) = WorktreeRules::from_repository(repo, index) else {
        return Vec::new();
    };
    let mut resolved = Vec::new();
    for path in unmerged {
        let raw_path = path.as_bytes();
        let abs = work_tree.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            continue;
        };
        let idx_mode = index
            .entries
            .iter()
            .find(|e| e.path == raw_path && e.stage() != 0)
            .map(|e| e.mode)
            .unwrap_or(0o100644);
        if !crate::rerere::worktree_metadata_needs_conflict_marker_inspection(&meta, idx_mode) {
            continue;
        }
        let Ok(wt_bytes) = std::fs::read(&abs) else {
            continue;
        };
        let marker_size = rules
            .file_attrs(path, false)
            .effective_conflict_marker_size();
        if !crate::rerere::worktree_bytes_have_unresolved_conflict_markers(&wt_bytes, marker_size) {
            resolved.push(path.clone());
        }
    }
    resolved.sort();
    resolved
}

/// Abort an in-progress merge, restoring index and worktree to [`read_orig_head`].
///
/// # Errors
///
/// Returns [`Error::Message`] when no merge is in progress, or propagates checkout and I/O errors.
pub fn abort_branch_merge(repo: &Repository) -> Result<()> {
    let git_dir = repo.git_dir();
    let in_merge = !read_merge_heads(git_dir)?.is_empty();
    let squash_conflict =
        git_dir.join("SQUASH_MSG").is_file() && git_dir.join("MERGE_MSG").is_file();
    if !in_merge && !squash_conflict {
        return Err(Error::Message(
            "there is no merge in progress — nothing to abort".into(),
        ));
    }
    restore_pre_merge_state(repo)
}

fn restore_pre_merge_state(repo: &Repository) -> Result<()> {
    let orig = read_orig_head(repo.git_dir())?
        .ok_or_else(|| Error::Message("merge in progress but ORIG_HEAD is missing".into()))?;
    let head_tree = repo.odb().commit_tree(&orig)?;

    let old_index = repo.load_index()?;
    let from_tree = write_tree_from_index(repo.odb(), &old_index, "")?;
    let mut clean_index = index_from_tree_with_worktree_stats(repo, &head_tree)?;
    repo.write_index(&mut clean_index)?;
    let changes = diff_trees(repo.odb(), Some(&from_tree), Some(&head_tree), "")?;
    checkout::checkout_tree_changes(repo, &changes)?;
    clear_merge_state(repo.git_dir())?;
    Ok(())
}

fn merge_overwrite_error_message(paths: &[String]) -> Error {
    if paths.is_empty() {
        Error::Message("you have uncommitted changes — commit them before merging".into())
    } else if paths.len() == 1 {
        Error::Message(format!(
            "local changes to '{}' would be overwritten by merge",
            paths[0]
        ))
    } else {
        Error::Message(format!(
            "local changes would be overwritten by merge:\n  {}",
            paths.join("\n  ")
        ))
    }
}

fn map_tree_transition_prepare_error_for_merge(err: TreeTransitionPrepareError) -> Error {
    match err {
        TreeTransitionPrepareError::OverwritePaths(paths) => merge_overwrite_error_message(&paths),
        TreeTransitionPrepareError::Failed(err) => err,
    }
}

fn ensure_merge_tree_transition_allowed(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<()> {
    prepare_tree_checkout(repo, from_tree, to_tree)?;
    match verify_tree_transition_allow_local(repo, from_tree, to_tree) {
        Ok(()) => Ok(()),
        Err(TreeTransitionVerifyError::WouldOverwrite(paths)) => {
            Err(merge_overwrite_error_message(&paths))
        }
        Err(TreeTransitionVerifyError::Failed(err)) => Err(err),
    }
}

fn merge_checkout_tree_transition(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<()> {
    prepare_tree_checkout(repo, from_tree, to_tree)?;
    let prepared = prepare_tree_transition_allow_local(repo, from_tree, to_tree, &HashSet::new())
        .map_err(map_tree_transition_prepare_error_for_merge)?;
    checkout_prepared_tree_transition_for_merge(repo, prepared)
}

fn empty_tree_oid(repo: &Repository) -> Result<ObjectId> {
    repo.odb().empty_tree_oid()
}

fn conflict_path_set(merged: &TreeMergeOutput) -> BTreeSet<Vec<u8>> {
    let mut set = BTreeSet::new();
    set.extend(merged.conflict_content.keys().cloned());
    for entry in merged.index.entries() {
        if entry.stage() != 0 {
            set.insert(entry.path.clone());
        }
    }
    set
}

pub(crate) fn apply_conflict_merge_worktree(
    repo: &Repository,
    head_tree: &ObjectId,
    merged: &TreeMergeOutput,
) -> Result<()> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;
    let materialized = materialized_merge_result_tree(repo.odb(), merged)?;
    let prepared =
        prepare_tree_transition_allow_local(repo, Some(head_tree), &materialized, &HashSet::new())
            .map_err(map_tree_transition_prepare_error_for_merge)?;
    let preserve_worktree_paths = prepared.preserve_worktree_paths().clone();
    let conflict_paths = conflict_path_set(merged);
    let mut index = repo.load_index()?;
    apply_tree_merge_output_to_index(&mut index, merged);
    if index.has_unmerged_entries() {
        index.clear_cache_tree();
    }
    repo.write_index(&mut index)?;

    let stage0_tree = write_tree_from_index(repo.odb(), &index, "")?;
    let changes = diff_trees(repo.odb(), Some(head_tree), Some(&stage0_tree), "")?;
    let filtered: Vec<_> = changes
        .into_iter()
        .filter(|change| {
            let path = change
                .new_path
                .as_deref()
                .or(change.old_path.as_deref())
                .unwrap_or("");
            !conflict_paths.contains(path.as_bytes()) && !preserve_worktree_paths.contains(path)
        })
        .collect();
    checkout::checkout_tree_changes(repo, &filtered)?;

    let rules = WorktreeRules::from_repository(repo, &index)?;
    for (path, oid) in &merged.conflict_content {
        let mode = index
            .get(path, 2)
            .or_else(|| index.get(path, 3))
            .map(|e| e.mode)
            .unwrap_or(0o100644);
        let rel = String::from_utf8_lossy(path);
        checkout::write_smudged_blob_to_worktree(repo, work_tree, &rules, rel.as_ref(), oid, mode)?;
    }
    refresh_stage0_index_stats_from_worktree(repo, work_tree, &mut index)?;
    repo.write_index(&mut index)?;
    Ok(())
}

fn write_orig_head(git_dir: &Path, into_oid: ObjectId) -> Result<()> {
    fs::write(
        git_dir.join("ORIG_HEAD"),
        format!("{}\n", into_oid.to_hex()),
    )
    .map_err(Error::Io)
}

fn record_merge_in_progress(
    git_dir: &Path,
    into_oid: ObjectId,
    other_oid: ObjectId,
    merge_msg: String,
) -> Result<()> {
    fs::write(
        git_dir.join("MERGE_HEAD"),
        format!("{}\n", other_oid.to_hex()),
    )
    .map_err(Error::Io)?;
    write_orig_head(git_dir, into_oid)?;
    fs::write(git_dir.join("MERGE_MSG"), merge_msg).map_err(Error::Io)?;
    Ok(())
}

fn strict_commit_identity(
    repo: &Repository,
    config: &ConfigSet,
    role: IdentRole,
) -> Result<String> {
    resolve_identity_with(repo.environment(), config, role, repo.identity_now())
        .map_err(|e| Error::Message(e.to_string()))
}

fn move_branch(
    repo: &Repository,
    refname: &str,
    old: ObjectId,
    new: ObjectId,
    reason: &str,
) -> Result<()> {
    update_checked_out_branch(repo, refname, Some(old), new, reason)
}

/// Refuse to touch the work tree when `branch_ref` no longer points at `expected` (`None`:
/// the branch must not exist), so a concurrent ref update is reported before anything changes.
pub(crate) fn ensure_branch_tip(
    repo: &Repository,
    branch_ref: &str,
    expected: Option<ObjectId>,
) -> Result<()> {
    let current = repo.refs().resolve(branch_ref).ok();
    refs::verify_branch_commit_cas(expected, current, branch_ref)
}

/// Point the checked-out branch `branch_ref` from `expected_old` (`None`: unborn) at `new`
/// with a compare-and-swap through the repository's ref store, then append branch and `HEAD`
/// reflog lines (per `core.logAllRefUpdates`) stamped with the repository's committer identity.
///
/// The reflog lines are appended rather than written in the ref transaction, because the
/// files store reloads and rewrites a ref's whole reflog when a transaction logs to it. Once
/// the ref has moved the operation has happened, so a failed append is reported as
/// [`Warning::ReflogNotWritten`] on the repository's diagnostics sink instead of an error.
///
/// # Errors
///
/// Config load and compare-and-swap failures, all before the ref changes.
pub(crate) fn update_checked_out_branch(
    repo: &Repository,
    branch_ref: &str,
    expected_old: Option<ObjectId>,
    new: ObjectId,
    reason: &str,
) -> Result<()> {
    let config = repo.config()?;
    let identity =
        resolve_loose_committer_identity_with(repo.environment(), &config, repo.identity_now());
    refs::commit_ref_update_on_store(
        repo.refs(),
        RefUpdate {
            name: branch_ref.to_owned(),
            new_value: Some(RawRef::Direct(new)),
            expected: expected_old.map_or(Expected::Missing, Expected::Oid),
            reflog: None,
            flags: RefUpdateFlags::default(),
        },
    )?;
    let old = expected_old.unwrap_or_else(crate::diff::zero_oid);
    for name in [branch_ref, "HEAD"] {
        let autocreate = refs::should_autocreate_reflog_with_config(&config, repo.git_dir(), name);
        if let Err(err) = refs::append_reflog_on_store(
            repo.refs(),
            name,
            &old,
            &new,
            &identity,
            reason,
            autocreate,
            &config,
        ) {
            repo.diagnostics().warn(Warning::ReflogNotWritten {
                refname: name.to_owned(),
                detail: err.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod integrate_tests {
    use std::collections::HashSet;

    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use std::collections::BTreeMap;

    fn index_entry(path: &str, stage: u8) -> IndexEntry {
        let mut entry = IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 0,
            oid: crate::diff::zero_oid(),
            flags: 0,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        };
        entry.set_stage(stage);
        entry
    }

    #[test]
    fn reduce_octopus_merge_heads_drops_ancestor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("f.txt"), "a\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "a"]);
        let a: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("a");
        std::fs::write(tmp.path().join("f.txt"), "b\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "b"]);
        let b: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("b");
        let (oids, names) =
            reduce_octopus_merge_heads(&repo, &[a, b], &["a".into(), "b".into()]).expect("reduce");
        assert_eq!(oids, vec![b]);
        assert_eq!(names, vec!["b".to_owned()]);
    }

    #[test]
    fn compose_fast_forward_index_keeps_staged_additions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("tracked.txt"), "v1\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "tracked.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "head"]);
        let head: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("head");
        let head_tree = repo.odb().commit_tree(&head).expect("head tree");
        std::fs::write(tmp.path().join("tracked.txt"), "v2\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "tracked.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "target"]);
        let target_tree = repo
            .odb()
            .commit_tree(
                &grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                    .trim()
                    .parse()
                    .expect("target"),
            )
            .expect("target tree");
        std::fs::write(tmp.path().join("new.txt"), "staged\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "new.txt"]);
        let index = repo.load_index().expect("index");
        let ff = compose_fast_forward_index(&repo, target_tree, head_tree, &index).expect("ff");
        assert!(ff.get(b"new.txt", 0).is_some());
        assert!(ff.get(b"tracked.txt", 0).is_some());
    }

    #[test]
    fn predict_merge_mem_overlay_guard_restores_outer_scope() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("f.txt"), "a\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "one"]);
        let head: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("head");
        grit_test_support::git(tmp.path(), &["branch", "topic"]);
        std::fs::write(tmp.path().join("f.txt"), "b\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "two"]);
        let tip: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("tip");
        grit_test_support::git(tmp.path(), &["checkout", "main"]);

        repo.odb().enable_mem_overlay();
        assert!(repo.odb().is_mem_overlay_active());
        let _ =
            predict_merge(&repo, head, tip, &MergePredictionOptions::default()).expect("predict");
        assert!(
            repo.odb().is_mem_overlay_active(),
            "outer mem overlay must stay active after predict_merge"
        );
        repo.odb().disable_mem_overlay();
    }

    #[test]
    fn integrate_branch_fast_forward_and_up_to_date() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("f.txt"), "v1\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "one"]);
        let head: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("head");
        grit_test_support::git(tmp.path(), &["checkout", "-q", "-b", "topic"]);
        std::fs::write(tmp.path().join("f.txt"), "v2\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "two"]);
        let tip: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("tip");
        grit_test_support::git(tmp.path(), &["checkout", "main"]);
        let up = integrate_branch(&repo, "refs/heads/main", head, head, "topic").expect("up");
        assert_eq!(up, IntegrateBranchOutcome::UpToDate);
        let ff = integrate_branch(&repo, "refs/heads/main", head, tip, "topic").expect("ff");
        assert!(matches!(ff, IntegrateBranchOutcome::FastForward { .. }));
    }

    #[test]
    fn compose_octopus_final_index_carries_staged_paths() {
        let mut pre = Index::new();
        pre.add_or_replace(index_entry("staged.txt", 0));
        let mut final_index = Index::new();
        final_index.add_or_replace(index_entry("merged.txt", 0));
        compose_octopus_final_index(&pre, &mut final_index);
        assert!(final_index.get(b"staged.txt", 0).is_some());
        assert!(final_index.get(b"merged.txt", 0).is_some());
    }

    #[test]
    fn tree_to_index_entries_and_map_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("a.txt"), "a\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "a.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "c"]);
        let tree: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD^{tree}"])
            .trim()
            .parse()
            .expect("tree");
        let entries = tree_to_index_entries(&repo, &tree, "").expect("entries");
        let map = tree_to_map(entries.clone());
        assert_eq!(map.len(), entries.len());
    }

    #[test]
    fn tree_merge_conflict_paths_unions_markers_and_unmerged_index() {
        let mut index = Index::new();
        index.add_or_replace(index_entry("b.txt", 1));
        index.add_or_replace(index_entry("b.txt", 2));
        let mut conflict_content = BTreeMap::new();
        conflict_content.insert(b"a.txt".to_vec(), crate::diff::zero_oid());
        let merged = TreeMergeOutput {
            index,
            conflict_content,
            result_tree: crate::diff::zero_oid(),
            touched_paths: HashSet::new(),
        };
        let paths: Vec<String> = conflicts_from_tree_merge(&merged)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(paths, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
        assert!(tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn integrate_branch_merged_without_conflicts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        grit_test_support::git(tmp.path(), &["config", "user.name", "Test"]);
        grit_test_support::git(tmp.path(), &["config", "user.email", "t@example.com"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("shared.txt"), "base\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "shared.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        grit_test_support::git(tmp.path(), &["switch", "-c", "topic"]);
        std::fs::write(tmp.path().join("shared.txt"), "topic\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "shared.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "topic"]);
        let topic: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "topic"])
            .trim()
            .parse()
            .expect("topic");
        grit_test_support::git(tmp.path(), &["checkout", "main"]);
        std::fs::write(tmp.path().join("main-only.txt"), "main\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "main-only.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "main"]);
        let main: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("main");
        let outcome =
            integrate_branch(&repo, "refs/heads/main", main, topic, "topic").expect("merge");
        let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
            panic!("expected clean merge, got {outcome:?}");
        };
        let tip: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "refs/heads/main"])
            .trim()
            .parse()
            .expect("tip");
        assert_eq!(tip, oid);
        grit_test_support::git(tmp.path(), &["fsck", "--strict"]);
    }

    #[test]
    fn integrate_branch_conflicts_then_abort() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("f.txt"), "base\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        grit_test_support::git(tmp.path(), &["switch", "-c", "topic"]);
        std::fs::write(tmp.path().join("f.txt"), "theirs\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "theirs"]);
        let topic: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "topic"])
            .trim()
            .parse()
            .expect("topic");
        grit_test_support::git(tmp.path(), &["checkout", "main"]);
        std::fs::write(tmp.path().join("f.txt"), "ours\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "ours"]);
        let main: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("main");
        let outcome =
            integrate_branch(&repo, "refs/heads/main", main, topic, "topic").expect("merge");
        let IntegrateBranchOutcome::Conflicts { paths, .. } = outcome else {
            panic!("expected conflicts, got {outcome:?}");
        };
        assert!(paths.iter().any(|p| p == "f.txt"));
        assert!(tmp.path().join(".git/MERGE_HEAD").exists());
        abort_branch_merge(&repo).expect("abort");
        assert!(!tmp.path().join(".git/MERGE_HEAD").exists());
        let tip: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("tip");
        assert_eq!(tip, main);
    }
}
