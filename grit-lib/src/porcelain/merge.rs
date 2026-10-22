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
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    apply_tree_merge_output_to_index, materialized_merge_result_tree, merge_trees_three_way,
    TreeMergeConflictPresentation, TreeMergeOutput, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind};
use crate::path_population::validate_tree_walk_entry;
use crate::path_validate::ProtectOptions;
use crate::porcelain::checkout;
use crate::porcelain::commit::{stamp_change_id_on_new_commit, write_commit_object};
use crate::porcelain::index_from_tree::{
    index_from_tree_with_worktree_stats, refresh_stage0_index_stats_from_worktree,
};
use crate::porcelain::switch::{
    checkout_prepared_tree_transition_for_merge, prepare_tree_transition_allow_local,
    verify_tree_transition_allow_local, TreeTransitionPrepareError, TreeTransitionVerifyError,
};
use crate::porcelain::tree_merge_checkout::finalize_clean_tree_merge_index;
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::refs;
use crate::repo::Repository;
use crate::state::{clear_merge_state, read_merge_heads, read_orig_head};
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

/// How a merge conflict should be presented to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeConflictKind {
    /// Both sides edited the same tracked file (textual or binary).
    Content,
    /// Both sides added the same path with different content.
    AddAdd,
    /// One side deleted the path and the other modified it.
    ModifyDelete {
        /// When true, the deletion is on the current branch (HEAD); the incoming branch modified the file.
        deleted_in_current: bool,
    },
}

/// One conflicted path reported from [`integrate_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflictDetail {
    /// Repository-relative path.
    pub path: String,
    /// Conflict shape derived from unmerged index stages.
    pub kind: MergeConflictKind,
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
/// the histories are unrelated or the worktree is not clean.
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

    let into_tree = commit_tree(repo, &into_oid)?;
    let other_tree = commit_tree(repo, &other_oid)?;
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

    let base_oid = merge_bases_first_vs_rest(repo, into_oid, &[other_oid])?
        .into_iter()
        .next()
        .ok_or_else(|| {
            Error::Message(format!(
                "'{label}' has no common history with the current branch"
            ))
        })?;
    let base_tree = commit_tree(repo, &base_oid)?;

    let presentation = TreeMergeConflictPresentation {
        label_theirs: crate::merge_trees::TheirsConflictLabel::Fixed(label),
        df_directory_ours_commit: Some(into_oid),
        df_directory_theirs_commit: Some(other_oid),
        ..TreeMergeConflictPresentation::default()
    };
    let merged = merge_trees_three_way(
        repo,
        base_tree,
        into_tree,
        other_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        presentation,
    )?;

    if has_tree_merge_conflicts(&merged) {
        let paths = tree_merge_conflict_paths(&merged);
        let details = classify_merge_conflicts(&merged.index, &paths);
        let materialized = materialized_merge_result_tree(repo.odb(), &merged)?;
        ensure_merge_tree_transition_allowed(repo, Some(&into_tree), &materialized)?;
        apply_conflict_merge_worktree(repo, &into_tree, &merged)?;
        let merge_msg = merge_message_text(label, options.message.as_deref());
        if matches!(options.commit, MergeCommitMode::Squash) {
            write_squash_msg_file(repo, into_oid, other_oid)?;
            write_squash_conflict_merge_msg(repo.git_dir(), &paths, options.message.as_deref())?;
        } else {
            record_merge_in_progress(repo.git_dir(), into_oid, other_oid, merge_msg)?;
        }
        return Ok(IntegrateBranchOutcome::Conflicts { paths, details });
    }

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
            let config = repo.config()?;
            let author = strict_commit_identity(repo, config.as_ref(), IdentRole::Author)?;
            let committer = strict_commit_identity(repo, config.as_ref(), IdentRole::Committer)?;
            let mut commit = CommitData {
                tree: merged.result_tree,
                parents: vec![into_oid, other_oid],
                author,
                committer,
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
        let trimmed = msg.trim();
        if trimmed.is_empty() {
            return format!("Merge {label}\n");
        }
        if msg.ends_with('\n') {
            return msg.to_owned();
        }
        return format!("{trimmed}\n");
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
        "{weekday} {month} {:02} {:02}:{:02}:{:02} {} {offset_str}",
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

/// Abort an in-progress merge, restoring index and worktree to [`read_orig_head`].
///
/// # Errors
///
/// Returns [`Error::Message`] when no merge is in progress, or propagates checkout and I/O errors.
pub fn abort_branch_merge(repo: &Repository) -> Result<()> {
    if read_merge_heads(repo.git_dir())?.is_empty() {
        return Err(Error::Message(
            "there is no merge in progress — nothing to abort".into(),
        ));
    }
    let orig = read_orig_head(repo.git_dir())?
        .ok_or_else(|| Error::Message("merge in progress but ORIG_HEAD is missing".into()))?;
    let head_tree = commit_tree(repo, &orig)?;

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

fn commit_tree(repo: &Repository, commit_oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb().read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "expected commit, got {}",
            obj.kind.as_str()
        )));
    }
    Ok(parse_commit(&obj.data)?.tree)
}

fn has_tree_merge_conflicts(merged: &TreeMergeOutput) -> bool {
    !merged.conflict_content.is_empty() || merged.index.entries().iter().any(|e| e.stage() != 0)
}

fn classify_merge_conflicts(index: &Index, paths: &[String]) -> Vec<MergeConflictDetail> {
    paths
        .iter()
        .map(|path| {
            let path_bytes = path.as_bytes();
            let stage1 = index.get(path_bytes, 1).is_some();
            let stage2 = index.get(path_bytes, 2).is_some();
            let stage3 = index.get(path_bytes, 3).is_some();
            let kind = match (stage1, stage2, stage3) {
                (false, true, true) => MergeConflictKind::AddAdd,
                (true, true, true) => MergeConflictKind::Content,
                (true, false, true) => MergeConflictKind::ModifyDelete {
                    deleted_in_current: true,
                },
                (true, true, false) => MergeConflictKind::ModifyDelete {
                    deleted_in_current: false,
                },
                _ => MergeConflictKind::Content,
            };
            MergeConflictDetail {
                path: path.clone(),
                kind,
            }
        })
        .collect()
}

fn tree_merge_conflict_paths(merged: &TreeMergeOutput) -> Vec<String> {
    let mut paths: Vec<String> = merged
        .conflict_content
        .keys()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .collect();
    paths.extend(
        merged
            .index
            .entries()
            .iter()
            .filter(|e| e.stage() != 0)
            .map(|e| String::from_utf8_lossy(&e.path).into_owned()),
    );
    paths.sort();
    paths.dedup();
    paths
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

fn apply_conflict_merge_worktree(
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
    fs::write(
        git_dir.join("ORIG_HEAD"),
        format!("{}\n", into_oid.to_hex()),
    )
    .map_err(Error::Io)?;
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
    refs::write_ref(repo.git_dir(), refname, &new)?;
    let env = repo.environment();
    let config = ConfigSet::load(env, Some(repo.git_dir()), true).unwrap_or_default();
    let who = resolve_loose_committer_identity_with(env, &config, repo.identity_now());
    let _ = refs::append_reflog(repo.git_dir(), refname, &old, &new, &who, reason, false);
    let _ = refs::append_reflog(repo.git_dir(), "HEAD", &old, &new, &who, reason, false);
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
        let head_tree = commit_tree(&repo, &head).expect("head tree");
        std::fs::write(tmp.path().join("tracked.txt"), "v2\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "tracked.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "target"]);
        let target_tree = commit_tree(
            &repo,
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
        grit_test_support::git(tmp.path(), &["branch", "topic"]);
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
        assert_eq!(
            tree_merge_conflict_paths(&merged),
            vec!["a.txt".to_owned(), "b.txt".to_owned()]
        );
        assert!(has_tree_merge_conflicts(&merged));
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
