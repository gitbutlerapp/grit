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
//! - [`compose_fast_forward_index`] — build the post-fast-forward index from the
//!   target tree, carrying forward staged additions that the fast-forward does
//!   not touch.
//! - [`tree_to_index_entries`] / [`tree_to_map`] — flatten a tree object into a
//!   recursive list (or path-keyed map) of [`IndexEntry`] values; the shared
//!   building block the transform above (and most of the CLI merge code)
//!   is written against.
//! - [`resolve_merge_source`] — turn a merge argument (branch, tag, object id or any
//!   revision expression) into the commit to merge.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::change_id::{change_id, ChangeId, ChangeIdPolicy};
use crate::config::ConfigSet;
use crate::diagnostics::Warning;
use crate::error::{Error, OverwriteReason, Result};
use crate::hooks::{run_post_merge, PostOperationHooks};
use crate::ident::{parse_signature_times, Signature};
use crate::ident_resolve::{resolve_loose_committer_identity_with, IdentRole};
use crate::index::{Index, IndexEntry};
use crate::merge_base::{
    is_ancestor, merge_bases_all, merge_bases_all_vs_union,
    walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use crate::merge_file::{ConflictStyle, MergeFavor};
use crate::merge_trees::{
    materialized_merge_result_tree, merge_trees_at_depth, merge_trees_three_way,
    DirectoryRenameMode, MergeTreeCache, TheirsConflictLabel, TreeMergeConflictPresentation,
    TreeMergeOutput, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind};
use crate::operation::{conflicts_from_tree_merge, tree_merge_has_conflicts, Conflict};
use crate::path_population::validate_tree_walk_entry;
use crate::path_validate::ProtectOptions;
use crate::porcelain::checkout;
use crate::porcelain::commit::{
    normalize_commit_message, stamp_change_id_on_new_commit, write_commit_object,
};
use crate::porcelain::restore::conflict_style_from_config;
use crate::porcelain::switch::{
    checkout_prepared_tree_transition_for_merge, prepare_tree_transition_allow_local,
    verify_tree_transition_allow_local, TreeTransitionPrepareError, TreeTransitionVerifyError,
};
use crate::porcelain::tree_merge_checkout::{
    apply_conflict_tree_merge_to_worktree, finalize_clean_tree_merge_index,
    write_index_after_merge_checkout,
};
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::progress::{poll_cancel, Cancel, ProgressSink};
use crate::refs;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
use crate::refs::update_detached_head_for_commit_on_store;
use crate::repo::Repository;
use crate::rev_parse_error::RevParseError;
use crate::state::{
    clear_merge_state, read_merge_heads, read_merge_msg, read_orig_head_for_repository,
    write_orig_head,
};
use crate::stripspace::{process as stripspace_process, Mode as StripspaceMode};
use thiserror::Error;

use crate::hooks::{run_commit_hook, CommitHookEnv, HookName};
use crate::porcelain::add::stage_for_commit_on_index;
use crate::porcelain::commit::{
    create_commit_with_loaded_index, default_commit_scope_from_config, run_commit_message_hooks,
    CommitRequest, CommitScope, HookPolicy, MessageSource,
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

        if crate::index::is_tree_mode(te.mode) {
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
                mode: crate::index::canon_mode(te.mode),
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
    /// Whether to run `post-merge` after a fast-forward, merge commit or squash.
    pub hooks: PostOperationHooks,
    /// Whether `pre-merge-commit` and `commit-msg` run before a merge commit is recorded
    /// ([`HookPolicy::SkipVerify`] is `--no-verify`). `prepare-commit-msg` runs either way, and
    /// `post-merge` follows [`Self::hooks`].
    pub verify: HookPolicy,
    /// Whether files the merged branch added inside a directory the current branch renamed
    /// (or the reverse) follow the rename; when unset, `merge.directoryRenames` decides
    /// ([`DirectoryRenameMode::from_repo`]).
    pub directory_renames: Option<DirectoryRenameMode>,
}

/// Resolve conflict marker style: explicit option, then `merge.conflictStyle`, then `merge`.
#[must_use]
pub fn resolve_merge_conflict_style(
    repo: &Repository,
    override_style: Option<ConflictStyle>,
) -> ConflictStyle {
    override_style.unwrap_or_else(|| conflict_style_from_config(repo))
}

/// Directory rename handling: explicit option, then `merge.directoryRenames`, then
/// [`DirectoryRenameMode::Conflict`].
fn resolve_directory_renames(
    repo: &Repository,
    override_mode: Option<DirectoryRenameMode>,
) -> Result<DirectoryRenameMode> {
    override_mode.map_or_else(|| DirectoryRenameMode::from_repo(repo), Ok)
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

/// Options for [`predict_merge`] and [`write_merge_tree`].
#[derive(Clone, Debug)]
pub struct MergePredictionOptions {
    /// When `false`, a fast-forward is not considered even when `other_oid` is a
    /// descendant of `into_oid`; a three-way merge is simulated instead. Ignored by
    /// [`write_merge_tree`], which always merges.
    pub allow_fast_forward: bool,
    /// When `true`, [`write_merge_tree`] merges commits with no merge base over the empty
    /// tree, like Git's `--allow-unrelated-histories`; when `false` it fails with
    /// [`Error::UnrelatedHistories`]. [`predict_merge`] always reports
    /// [`MergePrediction::Unrelated`] for such commits.
    pub allow_unrelated_histories: bool,
    /// Merge-favor policy for the simulated three-way merge (ours/theirs/normal).
    pub favor: MergeFavor,
    /// Whitespace handling for the simulated three-way merge.
    pub whitespace: WhitespaceMergeOptions,
    /// Label for the current side in conflict markers (`<<<<<<< HEAD` by default).
    pub ours_label: String,
    /// Label for the incoming side in conflict-marker metadata (presentation only).
    pub theirs_label: String,
    /// Whether files one side added inside a directory the other side renamed follow the
    /// rename; when unset, `merge.directoryRenames` decides ([`DirectoryRenameMode::from_repo`]).
    pub directory_renames: Option<DirectoryRenameMode>,
}

impl Default for MergePredictionOptions {
    fn default() -> Self {
        Self {
            allow_fast_forward: true,
            allow_unrelated_histories: false,
            favor: MergeFavor::default(),
            whitespace: WhitespaceMergeOptions::default(),
            ours_label: "HEAD".into(),
            theirs_label: "branch".into(),
            directory_renames: None,
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
        /// Merged tree OID: the same id `git merge-tree --write-tree` prints.
        ///
        /// The tree is **not** in the object database after [`predict_merge`] returns (its new
        /// trees and blobs were only held in memory), so it cannot be committed, diffed or
        /// read. Use [`write_merge_tree`] when the merged tree must be stored.
        tree: ObjectId,
    },
    /// A three-way merge would stop with conflicts.
    Conflicts {
        /// Conflicted paths and kinds (same classification as a real merge).
        conflicts: Vec<Conflict>,
    },
    /// The two commits share no common history.
    Unrelated,
}

/// Predict merging `other_oid` into `into_oid` without touching refs, the index,
/// merge state files, or the working tree.
///
/// Object writes from the merge are routed through the ODB in-memory overlay and
/// discarded before return, so the on-disk object store is unchanged. That includes the
/// tree reported by [`MergePrediction::Clean`]; call [`write_merge_tree`] to store it.
/// With several merge bases the prediction merges them into a virtual base first, exactly
/// as [`write_merge_tree`] and [`integrate_branch`] do, so they cannot disagree.
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

    let _overlay = MemOverlayGuard::enter(repo.odb());
    let Some(base) = merge_base_for_commits(repo, into_oid, other_oid, options.whitespace, None)?
    else {
        return Ok(MergePrediction::Unrelated);
    };
    let merged = merge_commit_trees(repo, &base, into_oid, other_oid, options)?;
    if tree_merge_has_conflicts(&merged) {
        return Ok(MergePrediction::Conflicts {
            conflicts: conflicts_from_tree_merge(&merged),
        });
    }
    let tree = materialized_merge_result_tree(repo.odb(), &merged)?;
    Ok(MergePrediction::Clean { tree })
}

/// One unmerged index entry of a [`WrittenMergeTree`]: the `<mode> <oid> <stage> <path>`
/// rows `git merge-tree --write-tree` prints after the tree id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTreeStage {
    /// Git file mode of this side (`0o100644`, `0o100755`, `0o120000`, `0o160000`).
    pub mode: u32,
    /// Blob (or gitlink commit) of this side; already in the object database.
    pub oid: ObjectId,
    /// `1` for the merge base, `2` for ours (`into`), `3` for theirs (`other`).
    pub stage: u8,
    /// Repository-relative path as raw bytes.
    pub path: Vec<u8>,
}

/// A three-way merge result stored in the object database ([`write_merge_tree`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenMergeTree {
    /// Merged tree, present in the object database. Conflicted files hold conflict-marker
    /// content, exactly like the tree `git merge-tree --write-tree` writes.
    pub tree: ObjectId,
    /// Conflicted paths and kinds; empty when the merge is clean.
    pub conflicts: Vec<Conflict>,
    /// Unmerged entries (stages 1–3) sorted by path, then stage; empty when the merge is clean.
    pub stages: Vec<MergeTreeStage>,
}

impl WrittenMergeTree {
    /// Whether the merge finished without conflicts, so [`Self::tree`] can be committed as is.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty() && self.stages.is_empty()
    }
}

/// Three-way merge `other_oid` into `into_oid` and store the resulting tree, without touching
/// refs, the index, merge state files or the working tree (the library form of
/// `git merge-tree --write-tree`).
///
/// Works in bare repositories. Unlike [`predict_merge`], every new tree and blob the merge
/// creates is written to the object database before return, so a clean result can be
/// committed directly with `NewCommit::new(tree, ..).with_parents(vec![into_oid, other_oid])`.
/// On conflicts the tree is still written, with conflict markers in the conflicted files, and
/// [`WrittenMergeTree::stages`] lists the unmerged entries.
///
/// A merge is always computed: [`MergePredictionOptions::allow_fast_forward`] is ignored, and
/// when one commit contains the other the result is the newer commit's tree. With several merge
/// bases (criss-cross history) they are merged into one virtual base first, as in
/// [`predict_merge`] and `git merge-tree`; stage 1 entries then come from that virtual base and
/// its blobs and trees are stored too. When `options`
/// allows unrelated histories, a missing merge base is treated as the empty tree. If the
/// repository handle already has an in-memory write overlay active, the new objects stay in
/// that outer overlay instead of being written to disk.
///
/// # Parameters
///
/// - `repo`: repository to merge in.
/// - `into_oid`: commit merged into (stage 2, "ours").
/// - `other_oid`: commit merged in (stage 3, "theirs").
/// - `options`: favor, whitespace, conflict labels and unrelated-history policy.
///
/// # Errors
///
/// - [`Error::UnrelatedHistories`] when the commits share no merge base and
///   [`MergePredictionOptions::allow_unrelated_histories`] is `false`.
/// - Object read, merge and object write failures are propagated.
pub fn write_merge_tree(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    options: &MergePredictionOptions,
) -> Result<WrittenMergeTree> {
    let base = match merge_base_for_commits(repo, into_oid, other_oid, options.whitespace, None)? {
        Some(base) => base,
        None if options.allow_unrelated_histories => MergeBase::empty(repo)?,
        None => {
            return Err(Error::UnrelatedHistories {
                ours: into_oid,
                theirs: other_oid,
            })
        }
    };

    let merged = merge_commit_trees(repo, &base, into_oid, other_oid, options)?;
    let tree = materialized_merge_result_tree(repo.odb(), &merged)?;

    let stages = merged
        .index
        .entries()
        .iter()
        .filter(|entry| entry.stage() != 0)
        .map(|entry| MergeTreeStage {
            mode: entry.mode,
            oid: entry.oid,
            stage: entry.stage(),
            path: entry.path.clone(),
        })
        .collect();
    Ok(WrittenMergeTree {
        tree,
        conflicts: conflicts_from_tree_merge(&merged),
        stages,
    })
}

/// Three-way merge the trees of `into_oid` and `other_oid` over `base`, labelling conflicts
/// from `options`. New objects go wherever the ODB currently writes (disk, or an active overlay).
fn merge_commit_trees(
    repo: &Repository,
    base: &MergeBase,
    into_oid: ObjectId,
    other_oid: ObjectId,
    options: &MergePredictionOptions,
) -> Result<TreeMergeOutput> {
    let into_tree = repo.odb().commit_tree(&into_oid)?;
    let other_tree = repo.odb().commit_tree(&other_oid)?;
    let presentation = TreeMergeConflictPresentation {
        label_ours: &options.ours_label,
        label_theirs: crate::merge_trees::TheirsConflictLabel::Fixed(&options.theirs_label),
        label_base: &base.label,
        df_directory_ours_commit: Some(into_oid),
        df_directory_theirs_commit: Some(other_oid),
        ..TreeMergeConflictPresentation::default()
    };
    merge_trees_three_way(
        repo,
        base.tree,
        into_tree,
        other_tree,
        options.favor,
        options.whitespace,
        None,
        presentation,
        resolve_directory_renames(repo, options.directory_renames)?,
    )
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
        conflicts: Vec<Conflict>,
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
        directory_renames: options.directory_renames,
        ..MergePredictionOptions::default()
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

/// What kind of name a merge source was given by ([`resolve_merge_source`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeSourceKind {
    /// A local branch (`refs/heads/<name>`).
    LocalBranch,
    /// A remote-tracking branch (`refs/remotes/<name>`).
    RemoteBranch,
    /// A tag (`refs/tags/<name>`), lightweight or annotated.
    Tag,
    /// Any other revision expression: an object id, `HEAD~2`, `ORIG_HEAD`, `v1.0^`, a full
    /// refname, and so on.
    Commit,
}

/// The commit a merge brings in, and how its name was resolved ([`resolve_merge_source`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeSource {
    /// The commit to merge (annotated tags already peeled).
    pub oid: ObjectId,
    /// Which namespace matched the spec.
    pub kind: MergeSourceKind,
}

/// Resolve the argument of a merge (`spec`) to the commit it names.
///
/// Branch names win over other meanings: `refs/heads/<spec>` is tried first, then
/// `refs/remotes/<spec>`, then `refs/tags/<spec>`. Anything else goes through the revision
/// parser, so object ids (full or abbreviated), `ORIG_HEAD`, `topic~3` and `origin/main^2`
/// all work. Annotated tags are peeled to the commit they name. Range expressions (`a..b`, `a...b`)
/// do not name a single commit and are reported as not found. Ref names can contain literal
/// braces: `foo{bar` is a valid source, while `foo{bar...main` is still a range.
/// Dots within selectors such as `main^{/release..candidate}` do not form a range.
///
/// # Parameters
///
/// - `repo`: repository to resolve in.
/// - `spec`: the merge argument as the user wrote it.
///
/// # Returns
///
/// The commit and the [`MergeSourceKind`] of the name that matched.
///
/// # Errors
///
/// - [`Error::RevisionNotFound`] when `spec` names nothing.
/// - [`Error::UnexpectedObjectKind`] when `spec` names (or a tag peels to) a tree or blob.
/// - Object database errors (missing or corrupt objects behind a ref) are propagated.
pub fn resolve_merge_source(repo: &Repository, spec: &str) -> Result<MergeSource> {
    let named = [
        ("refs/heads/", MergeSourceKind::LocalBranch),
        ("refs/remotes/", MergeSourceKind::RemoteBranch),
        ("refs/tags/", MergeSourceKind::Tag),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| {
        refs::resolve_ref_in(repo, &format!("{prefix}{spec}"))
            .ok()
            .map(|oid| (oid, kind))
    });
    // A full-length hex id parses without an object lookup, so a missing object only shows up
    // when peeling; that is still "names nothing", unlike a ref pointing at a missing object.
    let not_found = |err: Error| match err {
        Error::ObjectNotFound(_)
        | Error::RevParse(
            RevParseError::InvalidObjectName { .. }
            | RevParseError::RevisionOutOfRange { .. }
            | RevParseError::AmbiguousArgument { .. }
            | RevParseError::NoSuchBranch { .. },
        ) => Error::RevisionNotFound(spec.to_owned()),
        other => other,
    };
    let (peeled, actual, kind) = match named {
        Some((oid, kind)) => {
            let (peeled, actual) = repo.odb().peel_tags(oid)?;
            (peeled, actual, kind)
        }
        None => {
            // The revision parser resolves `a...b` to a merge base; a range never names the
            // commit to merge.
            if crate::rev_parse::is_top_level_range(spec) {
                return Err(Error::RevisionNotFound(spec.to_owned()));
            }
            let oid = crate::rev_parse::resolve_revision_without_index_dwim(repo, spec)
                .map_err(not_found)?;
            let (peeled, actual) = repo.odb().peel_tags(oid).map_err(not_found)?;
            (peeled, actual, MergeSourceKind::Commit)
        }
    };
    if actual != ObjectKind::Commit {
        return Err(Error::UnexpectedObjectKind {
            oid: peeled,
            expected: ObjectKind::Commit,
            actual,
        });
    }
    Ok(MergeSource { oid: peeled, kind })
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

/// The tree a merge of two commits uses as its base, and the name conflict markers give it.
pub(crate) struct MergeBase {
    /// Base tree: the merge base's tree, or a virtual tree when there are several merge bases.
    pub(crate) tree: ObjectId,
    /// Ancestor label: the abbreviated merge base, `merged common ancestors` or `empty tree`.
    pub(crate) label: Cow<'static, str>,
}

impl MergeBase {
    /// The empty tree, used when histories are unrelated.
    fn empty(repo: &Repository) -> Result<Self> {
        Ok(Self {
            tree: empty_tree_oid(repo)?,
            label: Cow::Borrowed("empty tree"),
        })
    }
}

/// Base for merging the commits `ours` and `theirs`, or `None` when they share no history.
///
/// One merge base is used as is. Several (criss-cross history) are merged into one virtual
/// base the way Git's recursive strategies do, so the outcome does not depend on which of the
/// equally good bases is listed first: the bases are taken oldest first (the reverse of
/// [`merge_bases_all`]), and each next base is merged into the running result with the merge
/// bases of the two as its own (recursively virtual) base. Inner merges never apply the
/// caller's ours/theirs preference; their content conflicts stay in the virtual tree as
/// markers labelled `Temporary merge branch 1` / `Temporary merge branch 2`, in `style`
/// (read from config when `None`). Blobs and trees of the virtual base are written to the
/// object database like any merge result.
pub(crate) fn merge_base_for_commits(
    repo: &Repository,
    ours: ObjectId,
    theirs: ObjectId,
    ws: WhitespaceMergeOptions,
    style: Option<ConflictStyle>,
) -> Result<Option<MergeBase>> {
    let mut bases = merge_bases_all(repo, &[ours, theirs])?;
    match bases.as_slice() {
        [] => return Ok(None),
        [single] => return single_merge_base(repo, *single).map(Some),
        _ => {}
    }
    bases.reverse();
    let style = style.unwrap_or_else(|| conflict_style_from_config(repo));
    merge_bases_into_one(repo, &bases, 1, ws, style).map(Some)
}

fn single_merge_base(repo: &Repository, base: ObjectId) -> Result<MergeBase> {
    Ok(MergeBase {
        tree: repo.odb().commit_tree(&base)?,
        label: Cow::Owned(crate::commit_pretty::abbrev_hex(&base, 7)),
    })
}

/// Fold `bases` (oldest first) into one base tree; the merges run at `call_depth`.
fn merge_bases_into_one(
    repo: &Repository,
    bases: &[ObjectId],
    call_depth: usize,
    ws: WhitespaceMergeOptions,
    style: ConflictStyle,
) -> Result<MergeBase> {
    let Some((&first, rest)) = bases.split_first() else {
        return MergeBase::empty(repo);
    };
    if rest.is_empty() {
        return single_merge_base(repo, first);
    }
    let mut tree = repo.odb().commit_tree(&first)?;
    // The virtual commit's history is the union of the histories of every base folded so far.
    let mut folded = vec![first];
    for &next in rest {
        let mut inner_bases = merge_bases_all_vs_union(repo, next, &folded)?;
        inner_bases.reverse();
        let inner_base = merge_bases_into_one(repo, &inner_bases, call_depth + 1, ws, style)?;
        let presentation = TreeMergeConflictPresentation {
            label_ours: "Temporary merge branch 1",
            label_theirs: TheirsConflictLabel::Fixed("Temporary merge branch 2"),
            label_base: &inner_base.label,
            style,
            ..TreeMergeConflictPresentation::default()
        };
        let merged = merge_trees_at_depth(
            repo,
            &mut MergeTreeCache::default(),
            inner_base.tree,
            tree,
            repo.odb().commit_tree(&next)?,
            MergeFavor::None,
            ws,
            None,
            presentation,
            DirectoryRenameMode::Off,
            call_depth,
            None,
        )?;
        tree = materialized_merge_result_tree(repo.odb(), &merged)?;
        folded.push(next);
    }
    Ok(MergeBase {
        tree,
        label: Cow::Borrowed("merged common ancestors"),
    })
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
        details: Vec<Conflict>,
    },
    /// A `--squash` merge applied the merged tree and wrote `SQUASH_MSG`.
    Squashed,
    /// A `--no-commit` merge applied the merged tree and recorded merge state.
    Stopped,
}

/// Integrate `other_oid` into the branch `into_ref` (currently at `into_oid`).
///
/// `into_ref` is the branch being merged into: either a full ref name (for example
/// `refs/heads/main`) or a short local branch name (for example `main` or
/// `feature/nested`, stored as `refs/heads/{into_ref}`). Short names that do not resolve
/// return [`Error::BranchNotFound`].
///
/// `label` names the other side in merge messages and reflogs: the default merge commit
/// subject is `Merge {label}` (see [`MergeOptions::message`]), and branch reflog lines
/// use `merge {label}` or `merge {label}: fast-forward` on fast-forward.
///
/// On success without conflicts, updates the branch ref, index, and worktree. On
/// conflicts, records `MERGE_HEAD` / `ORIG_HEAD` / `MERGE_MSG`, writes conflict
/// markers and a partially merged index, and leaves the branch ref unchanged.
///
/// When the commits have several merge bases (criss-cross history), the bases are first
/// merged into one virtual base, like Git's recursive strategies; stage 1 of a conflicted
/// path then holds the virtual base's version.
///
/// The work-tree update reports [`crate::progress::ProgressPhase::CheckingOutFiles`] to
/// `progress`. `cancel` (or [`ProgressSink::should_cancel`]) is checked before the work tree is
/// touched and throughout the checkout; once the checkout completed, the index, merge state and
/// branch update always follow, so a cancel never leaves the branch behind a finished checkout.
///
/// # Errors
///
/// Propagates merge, checkout, ref, and I/O failures. Returns [`Error::WouldOverwrite`]
/// when local or untracked paths would be overwritten by the merge tree, and
/// [`Error::DirtyWorktree`] when the worktree is not clean but no specific paths were
/// collected. Returns [`Error::Message`] when the histories are unrelated. Returns
/// [`Error::RefStore`] with [`crate::refs::store::RefStoreError::ExpectedMismatch`] when
/// the canonical branch ref no longer points at `into_oid` (checked before the worktree
/// is touched, and again when the ref moves).
/// Returns [`Error::Cancelled`] when cancelled; the branch, `HEAD`, the index and the merge
/// state are unchanged, and a cancel during the checkout rolls the work tree back.
pub fn integrate_branch(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<IntegrateBranchOutcome> {
    integrate_branch_with(
        repo,
        into_ref,
        into_oid,
        other_oid,
        label,
        &MergeOptions::default(),
        progress,
        cancel,
    )
}

/// Integrate `other_oid` into the branch at `into_ref` with explicit merge options.
///
/// `into_ref` accepts a full ref name or a short local branch name; see [`integrate_branch`].
/// `label` is used for merge messages and reflogs as documented on [`integrate_branch`].
///
/// Before a clean merge records its commit, runs `pre-merge-commit`, then
/// `prepare-commit-msg .git/MERGE_MSG merge` and `commit-msg .git/MERGE_MSG` with `MERGE_HEAD`
/// recorded, as Git does ([`MergeOptions::verify`] skips `pre-merge-commit` and `commit-msg`).
/// A message hook may rewrite the message, and `pre-merge-commit` may stage changes that the
/// commit then includes. None of these run for a fast-forward, a squash, conflicts or
/// [`MergeCommitMode::NoCommit`].
///
/// After a fast-forward or merge commit, runs `post-merge 0`; after a squash, `post-merge 1`
/// (unless [`MergeOptions::hooks`] is [`PostOperationHooks::Skip`]). Like Git, the hook does not
/// run when the branch is already up to date, when the merge stops on conflicts, or for
/// [`MergeCommitMode::NoCommit`]. Its outcome never fails the merge.
///
/// # Errors
///
/// Same as [`integrate_branch`], plus [`Error::NotFastForward`] when
/// [`FastForwardPolicy::Only`] cannot fast-forward, and [`Error::Hook`] when
/// `pre-merge-commit`, `prepare-commit-msg` or `commit-msg` refuses. A refused merge leaves the
/// merged index and work tree with `MERGE_HEAD`, `ORIG_HEAD` and `MERGE_MSG` recorded and the
/// branch unchanged: finish it with a commit ([`continue_merge`]) or [`abort_branch_merge`].
#[allow(clippy::too_many_arguments)]
pub fn integrate_branch_with(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<IntegrateBranchOutcome> {
    let outcome = integrate_branch_without_hooks(
        repo, into_ref, into_oid, other_oid, label, options, progress, cancel,
    )?;
    if options.hooks.enabled() {
        match outcome {
            IntegrateBranchOutcome::FastForward { .. } | IntegrateBranchOutcome::Merged { .. } => {
                run_post_merge(repo, false);
            }
            IntegrateBranchOutcome::Squashed => {
                run_post_merge(repo, true);
            }
            IntegrateBranchOutcome::UpToDate
            | IntegrateBranchOutcome::Conflicts { .. }
            | IntegrateBranchOutcome::Stopped => {}
        }
    }
    // Like Git, `post-merge` still sees the merge state the message hooks saw.
    if matches!(outcome, IntegrateBranchOutcome::Merged { .. }) {
        clear_merge_state(repo.git_dir())?;
    }
    Ok(outcome)
}

/// Map `into_ref` to the ref updated by integrate: full `refs/…` names are kept;
/// otherwise treat as a short local branch under `refs/heads/`.
fn canonical_integrate_into_ref(repo: &Repository, into_ref: &str) -> Result<String> {
    if into_ref.starts_with("refs/") {
        return Ok(into_ref.to_owned());
    }
    let canonical = format!("refs/heads/{into_ref}");
    if repo.refs().resolve(&canonical).is_err() {
        return Err(Error::BranchNotFound {
            branch: into_ref.to_owned(),
        });
    }
    Ok(canonical)
}

/// Where a merge updates the repository tip after a fast-forward or merge commit.
enum IntegrateTarget {
    Branch(String),
    DetachedHead,
}

/// Integrate `other_oid` into detached `HEAD` (currently at `into_oid`).
///
/// Same outcomes, options, hooks, and conflict handling as [`integrate_branch`], but only
/// `HEAD` moves on success; no branch ref is updated.
///
/// # Errors
///
/// Same as [`integrate_branch_with`], except branch-not-found and symbolic-HEAD cases do
/// not apply when `into_oid` matches detached `HEAD`.
pub fn integrate_detached_head(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<IntegrateBranchOutcome> {
    integrate_detached_head_with(
        repo,
        into_oid,
        other_oid,
        label,
        &MergeOptions::default(),
        progress,
        cancel,
    )
}

/// Integrate `other_oid` into detached `HEAD` at `into_oid` with explicit merge options.
///
/// See [`integrate_detached_head`] and [`integrate_branch_with`].
#[allow(clippy::too_many_arguments)]
pub fn integrate_detached_head_with(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<IntegrateBranchOutcome> {
    let outcome = integrate_at_tip_without_hooks(
        repo,
        IntegrateTarget::DetachedHead,
        into_oid,
        other_oid,
        label,
        options,
        progress,
        cancel,
    )?;
    if options.hooks.enabled() {
        match outcome {
            IntegrateBranchOutcome::FastForward { .. } | IntegrateBranchOutcome::Merged { .. } => {
                run_post_merge(repo, false);
            }
            IntegrateBranchOutcome::Squashed => {
                run_post_merge(repo, true);
            }
            IntegrateBranchOutcome::UpToDate
            | IntegrateBranchOutcome::Conflicts { .. }
            | IntegrateBranchOutcome::Stopped => {}
        }
    }
    if matches!(outcome, IntegrateBranchOutcome::Merged { .. }) {
        clear_merge_state(repo.git_dir())?;
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn integrate_branch_without_hooks(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<IntegrateBranchOutcome> {
    let into_ref = canonical_integrate_into_ref(repo, into_ref)?;
    integrate_at_tip_without_hooks(
        repo,
        IntegrateTarget::Branch(into_ref),
        into_oid,
        other_oid,
        label,
        options,
        progress,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn integrate_at_tip_without_hooks(
    repo: &Repository,
    target: IntegrateTarget,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<IntegrateBranchOutcome> {
    if into_oid == other_oid || is_ancestor(repo, other_oid, into_oid)? {
        write_orig_head(repo, into_oid)?;
        return Ok(IntegrateBranchOutcome::UpToDate);
    }
    match &target {
        IntegrateTarget::Branch(into_ref) => ensure_branch_tip(repo, into_ref, Some(into_oid))?,
        IntegrateTarget::DetachedHead => ensure_detached_head_tip(repo, into_oid)?,
    };
    poll_cancel(progress, cancel)?;
    // Like Git, every merge attempt past this point records where it started, including a
    // refused fast-forward-only merge and one that stops on conflicts.
    write_orig_head(repo, into_oid)?;

    let into_tree = repo.odb().commit_tree(&into_oid)?;
    let other_tree = repo.odb().commit_tree(&other_oid)?;
    let ff_possible = is_ancestor(repo, into_oid, other_oid)?;

    if matches!(options.fast_forward, FastForwardPolicy::Only) && !ff_possible {
        return Err(Error::NotFastForward);
    }

    if should_take_fast_forward(options, ff_possible) {
        if matches!(options.commit, MergeCommitMode::Squash) {
            merge_checkout_tree_transition(repo, Some(&into_tree), &other_tree, progress, cancel)?;
            let mut snapshot = crate::porcelain::worktree_guard::load_worktree_snapshot(repo)?;
            let ff_index =
                compose_fast_forward_index(repo, other_tree, into_tree, &snapshot.index)?;
            snapshot.index = ff_index;
            repo.write_index(&mut snapshot.index)?;
            write_squash_msg_file(repo, into_oid, other_oid)?;
            return Ok(IntegrateBranchOutcome::Squashed);
        }

        merge_checkout_tree_transition(repo, Some(&into_tree), &other_tree, progress, cancel)?;
        advance_integrate_tip(
            repo,
            &target,
            into_oid,
            other_oid,
            &format!("merge {label}: fast-forward"),
        )?;
        return Ok(IntegrateBranchOutcome::FastForward {
            oid: other_oid,
            change_id: stored_change_id_for_commit(repo, other_oid)?,
        });
    }

    let style = resolve_merge_conflict_style(repo, options.conflict_style);
    let ws = WhitespaceMergeOptions::default();
    let base = match merge_base_for_commits(repo, into_oid, other_oid, ws, Some(style))? {
        Some(base) => base,
        None if options.allow_unrelated => MergeBase::empty(repo)?,
        None => {
            return Err(Error::Message(format!(
                "'{label}' has no common history with the current branch"
            )));
        }
    };

    let presentation = TreeMergeConflictPresentation {
        label_theirs: crate::merge_trees::TheirsConflictLabel::Fixed(label),
        label_base: &base.label,
        df_directory_ours_commit: Some(into_oid),
        df_directory_theirs_commit: Some(other_oid),
        style,
        ..TreeMergeConflictPresentation::default()
    };
    let merged = merge_trees_three_way(
        repo,
        base.tree,
        into_tree,
        other_tree,
        options.favor,
        ws,
        None,
        presentation,
        resolve_directory_renames(repo, options.directory_renames)?,
    )?;
    poll_cancel(progress, cancel)?;

    if tree_merge_has_conflicts(&merged) {
        let details = conflicts_from_tree_merge(&merged);
        let paths: Vec<String> = details.iter().map(|c| c.path.clone()).collect();
        let materialized = materialized_merge_result_tree(repo.odb(), &merged)?;
        ensure_merge_tree_transition_allowed(repo, Some(&into_tree), &materialized)?;
        apply_conflict_merge_worktree(repo, &into_tree, &merged, &materialized, progress, cancel)?;
        let merge_msg = merge_message_text(label, options.message.as_deref());
        if matches!(options.commit, MergeCommitMode::Squash) {
            write_squash_msg_file(repo, into_oid, other_oid)?;
            write_squash_conflict_merge_msg(repo.git_dir(), &paths, options.message.as_deref())?;
        } else {
            record_merge_in_progress(repo.git_dir(), other_oid, merge_msg)?;
        }
        return Ok(IntegrateBranchOutcome::Conflicts { paths, details });
    }

    let commit_preflight = if matches!(options.commit, MergeCommitMode::Commit) {
        let config = repo.config()?;
        Some(MergeCommitPreflight {
            author: strict_commit_identity(repo, IdentRole::Author)?,
            committer: strict_commit_identity(repo, IdentRole::Committer)?,
            config,
        })
    } else {
        None
    };

    merge_checkout_tree_transition(
        repo,
        Some(&into_tree),
        &merged.result_tree,
        progress,
        cancel,
    )?;
    let mut lock = repo.lock_index()?;
    let mut snapshot = crate::porcelain::worktree_guard::load_worktree_snapshot(repo)?;
    finalize_clean_tree_merge_index(repo, &mut snapshot.index, &merged)?;
    write_index_after_merge_checkout(repo, &mut snapshot.index, &mut lock)?;

    match options.commit {
        MergeCommitMode::Squash => {
            write_squash_msg_file(repo, into_oid, other_oid)?;
            Ok(IntegrateBranchOutcome::Squashed)
        }
        MergeCommitMode::NoCommit => {
            record_merge_in_progress(
                repo.git_dir(),
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
            drop(lock);
            let preflight = commit_preflight
                .ok_or_else(|| Error::Message("merge commit preflight missing".into()))?;
            let config = preflight.config;
            let (tree, message) = run_merge_commit_hooks(
                repo,
                options.verify,
                other_oid,
                merged.result_tree,
                merge_message_text(label, options.message.as_deref()),
            )?;
            let mut commit = CommitData {
                tree,
                parents: vec![into_oid, other_oid],
                author: preflight.author,
                committer: preflight.committer,
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message,
                raw_message: None,
                extra_headers: Vec::new(),
            };
            let policy = ChangeIdPolicy::from_config(config.as_ref());
            let oid = stamp_change_id_on_new_commit(repo, &policy, &mut commit)
                .and_then(|()| write_commit_object(repo, &commit, None))
                .and_then(|oid| {
                    advance_integrate_tip(repo, &target, into_oid, oid, &format!("merge {label}"))
                        .map(|()| oid)
                })
                .inspect_err(|_| {
                    let _ = clear_merge_state(repo.git_dir());
                })?;
            Ok(IntegrateBranchOutcome::Merged {
                oid,
                change_id: change_id(&commit),
            })
        }
    }
}

/// Like [`integrate_branch`], with explicit fast-forward policy for [`pull`](crate::porcelain::pull).
///
/// `into_ref` and `label` follow [`integrate_branch`].
///
/// # Errors
///
/// Same as [`integrate_branch_with`].
#[allow(clippy::too_many_arguments)]
pub fn integrate_branch_with_options(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: IntegrateBranchOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<IntegrateBranchOutcome> {
    integrate_branch_with(
        repo,
        into_ref,
        into_oid,
        other_oid,
        label,
        &options.into(),
        progress,
        cancel,
    )
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
    /// Author of the merge commit; `None` uses the repository's author identity
    /// ([`Repository::identity`]).
    pub author: Option<Signature>,
    /// Committer of the merge commit; `None` uses the repository's committer identity.
    pub committer: Option<Signature>,
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
    #[error("you still have unmerged paths: {}", paths.join(", "))]
    UnmergedPaths {
        /// Repository-relative paths still in conflict stages.
        paths: Vec<String>,
    },
    /// Work tree content is resolved but the index still has conflict stages (`commit.autoStage=false`).
    #[error("resolved in the work tree but not staged: {}", paths.join(", "))]
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
/// the checked-out branch. Auto-staging reports [`crate::progress::ProgressPhase::StagingFiles`]
/// and polls `cancel`, with the same cancel points as [`crate::porcelain::commit::create_commit`].
///
/// # Errors
///
/// Returns [`ContinueMergeError::NoMergeInProgress`] when no merge is active,
/// [`ContinueMergeError::UnmergedPaths`] when conflict stages remain, or propagates
/// commit and I/O failures (including [`Error::Cancelled`]) as [`ContinueMergeError::Repository`].
pub fn continue_merge(
    repo: &Repository,
    options: &ContinueOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> std::result::Result<ContinueOutcome, ContinueMergeError> {
    let merge_heads = read_merge_heads(repo.git_dir())?;
    if merge_heads.is_empty() {
        return Err(ContinueMergeError::NoMergeInProgress);
    }

    let config = repo.config().map_err(ContinueMergeError::Repository)?;
    let auto_stage = default_commit_scope_from_config(config.as_ref()) == CommitScope::AutoStage;

    let mut index = repo.load_index().map_err(ContinueMergeError::Repository)?;
    if auto_stage {
        stage_for_commit_on_index(repo, &mut index, progress, cancel, false)
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
        message_from_merge: options.message.is_none(),
        author: options.author.clone(),
        committer: options.committer.clone(),
        allow_empty: false,
        ..Default::default()
    };
    let outcome = create_commit_with_loaded_index(repo, &mut index, &req, progress, cancel)
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

/// Abort an in-progress merge, restoring index and worktree to
/// [`read_orig_head_for_repository`].
///
/// A merge autostash recorded in `MERGE_AUTOSTASH` (left by a pull whose merge commit a hook
/// refused) is applied again afterwards, as `git merge --abort` does.
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

fn merge_abort_unmerged_paths(index: &Index) -> HashSet<Vec<u8>> {
    index
        .entries()
        .iter()
        .filter(|entry| entry.stage() != 0)
        .map(|entry| entry.path.clone())
        .collect()
}

/// Stage-0 rows for merge abort checkout diffing: omit paths that still have unmerged stages.
fn merge_abort_index_for_diff(index: &Index) -> Index {
    let unmerged = merge_abort_unmerged_paths(index);
    let mut diff_index = Index {
        version: index.version,
        hash_algo: index.hash_algo,
        source_mtime: index.source_mtime,
        ..Index::new()
    };
    for entry in index.entries() {
        if entry.stage() != 0 || unmerged.contains(&entry.path) {
            continue;
        }
        diff_index.push_entry_unsorted(entry.clone());
    }
    diff_index.sort();
    diff_index
}

fn merge_abort_append_unmerged_tree_checkouts(
    odb: &crate::odb::Odb,
    head_tree: &ObjectId,
    unmerged: &HashSet<Vec<u8>>,
    changes: &mut Vec<crate::diff::DiffEntry>,
) -> Result<()> {
    use crate::diff::{zero_oid, DiffEntry, DiffStatus};
    use crate::objects::FileMode;
    let mut seen: HashSet<String> = changes
        .iter()
        .filter_map(|change| change.new_path.clone())
        .collect();
    for path in unmerged {
        let Some(entry) = odb.tree_entry_at_path(head_tree, path)? else {
            continue;
        };
        let path_str = crate::repo_relpath::storage_string(path);
        if !seen.insert(path_str.clone()) {
            continue;
        }
        changes.push(DiffEntry {
            status: DiffStatus::Modified,
            old_path: Some(path_str.clone()),
            new_path: Some(path_str),
            old_mode: FileMode::new(entry.mode),
            new_mode: FileMode::new(entry.mode),
            old_oid: zero_oid(),
            new_oid: entry.oid,
            score: None,
        });
    }
    Ok(())
}

fn restore_pre_merge_state(repo: &Repository) -> Result<()> {
    let orig = read_orig_head_for_repository(repo)?
        .ok_or_else(|| Error::Message("merge in progress but ORIG_HEAD is missing".into()))?;
    let head_tree = repo.odb().commit_tree(&orig)?;

    let old_index = repo.load_index()?;
    // Paths only introduced by the conflicted merge (absent from the pre-merge tree) must be
    // removed after the hard reset realigns tracked paths to `head_tree`.
    let mut conflict_only: Vec<Vec<u8>> = old_index
        .entries()
        .iter()
        .filter(|e| {
            e.stage() != 0
                && repo
                    .odb()
                    .tree_entry_at_path(&head_tree, &e.path)
                    .ok()
                    .flatten()
                    .is_none()
        })
        .map(|e| e.path.clone())
        .collect();
    conflict_only.sort();
    conflict_only.dedup();

    let unmerged = merge_abort_unmerged_paths(&old_index);
    let diff_source = merge_abort_index_for_diff(&old_index);
    let mut changes =
        checkout::checkout_diff_align_index_to_tree(repo.odb(), &diff_source, &head_tree)?;
    merge_abort_append_unmerged_tree_checkouts(repo.odb(), &head_tree, &unmerged, &mut changes)?;
    let mut index = crate::porcelain::index_from_tree::index_from_tree(repo, &head_tree)?;
    if let Some(sparse) = crate::sparse_checkout::SparseCheckout::load(repo)? {
        sparse.carry_skip_worktree(&mut index, &old_index);
    }
    crate::porcelain::index_from_tree::prepare_index_for_tree_transition(
        repo, &head_tree, &mut index,
    )?;
    let mut lock = repo.lock_index()?;
    checkout::checkout_tree_changes_with_index_opts(
        repo,
        &changes,
        &mut index,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
        checkout::CheckoutApplyOptions {
            parallel_prepare_blobs: false,
            checkout_parallel_threshold: None,
            checkout_result_tree: Some(head_tree),
            ..checkout::CheckoutApplyOptions::default()
        },
        None,
        checkout::CheckoutIndexWrite::Commit(&mut lock),
    )?;
    if let Some(work_tree) = repo.work_tree() {
        let mut remove_paths = conflict_only;
        for entry in old_index.entries() {
            if entry.stage() != 0 {
                continue;
            }
            if repo
                .odb()
                .tree_entry_at_path(&head_tree, &entry.path)
                .ok()
                .flatten()
                .is_some()
            {
                continue;
            }
            remove_paths.push(entry.path.clone());
        }
        remove_paths.sort();
        remove_paths.dedup();
        for path in remove_paths {
            let abs = crate::index::worktree_path_from_index_rel(work_tree, &path);
            if abs.symlink_metadata().is_ok_and(|meta| !meta.is_dir()) {
                fs::remove_file(&abs)?;
                checkout::remove_empty_parent_dirs(work_tree, &abs);
            }
        }
    }
    let autostash = fs::read_to_string(repo.git_dir().join("MERGE_AUTOSTASH")).ok();
    clear_merge_state(repo.git_dir())?;
    if let Some(oid) = autostash
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        crate::porcelain::stash::restore_operation_autostash(repo, &ObjectId::from_hex(oid)?)?;
    }
    Ok(())
}

fn merge_overwrite_error_message(paths: &[String]) -> Error {
    if paths.is_empty() {
        Error::dirty_worktree(crate::error::WorktreeOperation::Merge, Vec::new())
    } else {
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            message: "local changes would be overwritten by merge".into(),
        }
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
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    prepare_tree_checkout(repo, from_tree, to_tree)?;
    let prepared = prepare_tree_transition_allow_local(repo, from_tree, to_tree, &HashSet::new())
        .map_err(map_tree_transition_prepare_error_for_merge)?;
    checkout_prepared_tree_transition_for_merge(repo, prepared, progress, cancel)
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

/// Write the conflicted merge result to the index and work tree.
///
/// Checks `cancel` before the index is written. A cancel during the checkout returns
/// [`Error::Cancelled`] after the checkout rolled the work tree back and the previous index
/// was put back, so no conflict stages are left without merge state.
pub(crate) fn apply_conflict_merge_worktree(
    repo: &Repository,
    head_tree: &ObjectId,
    merged: &TreeMergeOutput,
    materialized: &ObjectId,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    apply_conflict_tree_merge_to_worktree(
        repo,
        head_tree,
        merged,
        materialized,
        &conflict_path_set(merged),
        progress,
        cancel,
    )
}

fn record_merge_in_progress(git_dir: &Path, other_oid: ObjectId, merge_msg: String) -> Result<()> {
    fs::write(
        git_dir.join("MERGE_HEAD"),
        format!("{}\n", other_oid.to_hex()),
    )
    .map_err(Error::Io)?;
    fs::write(git_dir.join("MERGE_MSG"), merge_msg).map_err(Error::Io)?;
    Ok(())
}

/// Run the hooks Git runs before a clean merge records its commit, after the merged index and
/// work tree are written: `pre-merge-commit` (unless `verify` skips it), then, with the merge
/// state recorded, `prepare-commit-msg <MERGE_MSG> merge` and `commit-msg <MERGE_MSG>` (unless
/// `verify` skips it).
///
/// Returns the tree to commit, rebuilt from the index when `pre-merge-commit` ran (it may have
/// staged changes), and the message, read back from `MERGE_MSG` when a message hook ran.
///
/// # Errors
///
/// [`Error::Hook`] when a hook refuses or cannot start, [`Error::IndexUnmerged`] when
/// `pre-merge-commit` left conflict stages, and I/O errors. On any error the merge state
/// (`MERGE_HEAD`, `ORIG_HEAD`, `MERGE_MSG` with `message`) is left recorded and the branch is
/// unchanged, so the merge can be finished with a commit or abandoned with an abort.
fn run_merge_commit_hooks(
    repo: &Repository,
    verify: HookPolicy,
    other_oid: ObjectId,
    tree: ObjectId,
    message: String,
) -> Result<(ObjectId, String)> {
    let git_dir = repo.git_dir();
    let index_path = repo.index_path();
    let env = CommitHookEnv {
        index_file: Some(&index_path),
        git_editor: Some(":"),
        git_prefix: None,
        extra_env: &[],
    };
    let run = || -> Result<(ObjectId, String)> {
        let mut tree = tree;
        if verify != HookPolicy::SkipVerify {
            let name = HookName::PreMergeCommit.as_str();
            if run_commit_hook(repo, name, &[], None, &env)?.check(name)? {
                let index = repo.load_index()?;
                if index.has_unmerged_entries() {
                    return Err(Error::IndexUnmerged);
                }
                tree = write_tree_from_index(repo.odb(), &index, "")?;
            }
        }
        record_merge_in_progress(git_dir, other_oid, message.clone())?;
        let edited = run_commit_message_hooks(
            repo,
            &git_dir.join("MERGE_MSG"),
            MessageSource::Merge,
            verify,
            &env,
            &mut Vec::new(),
        )?;
        Ok((tree, edited.unwrap_or_else(|| message.clone())))
    };
    match run() {
        Ok(result) => Ok(result),
        Err(err) => {
            record_merge_in_progress(git_dir, other_oid, message)?;
            Err(err)
        }
    }
}

fn strict_commit_identity(repo: &Repository, role: IdentRole) -> Result<String> {
    Ok(repo.identity(role, repo.identity_now())?.header_line())
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

fn advance_integrate_tip(
    repo: &Repository,
    target: &IntegrateTarget,
    old: ObjectId,
    new: ObjectId,
    reason: &str,
) -> Result<()> {
    match target {
        IntegrateTarget::Branch(refname) => move_branch(repo, refname, old, new, reason),
        IntegrateTarget::DetachedHead => move_detached_head(repo, old, new, reason),
    }
}

fn move_detached_head(repo: &Repository, old: ObjectId, new: ObjectId, reason: &str) -> Result<()> {
    let config = repo.config()?;
    let identity =
        resolve_loose_committer_identity_with(repo.environment(), &config, repo.identity_now());
    update_detached_head_for_commit_on_store(
        repo.refs(),
        Some(old),
        new,
        &identity,
        reason,
        config.as_ref(),
    )
}

/// Refuse to merge when detached `HEAD` no longer points at `expected`.
fn ensure_detached_head_tip(repo: &Repository, expected: ObjectId) -> Result<()> {
    let current = match repo.refs().read_raw("HEAD")? {
        Some(RawRef::Direct(oid)) => Some(oid),
        Some(RawRef::Symbolic(_)) => {
            return Err(Error::Message(
                "HEAD points at a branch; use integrate_branch".into(),
            ));
        }
        None => None,
    };
    refs::verify_branch_commit_cas(Some(expected), current, "HEAD")
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn write_merge_tree_keeps_objects_in_outer_overlay() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| grit_test_support::git(tmp.path(), args);
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(tmp.path().join("f.txt"), "1\n2\n3\n4\n5\n6\n").expect("write");
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "base"]);
        git(&["switch", "-qc", "topic"]);
        std::fs::write(tmp.path().join("f.txt"), "one\n2\n3\n4\n5\n6\n").expect("write");
        git(&["commit", "-qam", "topic"]);
        git(&["switch", "-q", "main"]);
        std::fs::write(tmp.path().join("f.txt"), "1\n2\n3\n4\n5\nsix\n").expect("write");
        git(&["commit", "-qam", "main"]);
        let oid =
            |spec: &str| -> ObjectId { git(&["rev-parse", spec]).trim().parse().expect("oid") };
        let (main, topic) = (oid("main"), oid("topic"));
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");

        repo.odb().enable_mem_overlay();
        let loose_before = repo.odb().count_loose_objects().expect("count");
        let written = write_merge_tree(&repo, main, topic, &MergePredictionOptions::default())
            .expect("write_merge_tree");
        assert!(written.is_clean());
        assert!(repo.odb().is_mem_overlay_active());
        assert_eq!(
            repo.odb().count_loose_objects().expect("count"),
            loose_before
        );
        assert!(repo.odb().exists(&written.tree));

        repo.odb().disable_mem_overlay();
        assert!(!repo.odb().exists(&written.tree));
        let written = write_merge_tree(&repo, main, topic, &MergePredictionOptions::default())
            .expect("write_merge_tree");
        assert!(!repo.odb().is_mem_overlay_active());
        assert!(repo.odb().count_loose_objects().expect("count") > loose_before);
        assert!(repo.odb().exists(&written.tree));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let up = integrate_branch(
            &repo,
            "refs/heads/main",
            head,
            head,
            "topic",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("up");
        assert_eq!(up, IntegrateBranchOutcome::UpToDate);
        let ff = integrate_branch(
            &repo,
            "refs/heads/main",
            head,
            tip,
            "topic",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("ff");
        assert!(matches!(ff, IntegrateBranchOutcome::FastForward { .. }));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
            rename_conflicts: Vec::new(),
            directory_rename_conflicts: Vec::new(),
        };
        let paths: Vec<String> = conflicts_from_tree_merge(&merged)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(paths, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
        assert!(tree_merge_has_conflicts(&merged));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let outcome = integrate_branch(
            &repo,
            "refs/heads/main",
            main,
            topic,
            "topic",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("merge");
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn integrate_branch_accepts_short_into_ref() {
        use crate::error::Error;
        use crate::refs::store::RefStoreError;

        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        grit_test_support::git(tmp.path(), &["config", "user.name", "Test"]);
        grit_test_support::git(tmp.path(), &["config", "user.email", "t@example.com"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("a.txt"), "base\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "a.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        grit_test_support::git(tmp.path(), &["switch", "-c", "other"]);
        std::fs::write(tmp.path().join("o.txt"), "other\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "o.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "other"]);
        let other: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "other"])
            .trim()
            .parse()
            .expect("other");
        grit_test_support::git(tmp.path(), &["checkout", "main"]);
        std::fs::write(tmp.path().join("m.txt"), "main\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "m.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "main"]);
        let main: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("main");

        let outcome = integrate_branch(
            &repo,
            "main",
            main,
            other,
            "other",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("short into_ref");
        let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
            panic!("expected merge");
        };
        let commit = repo.odb().read_commit(&oid).expect("merge commit");
        assert_eq!(commit.message, "Merge other\n");
        let tip_short: ObjectId =
            grit_test_support::git(tmp.path(), &["rev-parse", "refs/heads/main"])
                .trim()
                .parse()
                .expect("main tip");
        assert_eq!(tip_short, oid);

        let tmp_full = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp_full.path(), &["init", "-q", "-b", "main"]);
        grit_test_support::git(tmp_full.path(), &["config", "user.name", "Test"]);
        grit_test_support::git(tmp_full.path(), &["config", "user.email", "t@example.com"]);
        let repo_full =
            Repository::discover(Some(tmp_full.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp_full.path().join("a.txt"), "base\n").expect("write");
        grit_test_support::git(tmp_full.path(), &["add", "a.txt"]);
        grit_test_support::git(tmp_full.path(), &["commit", "-m", "base"]);
        grit_test_support::git(tmp_full.path(), &["switch", "-c", "other"]);
        std::fs::write(tmp_full.path().join("o.txt"), "other\n").expect("write");
        grit_test_support::git(tmp_full.path(), &["add", "o.txt"]);
        grit_test_support::git(tmp_full.path(), &["commit", "-m", "other"]);
        let other_full: ObjectId = grit_test_support::git(tmp_full.path(), &["rev-parse", "other"])
            .trim()
            .parse()
            .expect("other");
        grit_test_support::git(tmp_full.path(), &["checkout", "main"]);
        std::fs::write(tmp_full.path().join("m.txt"), "main\n").expect("write");
        grit_test_support::git(tmp_full.path(), &["add", "m.txt"]);
        grit_test_support::git(tmp_full.path(), &["commit", "-m", "main"]);
        let main_full: ObjectId = grit_test_support::git(tmp_full.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("main");
        let outcome_full = integrate_branch(
            &repo_full,
            "refs/heads/main",
            main_full,
            other_full,
            "other",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("full into_ref");
        assert!(matches!(
            outcome_full,
            IntegrateBranchOutcome::Merged { .. }
        ));
        let IntegrateBranchOutcome::Merged { oid: oid_full, .. } = outcome_full else {
            unreachable!();
        };
        let commit_full = repo_full
            .odb()
            .read_commit(&oid_full)
            .expect("merge commit");
        assert_eq!(commit_full.message, "Merge other\n");

        grit_test_support::git(tmp.path(), &["switch", "-c", "feature/foo"]);
        std::fs::write(tmp.path().join("nested.txt"), "feature\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "nested.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "feature"]);
        let feature_tip: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("feature");
        grit_test_support::git(tmp.path(), &["checkout", "main"]);
        std::fs::write(tmp.path().join("main2.txt"), "main2\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "main2.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "main2"]);
        let main2: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("main2");
        let merged = integrate_branch(
            &repo,
            "feature/foo",
            feature_tip,
            main2,
            "main",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("nested short name");
        let IntegrateBranchOutcome::Merged {
            oid: feature_merge, ..
        } = merged
        else {
            panic!("expected merge into feature/foo");
        };
        let stored: ObjectId =
            grit_test_support::git(tmp.path(), &["rev-parse", "refs/heads/feature/foo"])
                .trim()
                .parse()
                .expect("feature/foo tip");
        assert_eq!(stored, feature_merge);
        assert!(repo.refs().resolve("refs/feature/foo").is_err());

        let tmp_missing = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp_missing.path(), &["init", "-q", "-b", "main"]);
        let repo_missing =
            Repository::discover(Some(tmp_missing.path()), &RepositoryOptions::empty())
                .expect("repo");
        std::fs::write(tmp_missing.path().join("f.txt"), "x\n").expect("write");
        grit_test_support::git(tmp_missing.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp_missing.path(), &["commit", "-m", "c"]);
        let head: ObjectId = grit_test_support::git(tmp_missing.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("head");
        let err = integrate_branch(
            &repo_missing,
            "no-such-branch",
            head,
            head,
            "other",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("missing branch");
        assert!(matches!(err, Error::BranchNotFound { branch } if branch == "no-such-branch"));
        assert!(!tmp_missing.path().join(".git/ORIG_HEAD").exists());
        let tip: ObjectId = grit_test_support::git(tmp_missing.path(), &["rev-parse", "main"])
            .trim()
            .parse()
            .expect("main tip");
        assert_eq!(tip, head);

        let stale = main;
        let err = integrate_branch(
            &repo,
            "refs/heads/main",
            stale,
            other,
            "other",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("stale tip");
        assert!(
            matches!(
                err,
                Error::RefStore(RefStoreError::ExpectedMismatch { ref name, .. })
                    if name == "refs/heads/main"
            ),
            "unexpected {err:?}"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        let outcome = integrate_branch(
            &repo,
            "refs/heads/main",
            main,
            topic,
            "topic",
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("merge");
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

    #[test]
    fn continue_merge_errors_list_paths_plainly() {
        let paths = vec!["f.txt".to_owned(), "dir/g h.txt".to_owned()];
        let unmerged = ContinueMergeError::UnmergedPaths {
            paths: paths.clone(),
        }
        .to_string();
        assert_eq!(
            unmerged,
            "you still have unmerged paths: f.txt, dir/g h.txt"
        );
        let unstaged = ContinueMergeError::ResolvedNotStaged { paths }.to_string();
        assert_eq!(
            unstaged,
            "resolved in the work tree but not staged: f.txt, dir/g h.txt"
        );
        for message in [unmerged, unstaged] {
            assert!(!message.contains(['[', ']', '"']), "{message}");
        }
    }
}
