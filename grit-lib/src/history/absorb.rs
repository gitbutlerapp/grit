//! Assign staged hunks to historical commits and plan a single history rewrite.

use std::collections::HashSet;

use crate::blame::{blame_file, BlameOptions};
use crate::branch_tracking::upstream_branch;
use crate::diff::structured::format_hunk_range;
use crate::diff::structured::{structured_text_diff, StructuredHunk, StructuredLineKind};
use crate::diff::DiffOptions;
use crate::diff::{diff_index_to_tree, diff_trees, DiffEntry, DiffStatus};
use crate::index::write_tree::write_tree_from_index;
use crate::merge_base::fork_point;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind};
use crate::porcelain::rebase::commit_subject_single_line;
use crate::repo::Repository;
use crate::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use crate::state::{resolve_head, HeadState};

use super::error::HistoryError;
use super::fixup::staged_fixup_paths;
use super::plan::plan_rewrite;
use super::types::{CommitEdit, EmptyPolicy, HistoryEdits, RewriteOptions, RewritePlan};

/// Default maximum number of commits considered when `--base` is omitted.
pub const DEFAULT_ABSORB_STACK_LIMIT: usize = 10;

/// How far a pure insertion may sit from blamed context before it stays staged.
const PURE_ADD_MAX_DISTANCE: usize = 3;

/// Options for [`absorb`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsorbOptions {
    /// Oldest commit boundary (exclusive). When `None`, [`default_absorb_base`] applies.
    pub base: Option<ObjectId>,
    /// Ref scope, committer, and empty policy for the combined rewrite.
    pub rewrite: RewriteOptions,
}

/// 1-based unified-diff style coordinates for one staged hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkRange {
    /// Old side start line (1-based).
    pub old_start: usize,
    /// Old side line count.
    pub old_lines: usize,
    /// New side start line (1-based).
    pub new_start: usize,
    /// New side line count.
    pub new_lines: usize,
}

/// A staged hunk assigned to a historical commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkAssignment {
    /// Repository-relative path.
    pub path: String,
    /// Staged hunk coordinates in the index↔`HEAD` diff.
    pub hunk_range: HunkRange,
    /// Commit that will receive the fixup.
    pub commit: ObjectId,
    /// First-line subject of the target commit (folded like Git's format-subject).
    pub subject: String,
}

fn format_hunk_side(start: usize, count: usize) -> String {
    if count == 0 {
        format!("{start},0")
    } else {
        format_hunk_range(start, count)
    }
}

/// Unified diff hunk header for one [`HunkRange`] (`@@ -old +new @@`).
#[must_use]
pub fn format_hunk_range_header(range: &HunkRange) -> String {
    format!(
        "@@ -{} +{} @@",
        format_hunk_side(range.old_start, range.old_lines),
        format_hunk_side(range.new_start, range.new_lines),
    )
}

/// Why a staged hunk could not be absorbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnassignedReason {
    /// No commit in range last touched the hunk context.
    NoCandidate,
    /// More than one commit in range touched the hunk context.
    MultipleCandidates,
    /// The hunk commutes with every commit in range (no fixup parent).
    CommutesWithEntireStack,
    /// A unique candidate does not commute with a descendant commit.
    DoesNotCommute,
    /// Pure insertion too far from lines blamed in `HEAD`.
    PureAdditionFarFromContext,
    /// The path was added in the index relative to `HEAD`.
    FileAdded,
    /// The path was deleted in the index relative to `HEAD`.
    FileDeleted,
}

impl UnassignedReason {
    /// Stable snake-case label for JSON output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoCandidate => "no_candidate",
            Self::MultipleCandidates => "multiple_candidates",
            Self::CommutesWithEntireStack => "commutes_with_entire_stack",
            Self::DoesNotCommute => "does_not_commute",
            Self::PureAdditionFarFromContext => "pure_addition_far_from_context",
            Self::FileAdded => "file_added",
            Self::FileDeleted => "file_deleted",
        }
    }
}

/// A staged hunk left in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnassignedHunk {
    /// Repository-relative path.
    pub path: String,
    /// Staged hunk coordinates.
    pub hunk_range: HunkRange,
    /// Why the hunk was not assigned.
    pub reason: UnassignedReason,
}

/// Result of [`absorb`]: rewrite plan plus assignment detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsorbPlan {
    /// Combined rewrite for every assigned target.
    pub plan: RewritePlan,
    /// Hunks assigned to historical commits.
    pub assignments: Vec<HunkAssignment>,
    /// Hunks that remain staged after apply.
    pub unassigned: Vec<UnassignedHunk>,
}

/// Plan absorption of staged hunks into commits between `opts.base` and `HEAD`.
///
/// # Errors
///
/// Returns [`HistoryError::BareRepository`], [`HistoryError::NothingStaged`], merge
/// [`HistoryError::Conflict`], and other planning errors.
pub fn absorb(repo: &Repository, opts: &AbsorbOptions) -> Result<AbsorbPlan, HistoryError> {
    if repo.is_bare() {
        return Err(HistoryError::BareRepository);
    }

    let head = head_commit_oid(repo)?;
    let head_tree = commit_tree(repo, head)?;
    let index = repo.load_index().map_err(library_err)?;
    let staged_entries =
        diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false).map_err(library_err)?;
    if staged_entries.is_empty() {
        return Err(HistoryError::NothingStaged);
    }

    let (stack, _base) = if let Some(base) = opts.base {
        (build_absorb_stack(repo, head, Some(base))?, base)
    } else {
        let stack = build_absorb_stack(repo, head, None)?;
        let base = stack
            .last()
            .and_then(|oldest| first_parent(repo, *oldest).ok().flatten())
            .unwrap_or_else(|| ObjectId::null(repo.odb().hash_algo()));
        (stack, base)
    };
    let stack_set: HashSet<ObjectId> = stack.iter().copied().collect();

    let mut assignments = Vec::new();
    let mut unassigned = Vec::new();

    for entry in &staged_entries {
        classify_path_hunks(
            repo,
            head,
            &head_tree,
            &index,
            entry,
            &stack,
            &stack_set,
            &mut assignments,
            &mut unassigned,
        )?;
    }

    if stack.is_empty() {
        return Ok(AbsorbPlan {
            plan: empty_absorb_plan().plan,
            assignments,
            unassigned,
        });
    }

    if assignments.is_empty() {
        return Ok(AbsorbPlan {
            plan: RewritePlan {
                commits: Vec::new(),
                ref_updates: Vec::new(),
                head: Default::default(),
            },
            assignments,
            unassigned,
        });
    }

    let mut edits = HistoryEdits::new();
    let mut rewrite_opts = opts.rewrite.clone();
    rewrite_opts.empty = EmptyPolicy::Drop;

    let mut targets: Vec<ObjectId> = assignments
        .iter()
        .map(|a| a.commit)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    targets.sort_by_key(|commit| stack.iter().position(|c| c == commit).unwrap_or(usize::MAX));

    let mut working_index = index.clone();
    for (idx, target) in targets.iter().copied().enumerate() {
        let (merge_head, target_tree) = if edits.is_empty() {
            (head_tree, commit_tree(repo, target)?)
        } else {
            let prefix = plan_rewrite(repo, &edits, &rewrite_opts)?;
            let merge_head = prefix
                .head
                .new_tree
                .or(prefix.head.old_tree)
                .unwrap_or(head_tree);
            let target_tree = prefix
                .commits
                .iter()
                .find(|row| row.old == target)
                .and_then(|row| row.new)
                .and_then(|oid| commit_tree(repo, oid).ok())
                .unwrap_or_else(|| commit_tree(repo, target).unwrap_or(head_tree));
            (merge_head, target_tree)
        };
        let merged_tree = merged_tree_for_target_with_trees(
            repo,
            merge_head,
            target_tree,
            &working_index,
            target,
            &assignments,
            &unassigned,
        )?;
        edits.insert(
            target,
            CommitEdit::Rewrite {
                tree: Some(merged_tree),
                message: None,
                author: None,
                parents: None,
            },
        );
        let prefix = plan_rewrite(repo, &edits, &rewrite_opts)?;
        let post_head_tree = prefix
            .head
            .new_tree
            .or(prefix.head.old_tree)
            .unwrap_or(head_tree);
        advance_working_index_after_target(
            repo,
            &mut working_index,
            merge_head,
            post_head_tree,
            &index,
            target,
            &assignments,
            &unassigned,
            &targets[idx + 1..],
        )?;
    }

    let plan = plan_rewrite(repo, &edits, &rewrite_opts)?;
    Ok(AbsorbPlan {
        plan,
        assignments,
        unassigned,
    })
}

fn empty_absorb_plan() -> AbsorbPlan {
    AbsorbPlan {
        plan: RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: Default::default(),
        },
        assignments: Vec::new(),
        unassigned: Vec::new(),
    }
}

/// Paths touched by staged hunks that would be consumed on apply.
///
/// # Errors
///
/// Returns [`HistoryError::BareRepository`] or diff failures.
pub fn staged_absorb_paths(repo: &Repository) -> Result<Vec<String>, HistoryError> {
    staged_fixup_paths(repo)
}

#[allow(clippy::too_many_arguments)]
fn classify_path_hunks(
    repo: &Repository,
    head: ObjectId,
    head_tree: &ObjectId,
    index: &crate::index::Index,
    entry: &DiffEntry,
    stack: &[ObjectId],
    stack_set: &HashSet<ObjectId>,
    assignments: &mut Vec<HunkAssignment>,
    unassigned: &mut Vec<UnassignedHunk>,
) -> Result<(), HistoryError> {
    let path = entry.path().to_owned();
    match entry.status {
        DiffStatus::Added => {
            for hunk in synthetic_whole_file_hunks(entry)? {
                unassigned.push(UnassignedHunk {
                    path: path.clone(),
                    hunk_range: hunk,
                    reason: UnassignedReason::FileAdded,
                });
            }
            return Ok(());
        }
        DiffStatus::Deleted => {
            for hunk in synthetic_whole_file_hunks(entry)? {
                unassigned.push(UnassignedHunk {
                    path: path.clone(),
                    hunk_range: hunk,
                    reason: UnassignedReason::FileDeleted,
                });
            }
            return Ok(());
        }
        _ => {}
    }

    let head_blob = blob_at_path(repo, *head_tree, &path)?;
    let index_blob = index
        .get(path.as_bytes(), 0)
        .and_then(|e| repo.odb().read(&e.oid).ok().map(|o| o.data))
        .unwrap_or_default();

    let diff = structured_text_diff(
        &String::from_utf8_lossy(&head_blob),
        &String::from_utf8_lossy(&index_blob),
        DiffOptions {
            context_lines: 0,
            word_level_inline: false,
            ..DiffOptions::default()
        },
    );

    let mut claimed_new_lines: Vec<(usize, usize)> = Vec::new();
    for hunk in diff.hunks {
        if !hunk_has_changes(&hunk) {
            continue;
        }
        let range = hunk_to_range(&hunk);
        if hunk_ranges_overlap_on_new_side(&range, &claimed_new_lines) {
            unassigned.push(UnassignedHunk {
                path: path.clone(),
                hunk_range: range,
                reason: UnassignedReason::DoesNotCommute,
            });
            continue;
        }
        let stack_newest_first: Vec<ObjectId> = stack.iter().rev().copied().collect();
        match assign_hunk(
            repo,
            head,
            &path,
            &head_blob,
            &stack_newest_first,
            stack_set,
            &range,
            &hunk,
        ) {
            Ok(Some(commit)) => {
                claimed_new_lines.push(new_line_span(&range));
                assignments.push(HunkAssignment {
                    path: path.clone(),
                    hunk_range: range,
                    commit,
                    subject: commit_subject(repo, commit),
                });
            }
            Ok(None) => {}
            Err(reason) => {
                claimed_new_lines.push(new_line_span(&range));
                unassigned.push(UnassignedHunk {
                    path: path.clone(),
                    hunk_range: range,
                    reason,
                });
            }
        }
    }
    Ok(())
}

fn new_line_span(range: &HunkRange) -> (usize, usize) {
    let end = range
        .new_start
        .saturating_add(range.new_lines.saturating_sub(1));
    (range.new_start, end)
}

fn hunk_ranges_overlap_on_new_side(range: &HunkRange, others: &[(usize, usize)]) -> bool {
    let (start, end) = new_line_span(range);
    others
        .iter()
        .any(|&(other_start, other_end)| start <= other_end && end >= other_start)
}

fn synthetic_whole_file_hunks(entry: &DiffEntry) -> Result<Vec<HunkRange>, HistoryError> {
    Ok(vec![HunkRange {
        old_start: 1,
        old_lines: if entry.status == DiffStatus::Deleted {
            1
        } else {
            0
        },
        new_start: 1,
        new_lines: if entry.status == DiffStatus::Added {
            1
        } else {
            0
        },
    }])
}

fn hunk_has_changes(hunk: &StructuredHunk) -> bool {
    hunk.lines
        .iter()
        .any(|line| matches!(line.kind, StructuredLineKind::Add | StructuredLineKind::Del))
}

fn hunk_to_range(hunk: &StructuredHunk) -> HunkRange {
    HunkRange {
        old_start: hunk.old_start,
        old_lines: hunk.old_lines,
        new_start: hunk.new_start,
        new_lines: hunk.new_lines,
    }
}

#[allow(clippy::too_many_arguments)]
fn assign_hunk(
    repo: &Repository,
    head: ObjectId,
    path: &str,
    head_blob: &[u8],
    stack: &[ObjectId],
    stack_set: &HashSet<ObjectId>,
    range: &HunkRange,
    hunk: &StructuredHunk,
) -> Result<Option<ObjectId>, UnassignedReason> {
    let candidates = blame_candidates(repo, head, path, head_blob, range, stack_set)?;
    match candidates.len() {
        0 => Err(UnassignedReason::NoCandidate),
        1 => {
            let candidate = candidates[0];
            if commutes_with_all_later_commits(repo, path, stack, candidate, hunk) {
                Ok(Some(candidate))
            } else {
                Err(UnassignedReason::DoesNotCommute)
            }
        }
        _ => Err(UnassignedReason::MultipleCandidates),
    }
}

fn commutes_with_all_later_commits(
    repo: &Repository,
    path: &str,
    stack_newest_first: &[ObjectId],
    candidate: ObjectId,
    hunk: &StructuredHunk,
) -> bool {
    let mut commuted = hunk_to_patch(hunk);
    for &commit in stack_newest_first {
        if commit == candidate {
            break;
        }
        let Some(parent) = first_parent(repo, commit).ok().flatten() else {
            return false;
        };
        let Some(commit_hunks) = commit_diff_hunks(repo, parent, commit, path) else {
            return false;
        };
        if commit_hunks.is_empty() {
            continue;
        }
        match commute_diff_before(&commuted, &commit_hunks) {
            Some(next) => commuted = next,
            None => return false,
        }
    }
    true
}

fn blame_candidates(
    repo: &Repository,
    head: ObjectId,
    path: &str,
    head_blob: &[u8],
    range: &HunkRange,
    stack_set: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>, UnassignedReason> {
    if range.old_lines == 0 {
        return blame_candidates_for_pure_add(repo, head, path, head_blob, range, stack_set);
    }

    let start = range.old_start;
    let end = range
        .old_start
        .saturating_add(range.old_lines.saturating_sub(1));
    let blame = blame_file(
        repo,
        path,
        &BlameOptions {
            start: Some(head),
            line_range: Some(start..=end),
            first_parent_only: true,
        },
    )
    .map_err(|_| UnassignedReason::NoCandidate)?;

    let commits: HashSet<ObjectId> = blame
        .lines
        .iter()
        .map(|line| line.commit)
        .filter(|c| stack_set.contains(c))
        .collect();
    Ok(commits.into_iter().collect())
}

fn blame_candidates_for_pure_add(
    repo: &Repository,
    head: ObjectId,
    path: &str,
    head_blob: &[u8],
    range: &HunkRange,
    stack_set: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>, UnassignedReason> {
    let head_lines = line_count(head_blob);
    let insert_at = range.new_start;
    let mut probe_lines = Vec::new();
    if insert_at > 1 {
        probe_lines.push(insert_at - 1);
    }
    if insert_at <= head_lines {
        probe_lines.push(insert_at);
    }
    if probe_lines.is_empty() {
        return Err(UnassignedReason::PureAdditionFarFromContext);
    }

    let mut commits = HashSet::new();
    for &line in &probe_lines {
        let blame = blame_file(
            repo,
            path,
            &BlameOptions {
                start: Some(head),
                line_range: Some(line..=line),
                first_parent_only: true,
            },
        )
        .map_err(|_| UnassignedReason::NoCandidate)?;
        for entry in blame.lines {
            if stack_set.contains(&entry.commit) {
                commits.insert(entry.commit);
            }
        }
    }

    if commits.is_empty() {
        return Err(UnassignedReason::PureAdditionFarFromContext);
    }

    let min_dist = probe_lines
        .iter()
        .map(|line| line.abs_diff(insert_at))
        .min()
        .unwrap_or(usize::MAX);
    if min_dist > PURE_ADD_MAX_DISTANCE {
        return Err(UnassignedReason::PureAdditionFarFromContext);
    }

    Ok(commits.into_iter().collect())
}

fn line_count(blob: &[u8]) -> usize {
    if blob.is_empty() {
        return 0;
    }
    blob.iter().filter(|&&b| b == b'\n').count() + if blob.last() == Some(&b'\n') { 0 } else { 1 }
}

#[derive(Debug, Clone)]
struct PatchHunk {
    removed_start: usize,
    removed_lines: Vec<Vec<u8>>,
    added_start: usize,
    added_lines: Vec<Vec<u8>>,
}

fn hunk_to_patch(hunk: &StructuredHunk) -> PatchHunk {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for line in &hunk.lines {
        let text = line
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<String>();
        let bytes = if text.ends_with('\n') {
            text.into_bytes()
        } else {
            let mut b = text.into_bytes();
            b.push(b'\n');
            b
        };
        match line.kind {
            StructuredLineKind::Del => removed.push(bytes),
            StructuredLineKind::Add => added.push(bytes),
            StructuredLineKind::Context => {}
        }
    }
    PatchHunk {
        removed_start: hunk.old_start,
        removed_lines: removed,
        added_start: hunk.new_start,
        added_lines: added,
    }
}

fn commute_diff_before(after: &PatchHunk, before: &[PatchHunk]) -> Option<PatchHunk> {
    before.iter().rev().try_fold(after.clone(), |after, next| {
        commute_hunks(next, &after).map(|(commuted_after, _)| commuted_after)
    })
}

fn commute_hunks(first: &PatchHunk, second: &PatchHunk) -> Option<(PatchHunk, PatchHunk)> {
    let (first_upper, first_lower) = (
        first.removed_start,
        first
            .removed_start
            .saturating_add(first.removed_lines.len().saturating_sub(1)),
    );
    let (second_upper, second_lower) = (
        second.removed_start,
        second
            .removed_start
            .saturating_add(second.removed_lines.len().saturating_sub(1)),
    );

    let (first_above, above, below) = if first_lower < second_upper {
        (true, first, second)
    } else if second_lower < first_upper {
        (false, second, first)
    } else if (first.added_lines.is_empty()
        && second.added_lines.is_empty()
        && uniform_lines(first.removed_lines.iter().chain(&second.removed_lines)))
        || (first.removed_lines.is_empty()
            && second.removed_lines.is_empty()
            && uniform_lines(first.added_lines.iter().chain(&second.added_lines)))
    {
        return Some((second.clone(), first.clone()));
    } else {
        return None;
    };

    let above = above.clone();
    let mut below = below.clone();
    let above_delta = above.added_lines.len() as i64 - above.removed_lines.len() as i64;
    let shift = above_delta * if first_above { -1 } else { 1 };
    below.removed_start = (below.removed_start as i64 + shift).max(1) as usize;
    below.added_start = (below.added_start as i64 + shift).max(1) as usize;

    if first_above {
        Some((below, above))
    } else {
        Some((above, below))
    }
}

fn uniform_lines<'a, I>(iter: I) -> bool
where
    I: IntoIterator<Item = &'a Vec<u8>>,
{
    let mut iter = iter.into_iter();
    let Some(first) = iter.next() else {
        return true;
    };
    iter.all(|line| line == first)
}

fn commit_diff_hunks(
    repo: &Repository,
    parent: ObjectId,
    commit: ObjectId,
    path: &str,
) -> Option<Vec<PatchHunk>> {
    let parent_tree = commit_tree(repo, parent).ok()?;
    let commit_tree_id = commit_tree(repo, commit).ok()?;
    tree_diff_hunks(repo, parent_tree, commit_tree_id, path)
}

fn tree_diff_hunks(
    repo: &Repository,
    old_tree: ObjectId,
    new_tree: ObjectId,
    path: &str,
) -> Option<Vec<PatchHunk>> {
    let entries = diff_trees(repo.odb(), Some(&old_tree), Some(&new_tree), "").ok()?;
    let Some(entry) = entries.iter().find(|e| e.path() == path) else {
        return Some(Vec::new());
    };
    if !matches!(
        entry.status,
        DiffStatus::Modified | DiffStatus::Added | DiffStatus::Deleted
    ) {
        return Some(Vec::new());
    }
    let old_blob = blob_at_path(repo, old_tree, path).unwrap_or_default();
    let new_blob = blob_at_path(repo, new_tree, path).unwrap_or_default();
    let diff = structured_text_diff(
        &String::from_utf8_lossy(&old_blob),
        &String::from_utf8_lossy(&new_blob),
        DiffOptions {
            context_lines: 0,
            word_level_inline: false,
            ..DiffOptions::default()
        },
    );
    Some(
        diff.hunks
            .into_iter()
            .filter(hunk_has_changes)
            .map(|h| hunk_to_patch(&h))
            .collect(),
    )
}

/// Index blob for one path after apply: new `HEAD` plus any unassigned hunks.
pub(crate) fn remaining_index_blob_for_path(
    repo: &Repository,
    pre_head_tree: ObjectId,
    post_head_tree: ObjectId,
    pre_index_blob: &[u8],
    path: &str,
    assignments: &[HunkAssignment],
) -> Result<Vec<u8>, HistoryError> {
    let pre_head_blob = blob_at_path(repo, pre_head_tree, path).unwrap_or_default();
    let post_head_blob = blob_at_path(repo, post_head_tree, path).unwrap_or_default();
    let diff = structured_text_diff(
        &String::from_utf8_lossy(&pre_head_blob),
        &String::from_utf8_lossy(pre_index_blob),
        DiffOptions {
            context_lines: 0,
            word_level_inline: false,
            ..DiffOptions::default()
        },
    );
    let assigned_keys: HashSet<(usize, usize, usize, usize)> = assignments
        .iter()
        .filter(|a| a.path == path)
        .map(|a| {
            (
                a.hunk_range.old_start,
                a.hunk_range.old_lines,
                a.hunk_range.new_start,
                a.hunk_range.new_lines,
            )
        })
        .collect();
    let mut assigned_patches = Vec::new();
    let mut unassigned_patches = Vec::new();
    for hunk in diff.hunks {
        if !hunk_has_changes(&hunk) {
            continue;
        }
        let key = (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines,
        );
        let patch = hunk_to_patch(&hunk);
        if assigned_keys.contains(&key) {
            assigned_patches.push(patch);
        } else {
            unassigned_patches.push(patch);
        }
    }
    let translated: Vec<PatchHunk> = unassigned_patches
        .into_iter()
        .map(|h| translate_hunk_to_post_head(h, &assigned_patches))
        .collect();
    Ok(apply_patch_hunks_bottom_up(&post_head_blob, &translated))
}

fn index_blob_at_path(repo: &Repository, index: &crate::index::Index, path: &str) -> Vec<u8> {
    index
        .get(path.as_bytes(), 0)
        .and_then(|entry| repo.odb().read(&entry.oid).ok().map(|raw| raw.data))
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn advance_working_index_after_target(
    repo: &Repository,
    working_index: &mut crate::index::Index,
    pre_head_tree: ObjectId,
    post_head_tree: ObjectId,
    original_index: &crate::index::Index,
    target: ObjectId,
    assignments: &[HunkAssignment],
    unassigned: &[UnassignedHunk],
    targets_after: &[ObjectId],
) -> Result<(), HistoryError> {
    let later_targets: HashSet<ObjectId> = targets_after.iter().copied().collect();
    let paths: HashSet<&str> = assignments
        .iter()
        .filter(|a| a.commit == target)
        .map(|a| a.path.as_str())
        .collect();
    for path in paths {
        let pending = unassigned.iter().any(|u| u.path == path)
            || assignments
                .iter()
                .any(|a| a.path == path && later_targets.contains(&a.commit));
        if pending {
            let pre_blob = index_blob_at_path(repo, original_index, path);
            let target_assignments: Vec<HunkAssignment> = assignments
                .iter()
                .filter(|a| a.path == path && a.commit == target)
                .cloned()
                .collect();
            let blob = remaining_index_blob_for_path(
                repo,
                pre_head_tree,
                post_head_tree,
                &pre_blob,
                path,
                &target_assignments,
            )?;
            write_index_blob(repo, working_index, path, &blob)?;
        } else {
            restore_index_path_from_tree(repo, working_index, post_head_tree, path)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn merged_tree_for_target_with_trees(
    repo: &Repository,
    head_tree: ObjectId,
    target_tree: ObjectId,
    index: &crate::index::Index,
    target: ObjectId,
    assignments: &[HunkAssignment],
    unassigned: &[UnassignedHunk],
) -> Result<ObjectId, HistoryError> {
    let partial_index =
        partial_index_for_target(repo, head_tree, index, target, assignments, unassigned)?;
    let index_tree = write_tree_from_index(repo.odb(), &partial_index, "").map_err(library_err)?;

    let merge = merge_trees_three_way(
        repo,
        head_tree,
        target_tree,
        index_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .map_err(library_err)?;

    if merge.index.has_unmerged_entries() || !merge.conflict_content.is_empty() {
        let mut paths = merge
            .conflict_content
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect::<Vec<_>>();
        paths.sort();
        return Err(HistoryError::Conflict {
            commit: target,
            paths,
        });
    }
    Ok(merge.result_tree)
}

fn partial_index_for_target(
    repo: &Repository,
    head_tree: ObjectId,
    index: &crate::index::Index,
    target: ObjectId,
    assignments: &[HunkAssignment],
    unassigned: &[UnassignedHunk],
) -> Result<crate::index::Index, HistoryError> {
    let mut out = index.clone();
    let assigned_paths: HashSet<&str> = assignments
        .iter()
        .filter(|a| a.commit == target)
        .map(|a| a.path.as_str())
        .collect();

    for path in staged_paths_union(assignments, unassigned) {
        let keep = assigned_paths.contains(path.as_str());
        if keep {
            let has_unassigned_on_path = unassigned.iter().any(|u| u.path == path);
            if has_unassigned_on_path || has_other_assignments_on_path(&path, target, assignments) {
                let blob =
                    partial_blob_for_path(repo, head_tree, index, &path, target, assignments)?;
                write_index_blob(repo, &mut out, &path, &blob)?;
            }
            continue;
        }
        restore_index_path_from_tree(repo, &mut out, head_tree, &path)?;
    }
    Ok(out)
}

fn staged_paths_union(
    assignments: &[HunkAssignment],
    unassigned: &[UnassignedHunk],
) -> HashSet<String> {
    assignments
        .iter()
        .map(|a| a.path.clone())
        .chain(unassigned.iter().map(|u| u.path.clone()))
        .collect()
}

fn has_other_assignments_on_path(
    path: &str,
    target: ObjectId,
    assignments: &[HunkAssignment],
) -> bool {
    assignments
        .iter()
        .any(|a| a.path == path && a.commit != target)
}

fn blob_with_assignments_on_path(
    repo: &Repository,
    head_tree: ObjectId,
    index: &crate::index::Index,
    path: &str,
    assignments: &[HunkAssignment],
) -> Result<Vec<u8>, HistoryError> {
    let head_blob = blob_at_path(repo, head_tree, path)?;
    let index_blob = index
        .get(path.as_bytes(), 0)
        .and_then(|e| repo.odb().read(&e.oid).ok().map(|o| o.data))
        .unwrap_or_default();
    let diff = structured_text_diff(
        &String::from_utf8_lossy(&head_blob),
        &String::from_utf8_lossy(&index_blob),
        DiffOptions {
            context_lines: 0,
            word_level_inline: false,
            ..DiffOptions::default()
        },
    );
    let target_ranges: HashSet<(usize, usize, usize, usize)> = assignments
        .iter()
        .filter(|a| a.path == path)
        .map(|a| {
            (
                a.hunk_range.old_start,
                a.hunk_range.old_lines,
                a.hunk_range.new_start,
                a.hunk_range.new_lines,
            )
        })
        .collect();

    let selected: Vec<PatchHunk> = diff
        .hunks
        .iter()
        .filter(|h| hunk_has_changes(h))
        .filter(|h| target_ranges.contains(&(h.old_start, h.old_lines, h.new_start, h.new_lines)))
        .map(hunk_to_patch)
        .collect();
    Ok(apply_patch_hunks_bottom_up(&head_blob, &selected))
}

fn partial_blob_for_path(
    repo: &Repository,
    head_tree: ObjectId,
    index: &crate::index::Index,
    path: &str,
    target: ObjectId,
    assignments: &[HunkAssignment],
) -> Result<Vec<u8>, HistoryError> {
    let selected: Vec<_> = assignments
        .iter()
        .filter(|a| a.path == path && a.commit == target)
        .cloned()
        .collect();
    blob_with_assignments_on_path(repo, head_tree, index, path, &selected)
}

fn apply_patch_hunks_bottom_up(base: &[u8], hunks: &[PatchHunk]) -> Vec<u8> {
    let mut sorted: Vec<&PatchHunk> = hunks.iter().collect();
    sorted.sort_by(|a, b| {
        b.removed_start
            .cmp(&a.removed_start)
            .then(b.added_start.cmp(&a.added_start))
    });
    let mut result = base.to_vec();
    for hunk in sorted {
        result = apply_patch_hunk_to_blob(&result, hunk);
    }
    result
}

fn net_line_delta_before(assigned: &[PatchHunk], pre_head_line: usize) -> i64 {
    let mut sorted: Vec<&PatchHunk> = assigned.iter().collect();
    sorted.sort_by_key(|h| h.removed_start);
    let mut delta = 0i64;
    for h in sorted {
        if h.removed_lines.is_empty() {
            if h.removed_start < pre_head_line {
                delta += h.added_lines.len() as i64;
            }
        } else if h.removed_start < pre_head_line {
            delta += h.added_lines.len() as i64 - h.removed_lines.len() as i64;
        }
    }
    delta
}

fn translate_hunk_to_post_head(mut hunk: PatchHunk, assigned: &[PatchHunk]) -> PatchHunk {
    let shift = net_line_delta_before(assigned, hunk.removed_start);
    if shift != 0 {
        hunk.removed_start = (hunk.removed_start as i64 + shift).max(1) as usize;
        hunk.added_start = (hunk.added_start as i64 + shift).max(1) as usize;
    }
    hunk
}

fn apply_patch_hunk_to_blob(base: &[u8], hunk: &PatchHunk) -> Vec<u8> {
    let mut lines = split_lines(base);
    if hunk.removed_lines.is_empty() {
        let insert_at = hunk.removed_start.min(lines.len());
        lines.splice(insert_at..insert_at, hunk.added_lines.iter().cloned());
        return join_lines(&lines);
    }
    let remove_at = hunk.removed_start.saturating_sub(1);
    let remove_count = hunk.removed_lines.len();
    if remove_at <= lines.len() {
        lines.splice(
            remove_at..remove_at.saturating_add(remove_count).min(lines.len()),
            hunk.added_lines.iter().cloned(),
        );
    } else {
        lines.extend(hunk.added_lines.iter().cloned());
    }
    join_lines(&lines)
}

fn split_lines(data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            out.push(data[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(data[start..].to_vec());
    }
    out
}

fn join_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    lines.iter().flat_map(|l| l.iter().copied()).collect()
}

fn write_index_blob(
    repo: &Repository,
    index: &mut crate::index::Index,
    path: &str,
    blob: &[u8],
) -> Result<(), HistoryError> {
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, blob)
        .map_err(library_err)?;
    let key = path.as_bytes();
    if let Some(entry) = index.get_mut(key, 0) {
        entry.oid = oid;
        entry.size = blob.len() as u32;
    }
    Ok(())
}

fn restore_index_path_from_tree(
    repo: &Repository,
    index: &mut crate::index::Index,
    tree: ObjectId,
    path: &str,
) -> Result<(), HistoryError> {
    match blob_at_path(repo, tree, path) {
        Ok(blob) => write_index_blob(repo, index, path, &blob),
        Err(_) => {
            index.remove_path_all_stages(path.as_bytes());
            Ok(())
        }
    }
}

/// Default exclusive base for absorb when `--base` is omitted.
///
/// # Errors
///
/// Returns planning failures when `HEAD` or branch metadata cannot be read.
pub fn default_absorb_base(repo: &Repository, head: ObjectId) -> Result<ObjectId, HistoryError> {
    let stack = build_absorb_stack(repo, head, None)?;
    Ok(stack
        .last()
        .copied()
        .and_then(|oldest| first_parent(repo, oldest).ok().flatten())
        .unwrap_or(ObjectId::null(repo.odb().hash_algo())))
}

fn build_absorb_stack(
    repo: &Repository,
    head: ObjectId,
    explicit_base: Option<ObjectId>,
) -> Result<Vec<ObjectId>, HistoryError> {
    let mut hidden = HashSet::new();
    if let Some(base) = explicit_base {
        hidden.insert(base);
    } else {
        hide_other_branch_tips(repo, &mut hidden)?;
        if let Some(branch) = current_branch_short(repo)? {
            if let Some(upstream) = upstream_branch(repo, &branch) {
                if let Ok(fp) = fork_point(repo, &upstream.full_ref, upstream.oid, head) {
                    hidden.insert(fp);
                }
            }
        }
    }

    let negative: Vec<String> = hidden.iter().map(ObjectId::to_hex).collect();
    let max_count = if explicit_base.is_some() {
        None
    } else {
        Some(DEFAULT_ABSORB_STACK_LIMIT)
    };
    let walk = rev_list(
        repo,
        &[head.to_hex()],
        &negative,
        &RevListOptions {
            ordering: OrderingMode::Topo,
            reverse: false,
            first_parent: true,
            max_count,
            ..RevListOptions::default()
        },
    )
    .map_err(library_err)?;

    let mut stack: Vec<ObjectId> = walk.commits;
    stack.retain(|commit| {
        load_commit(repo, *commit)
            .map(|c| c.parents.len() <= 1)
            .unwrap_or(false)
    });
    stack.reverse();
    Ok(stack)
}

fn hide_other_branch_tips(
    repo: &Repository,
    hidden: &mut HashSet<ObjectId>,
) -> Result<(), HistoryError> {
    let head_ref = resolve_head(repo.git_dir()).map_err(library_err)?;
    let current = match head_ref {
        HeadState::Branch { refname, .. } => refname,
        _ => return Ok(()),
    };
    for (name, oid) in repo.references().list("refs/heads/").map_err(library_err)? {
        if name == current {
            continue;
        }
        hidden.insert(oid);
    }
    Ok(())
}

fn current_branch_short(repo: &Repository) -> Result<Option<String>, HistoryError> {
    match resolve_head(repo.git_dir()).map_err(library_err)? {
        HeadState::Branch { refname, .. } => {
            Ok(refname.strip_prefix("refs/heads/").map(str::to_owned))
        }
        _ => Ok(None),
    }
}

fn library_err(err: crate::error::Error) -> HistoryError {
    HistoryError::Planning {
        message: err.to_string(),
    }
}

fn head_commit_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    match resolve_head(repo.git_dir()).map_err(library_err)? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => Ok(oid),
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => Err(HistoryError::Planning {
            message: "HEAD does not point at a commit".into(),
        }),
    }
}

fn commit_subject(repo: &Repository, commit: ObjectId) -> String {
    load_commit(repo, commit)
        .map(|data| commit_subject_single_line(&data.message))
        .unwrap_or_default()
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData, HistoryError> {
    let raw = repo.odb().read(&oid).map_err(library_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(HistoryError::Planning {
            message: format!("{oid} is not a commit object"),
        });
    }
    parse_commit(&raw.data).map_err(library_err)
}

fn commit_tree(repo: &Repository, commit: ObjectId) -> Result<ObjectId, HistoryError> {
    Ok(load_commit(repo, commit)?.tree)
}

fn first_parent(repo: &Repository, commit: ObjectId) -> Result<Option<ObjectId>, HistoryError> {
    let parents = load_commit(repo, commit)?.parents;
    Ok(parents.into_iter().next())
}

#[cfg(test)]
mod patch_tests {
    use super::{commute_hunks, PatchHunk};

    #[test]
    fn overlapping_line_replacements_do_not_commute() {
        let later = PatchHunk {
            removed_start: 4,
            removed_lines: vec![b"target-4\n".to_vec()],
            added_start: 4,
            added_lines: vec![b"later-4\n".to_vec()],
        };
        let staged = PatchHunk {
            removed_start: 4,
            removed_lines: vec![b"later-4\n".to_vec()],
            added_start: 4,
            added_lines: vec![b"staged-4\n".to_vec()],
        };
        assert!(
            super::commute_diff_before(&staged, &[later]).is_none(),
            "replacing the same line in a later commit must not commute"
        );
    }

    #[test]
    fn commute_hunks_shared_endpoint_is_not_disjoint() {
        let first = PatchHunk {
            removed_start: 3,
            removed_lines: vec![b"a\n".to_vec()],
            added_start: 3,
            added_lines: vec![b"b\n".to_vec()],
        };
        let second = PatchHunk {
            removed_start: 3,
            removed_lines: vec![b"b\n".to_vec()],
            added_start: 3,
            added_lines: vec![b"c\n".to_vec()],
        };
        assert!(
            commute_hunks(&first, &second).is_none(),
            "touching inclusive ranges must not commute as disjoint"
        );
    }
}

fn blob_at_path(repo: &Repository, tree: ObjectId, path: &str) -> Result<Vec<u8>, HistoryError> {
    let mut current = tree;
    for (idx, part) in path.split('/').enumerate() {
        let raw = repo.odb().read(&current).map_err(library_err)?;
        let entries = parse_tree(&raw.data).map_err(library_err)?;
        let Some(entry) = entries
            .iter()
            .find(|e| e.name.as_slice() == part.as_bytes())
        else {
            return Err(HistoryError::Planning {
                message: format!("path '{path}' not found in tree"),
            });
        };
        if idx + 1 == path.split('/').count() {
            let blob = repo.odb().read(&entry.oid).map_err(library_err)?;
            return Ok(blob.data);
        }
        current = entry.oid;
    }
    Err(HistoryError::Planning {
        message: format!("path '{path}' not found in tree"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commute_disjoint_hunks() {
        let a = PatchHunk {
            removed_start: 1,
            removed_lines: vec![b"a\n".to_vec()],
            added_start: 1,
            added_lines: vec![b"b\n".to_vec()],
        };
        let b = PatchHunk {
            removed_start: 3,
            removed_lines: vec![b"c\n".to_vec()],
            added_start: 3,
            added_lines: vec![b"d\n".to_vec()],
        };
        let (ab, ba) = (
            commute_hunks(&a, &b).expect("commute"),
            commute_hunks(&b, &a).expect("commute"),
        );
        assert_eq!(ab.0.added_start, 3);
        assert_eq!(ba.0.added_start, 1);
    }
}
