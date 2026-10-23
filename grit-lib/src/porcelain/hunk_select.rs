//! Select structured diff hunks by stable id and apply them to the index.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path as StdPath;
use std::str::FromStr;

use thiserror::Error;

use crate::apply::{
    apply_hunks_to_blob, apply_patch_to_index_and_write, ApplyOptions, FilePatch, Hunk, HunkLine,
    IndexApplyOptions, IndexApplyOutcome,
};
use crate::diff::hunk_id::{
    hunk_ids, hunk_ids_with_duplicate_peers, DiffSide, HunkId, HunkIdError,
};
use crate::diff::structured::{
    StructuredHunk, StructuredLine, StructuredLineKind, StructuredSegment,
};
use crate::diff::{
    collect_files_for_target, hunk_diff_bundle, read_worktree_side_bytes_for_diff,
    refresh_index_stat_content_verified_with_rules, unstaged_file_diffs_with_peer_hunks,
    DiffOptions, DiffStatus, DiffTarget, FileDiff,
};
use crate::error::{ApplyError, Error};
use crate::index::{index_file_mtime, MODE_EXECUTABLE, MODE_REGULAR};
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::porcelain::checkout::{
    smudge_bytes_for_worktree_path, write_to_worktree_cached, LeadingDirCache,
};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use crate::worktree_rules::WorktreeRules;

/// New-side line range for partial hunk staging (1-based, inclusive).
///
/// Line numbers match `grit diff --json` `lines[].new` on the unstaged side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineRange {
    /// Repository-relative path.
    pub path: String,
    /// First new-side line number in the range (1-based).
    pub start: usize,
    /// Last new-side line number in the range (1-based, inclusive).
    pub end: usize,
}

/// Selected hunks by stable id and/or new-side line ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HunkSelection {
    /// Stable hunk ids from `grit diff --json` (`hunks[].id`).
    pub ids: Vec<HunkId>,
    /// Partial selections by new-side line number (`diff --json` `lines[].new`).
    pub ranges: Vec<LineRange>,
}

/// Failure while resolving or staging hunk ids.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum HunkSelectError {
    /// Id string is not 12 lowercase hex characters.
    #[error("unknown hunk id {id}")]
    UnknownId {
        /// Raw id token from the caller.
        id: String,
    },
    /// Id is well-formed but no longer present on the requested side.
    #[error("stale hunk id {id}")]
    StaleId {
        /// The requested id.
        id: HunkId,
        /// Path when the caller narrowed the diff (or a single candidate file).
        path: Option<String>,
        /// Current ids on the same path when cheap to compute.
        current_ids: Vec<HunkId>,
    },
    /// Id belongs to the other index/worktree side (for example staged vs unstaged).
    #[error("hunk id {id} is for the {actual_side} side, not {requested_side}")]
    WrongSide {
        /// The requested id.
        id: HunkId,
        /// Side the id was computed for.
        actual_side: &'static str,
        /// Side the caller requested.
        requested_side: &'static str,
    },
    /// Binary paths cannot be partially staged by hunk id.
    #[error("binary file {path} has no selectable hunks")]
    Binary {
        /// Repository-relative path.
        path: String,
    },
    /// Index apply failed after hunk resolution.
    #[error("{0}")]
    Apply(#[from] ApplyError),
    /// Line range lies outside the file's new-side line count.
    #[error("line range {start}-{end} is out of bounds for {path} (file has {max_line} lines)")]
    RangeOutOfBounds {
        /// Path named in the range.
        path: String,
        /// Range start (1-based).
        start: usize,
        /// Range end (1-based).
        end: usize,
        /// Last valid new-side line number.
        max_line: usize,
    },
    /// Range does not select any additions or deletions.
    #[error("line range selects nothing on {path}")]
    RangeSelectsNothing {
        /// Path named in the range.
        path: String,
    },
    /// Repository or diff engine failure (not a hunk-id selection error).
    #[error("{0}")]
    Repo(String),
}

impl From<Error> for HunkSelectError {
    fn from(err: Error) -> Self {
        HunkSelectError::Repo(err.to_string())
    }
}

impl From<HunkIdError> for HunkSelectError {
    fn from(_: HunkIdError) -> Self {
        HunkSelectError::UnknownId { id: String::new() }
    }
}

/// Result of [`stage_hunks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageHunksOutcome {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Staged ids per path (repository-relative).
    pub staged_hunks: BTreeMap<String, Vec<HunkId>>,
    /// Staged line ranges per path when line-range selection was used (`start` or `start-end` strings).
    pub staged_lines: BTreeMap<String, Vec<String>>,
    /// Index paths touched by the apply engine.
    pub added: Vec<String>,
    /// Paths updated in the index.
    pub modified: Vec<String>,
    /// Paths removed from the index.
    pub removed: Vec<String>,
}

impl StageHunksOutcome {
    fn from_apply(
        dry_run: bool,
        staged_hunks: BTreeMap<String, Vec<HunkId>>,
        staged_lines: BTreeMap<String, Vec<String>>,
        apply: IndexApplyOutcome,
    ) -> Self {
        Self {
            dry_run,
            staged_hunks,
            staged_lines,
            added: apply.added,
            modified: apply.modified,
            removed: apply.removed,
        }
    }
}

fn side_label(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Unstaged => "unstaged",
        DiffSide::Staged => "staged",
        DiffSide::Other => "other",
    }
}

/// Unstaged file diffs with staged peer hunks on the same path (for id assignment).
///
/// # Errors
///
/// Propagates diff and repository errors.
pub fn unstaged_file_diffs(
    repo: &Repository,
    pathspecs: &[String],
) -> std::result::Result<Vec<(FileDiff, Vec<StructuredHunk>)>, Error> {
    let opts = DiffOptions::from_config(repo.config()?.as_ref());
    let (files, peers) = unstaged_file_diffs_with_peer_hunks(repo, pathspecs, opts)?;
    Ok(files
        .into_iter()
        .map(|f| {
            let peer = peers.get(&f.path).cloned().unwrap_or_default();
            (f, peer)
        })
        .collect())
}

/// Parse comma-separated or repeated id tokens into a [`HunkSelection`].
///
/// # Errors
///
/// Returns [`HunkSelectError::UnknownId`] for malformed tokens.
pub fn parse_hunk_id_list(
    tokens: &[String],
) -> std::result::Result<HunkSelection, HunkSelectError> {
    let mut ids = Vec::new();
    for token in tokens {
        for part in token.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            ids.push(
                HunkId::from_str(part).map_err(|_| HunkSelectError::UnknownId {
                    id: part.to_string(),
                })?,
            );
        }
    }
    Ok(HunkSelection {
        ids,
        ranges: Vec::new(),
    })
}

/// Resolve `selection` on `side` to index-ready [`FilePatch`] rows.
///
/// Recomputes the live diff only for unstaged paths (optionally filtered by `pathspecs`).
/// Each path's id set uses the same peer-aware rules as `grit diff --json`.
///
/// # Errors
///
/// See [`HunkSelectError`].
pub fn resolve_selection(
    repo: &Repository,
    side: DiffSide,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> std::result::Result<Vec<FilePatch>, HunkSelectError> {
    // Returns index-ready patches; see [`resolve_selection_with_staged`] for staged id map.
    Ok(resolve_selection_with_staged(repo, side, selection, pathspecs)?.patches)
}

#[derive(Debug, Clone)]
struct ResolvedSelection {
    patches: Vec<FilePatch>,
    staged_hunks: BTreeMap<String, Vec<HunkId>>,
    staged_lines: BTreeMap<String, Vec<String>>,
}

fn merge_resolved(into: &mut ResolvedSelection, other: ResolvedSelection) {
    for (path, ids) in other.staged_hunks {
        into.staged_hunks.entry(path).or_default().extend(ids);
    }
    for (path, ranges) in other.staged_lines {
        into.staged_lines.entry(path).or_default().extend(ranges);
    }
    for patch in other.patches {
        if let Some(existing) = into
            .patches
            .iter_mut()
            .find(|p| p.effective_path() == patch.effective_path())
        {
            existing.hunks.extend(patch.hunks);
            existing.hunks.sort_by_key(|h| (h.new_start, h.old_start));
        } else {
            into.patches.push(patch);
        }
    }
}

fn diff_opts(repo: &Repository) -> std::result::Result<DiffOptions, HunkSelectError> {
    Ok(DiffOptions::from_config(repo.config()?.as_ref()))
}

fn resolve_selection_with_staged(
    repo: &Repository,
    side: DiffSide,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> std::result::Result<ResolvedSelection, HunkSelectError> {
    if selection.ids.is_empty() && selection.ranges.is_empty() {
        return Ok(ResolvedSelection {
            patches: Vec::new(),
            staged_hunks: BTreeMap::new(),
            staged_lines: BTreeMap::new(),
        });
    }
    let opts = diff_opts(repo)?;
    let requested: HashSet<HunkId> = selection.ids.iter().cloned().collect();

    match side {
        DiffSide::Unstaged => {
            let mut resolved = if requested.is_empty() {
                ResolvedSelection {
                    patches: Vec::new(),
                    staged_hunks: BTreeMap::new(),
                    staged_lines: BTreeMap::new(),
                }
            } else {
                resolve_unstaged(repo, pathspecs, opts.clone(), &requested)?
            };
            if !selection.ranges.is_empty() {
                let line_part =
                    resolve_line_ranges_unstaged(repo, pathspecs, opts, selection, &requested)?;
                merge_resolved(&mut resolved, line_part);
            }
            Ok(resolved)
        }
        DiffSide::Staged => {
            let mut resolved = if requested.is_empty() {
                ResolvedSelection {
                    patches: Vec::new(),
                    staged_hunks: BTreeMap::new(),
                    staged_lines: BTreeMap::new(),
                }
            } else {
                resolve_staged(repo, pathspecs, opts.clone(), &requested)?
            };
            if !selection.ranges.is_empty() {
                let line_part =
                    resolve_line_ranges_staged(repo, pathspecs, opts, selection, &requested)?;
                merge_resolved(&mut resolved, line_part);
            }
            Ok(resolved)
        }
        DiffSide::Other => {
            let id = selection.ids.first().cloned().ok_or_else(|| {
                HunkSelectError::Repo("hunk selection requires unstaged or staged side".into())
            })?;
            Err(HunkSelectError::WrongSide {
                id,
                actual_side: "other",
                requested_side: side_label(side),
            })
        }
    }
}

fn resolve_staged(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
    requested: &HashSet<HunkId>,
) -> std::result::Result<ResolvedSelection, HunkSelectError> {
    let files = collect_files_for_target(repo, &DiffTarget::IndexVsHead, pathspecs, opts)?;
    resolve_on_files(
        DiffSide::Staged,
        files,
        HashMap::new(),
        requested,
        pathspecs,
    )
}

fn resolve_unstaged(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
    requested: &HashSet<HunkId>,
) -> std::result::Result<ResolvedSelection, HunkSelectError> {
    let bundle = hunk_diff_bundle(repo, pathspecs, opts)?;
    if let Some(id) = find_id_on_staged_files(&bundle.staged_files, requested) {
        return Err(HunkSelectError::WrongSide {
            id,
            actual_side: "staged",
            requested_side: "unstaged",
        });
    }
    resolve_on_files(
        DiffSide::Unstaged,
        bundle.unstaged,
        bundle.peer_hunks,
        requested,
        pathspecs,
    )
}

fn find_id_on_staged_files(
    staged_files: &[FileDiff],
    requested: &HashSet<HunkId>,
) -> Option<HunkId> {
    for file in staged_files {
        if file.binary {
            continue;
        }
        for id in hunk_ids(DiffSide::Staged, &file.path, &file.hunks) {
            if requested.contains(&id) {
                return Some(id);
            }
        }
    }
    None
}

fn resolve_on_files(
    side: DiffSide,
    files: Vec<FileDiff>,
    peer_hunks: HashMap<String, Vec<StructuredHunk>>,
    requested: &HashSet<HunkId>,
    pathspecs: &[String],
) -> std::result::Result<ResolvedSelection, HunkSelectError> {
    let mut found = HashSet::new();
    let mut patches = Vec::new();
    let mut staged_by_path: BTreeMap<String, Vec<HunkId>> = BTreeMap::new();

    for file in &files {
        if file.binary {
            continue;
        }
        let peer = peer_hunks
            .get(&file.path)
            .map(|h| h.as_slice())
            .unwrap_or(&[]);
        let ids = hunk_ids_with_duplicate_peers(side, &file.path, &file.hunks, peer);
        let mut selected = Vec::new();
        let mut selected_ids = Vec::new();
        for (hunk, id) in file.hunks.iter().zip(ids.iter()) {
            if requested.contains(id) {
                selected.push(hunk.clone());
                selected_ids.push(id.clone());
                found.insert(id.clone());
            }
        }
        if selected.is_empty() {
            continue;
        }
        staged_by_path.insert(file.path.clone(), selected_ids);
        patches.push(file_patch_from_hunks(file, &selected)?);
    }

    let missing: Vec<HunkId> = requested
        .iter()
        .filter(|id| !found.contains(*id))
        .cloned()
        .collect();
    if let Some(id) = missing.first().cloned() {
        if let Some(path) = stale_path_hint(pathspecs, &files) {
            if files
                .iter()
                .find(|f| f.path == path)
                .is_some_and(|f| f.binary)
            {
                return Err(HunkSelectError::Binary { path });
            }
        }
        let current_ids = current_ids_for_stale(side, pathspecs, &files, &peer_hunks);
        return Err(HunkSelectError::StaleId {
            id,
            path: stale_path_hint(pathspecs, &files),
            current_ids,
        });
    }

    Ok(ResolvedSelection {
        patches,
        staged_hunks: staged_by_path,
        staged_lines: BTreeMap::new(),
    })
}

fn stale_path_hint(pathspecs: &[String], files: &[FileDiff]) -> Option<String> {
    if pathspecs.len() == 1 {
        return Some(pathspecs[0].clone());
    }
    if files.len() == 1 {
        return Some(files[0].path.clone());
    }
    None
}

fn current_ids_for_stale(
    side: DiffSide,
    pathspecs: &[String],
    files: &[FileDiff],
    peer_hunks: &HashMap<String, Vec<StructuredHunk>>,
) -> Vec<HunkId> {
    let path = stale_path_hint(pathspecs, files);
    let Some(path) = path else {
        return Vec::new();
    };
    let Some(file) = files.iter().find(|f| f.path == path) else {
        return Vec::new();
    };
    if file.binary {
        return Vec::new();
    }
    let peer = peer_hunks
        .get(&file.path)
        .map(|h| h.as_slice())
        .unwrap_or(&[]);
    hunk_ids_with_duplicate_peers(side, &file.path, &file.hunks, peer)
}

fn file_patch_from_hunks(
    file: &FileDiff,
    hunks: &[StructuredHunk],
) -> std::result::Result<FilePatch, HunkSelectError> {
    if file.binary {
        return Err(HunkSelectError::Binary {
            path: file.path.clone(),
        });
    }
    // Index preimage always comes from the existing stage-0 entry (including intent-to-add).
    let is_new = false;
    let is_deleted = matches!(file.status, DiffStatus::Deleted);
    let is_rename = matches!(file.status, DiffStatus::Renamed);
    let apply_hunks: Vec<Hunk> = hunks
        .iter()
        .enumerate()
        .map(|(i, h)| structured_hunk_to_apply_hunk(h, i + 1))
        .collect();
    Ok(FilePatch {
        diff_old_path: file.old_path.clone().or_else(|| Some(file.path.clone())),
        diff_new_path: Some(file.path.clone()),
        old_path: if is_new {
            None
        } else {
            file.old_path.clone().or_else(|| Some(file.path.clone()))
        },
        new_path: if is_deleted {
            None
        } else {
            Some(file.path.clone())
        },
        saw_old_header: true,
        saw_new_header: true,
        old_mode: file.old_mode.clone(),
        new_mode: file.new_mode.clone(),
        old_mode_line: None,
        new_mode_line: None,
        is_new,
        is_deleted,
        is_rename,
        is_copy: matches!(file.status, DiffStatus::Copied),
        similarity_index: None,
        dissimilarity_index: None,
        old_oid: None,
        new_oid: None,
        binary_patch: None,
        is_binary: false,
        hunks: apply_hunks,
        ws_rule: 0,
        is_toplevel_relative: true,
    })
}

fn merge_intervals(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable_by_key(|(s, _)| *s);
    let mut out = vec![sorted[0]];
    for (start, end) in sorted.into_iter().skip(1) {
        if let Some(last) = out.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        out.push((start, end));
    }
    out
}

fn line_in_intervals(line: usize, intervals: &[(usize, usize)]) -> bool {
    intervals
        .iter()
        .any(|(start, end)| line >= *start && line <= *end)
}

fn deletion_selected(removal_point: usize, intervals: &[(usize, usize)]) -> bool {
    intervals
        .iter()
        .any(|(start, end)| removal_point >= *start && removal_point <= *end + 1)
}

fn count_new_side_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut count = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.last() != Some(&b'\n') {
        count += 1;
    }
    count
}

fn index_side_line_count(
    repo: &Repository,
    file: &FileDiff,
) -> std::result::Result<usize, HunkSelectError> {
    if matches!(file.status, DiffStatus::Deleted) {
        return Ok(0);
    }
    let index = repo.index_handle().load().map_err(HunkSelectError::from)?;
    let Some(entry) = index.get(file.path.as_bytes(), 0) else {
        return Ok(0);
    };
    let blob = repo
        .odb()
        .read(&entry.oid)
        .map_err(|e| HunkSelectError::Repo(e.to_string()))?;
    Ok(count_new_side_lines(&blob.data))
}

fn new_side_line_count(
    repo: &Repository,
    file: &FileDiff,
) -> std::result::Result<usize, HunkSelectError> {
    if matches!(file.status, DiffStatus::Deleted) {
        return Ok(0);
    }
    let wt = repo
        .work_tree()
        .ok_or_else(|| HunkSelectError::Repo("line ranges require a work tree".into()))?;
    let mode_str = file.new_mode.as_deref().unwrap_or("100644");
    let mode = u32::from_str_radix(mode_str, 8).map_err(|_| {
        HunkSelectError::Repo(format!("invalid new mode {mode_str} for {}", file.path))
    })?;
    let config = repo.config().map_err(HunkSelectError::from)?;
    let index = repo.index_handle().load().map_err(HunkSelectError::from)?;
    let rules = WorktreeRules::from_repository(repo, &index).map_err(HunkSelectError::from)?;
    let index_entry = index.get(file.path.as_bytes(), 0);
    let bytes = read_worktree_side_bytes_for_diff(
        repo.odb(),
        wt,
        &file.path,
        mode,
        config.as_ref(),
        index_entry,
        Some(&rules),
    )
    .map_err(HunkSelectError::from)?;
    Ok(count_new_side_lines(&bytes))
}

fn range_label(start: usize, end: usize) -> String {
    if start == end {
        format!("{start}")
    } else {
        format!("{start}-{end}")
    }
}

fn validate_line_ranges(
    path: &str,
    intervals: &[(usize, usize)],
    max_line: usize,
) -> std::result::Result<(), HunkSelectError> {
    for &(start, end) in intervals {
        if start == 0 || end == 0 || start > end || end > max_line {
            return Err(HunkSelectError::RangeOutOfBounds {
                path: path.to_string(),
                start,
                end,
                max_line,
            });
        }
    }
    Ok(())
}

fn filter_hunk_by_intervals(
    hunk: &StructuredHunk,
    intervals: &[(usize, usize)],
) -> Option<StructuredHunk> {
    let mut new_cur = hunk.new_start;
    let mut old_cur = hunk.old_start;
    let mut out_lines = Vec::new();
    let mut any_change = false;

    for line in &hunk.lines {
        match line.kind {
            StructuredLineKind::Context => {
                let new_n = line.new.unwrap_or(new_cur);
                let old_n = line.old.unwrap_or(old_cur);
                out_lines.push(line.clone());
                new_cur = new_n.saturating_add(1);
                old_cur = old_n.saturating_add(1);
            }
            StructuredLineKind::Del => {
                let removal = new_cur;
                let old_n = line.old.unwrap_or(old_cur);
                if deletion_selected(removal, intervals) {
                    out_lines.push(line.clone());
                    any_change = true;
                } else {
                    let text: String = line.segments.iter().map(|s| s.text.as_str()).collect();
                    out_lines.push(StructuredLine {
                        kind: StructuredLineKind::Context,
                        old: Some(old_n),
                        new: Some(removal),
                        segments: vec![StructuredSegment {
                            text,
                            emphasis: false,
                        }],
                        no_newline_at_eof: line.no_newline_at_eof,
                    });
                }
                old_cur = old_n.saturating_add(1);
            }
            StructuredLineKind::Add => {
                let new_n = line.new.unwrap_or(new_cur);
                if line_in_intervals(new_n, intervals) {
                    out_lines.push(line.clone());
                    any_change = true;
                }
                new_cur = new_n.saturating_add(1);
            }
        }
    }

    if !any_change {
        return None;
    }

    let mut filtered = StructuredHunk {
        old_start: hunk.old_start,
        new_start: hunk.new_start,
        old_lines: hunk.old_lines,
        new_lines: hunk.new_lines,
        context: hunk.context.clone(),
        lines: out_lines,
    };
    recount_structured_hunk(&mut filtered);
    Some(filtered)
}

fn filter_hunk_complement_by_intervals(
    hunk: &StructuredHunk,
    intervals: &[(usize, usize)],
) -> Option<StructuredHunk> {
    let mut new_cur = hunk.new_start;
    let mut old_cur = hunk.old_start;
    let mut out_lines = Vec::new();
    let mut any_change = false;

    for line in &hunk.lines {
        match line.kind {
            StructuredLineKind::Context => {
                let new_n = line.new.unwrap_or(new_cur);
                let old_n = line.old.unwrap_or(old_cur);
                out_lines.push(line.clone());
                new_cur = new_n.saturating_add(1);
                old_cur = old_n.saturating_add(1);
            }
            StructuredLineKind::Del => {
                let removal = new_cur;
                let old_n = line.old.unwrap_or(old_cur);
                if deletion_selected(removal, intervals) {
                    let text: String = line.segments.iter().map(|s| s.text.as_str()).collect();
                    out_lines.push(StructuredLine {
                        kind: StructuredLineKind::Context,
                        old: Some(old_n),
                        new: Some(removal),
                        segments: vec![StructuredSegment {
                            text,
                            emphasis: false,
                        }],
                        no_newline_at_eof: line.no_newline_at_eof,
                    });
                } else {
                    out_lines.push(line.clone());
                    any_change = true;
                }
                old_cur = old_n.saturating_add(1);
            }
            StructuredLineKind::Add => {
                let new_n = line.new.unwrap_or(new_cur);
                if !line_in_intervals(new_n, intervals) {
                    out_lines.push(line.clone());
                    any_change = true;
                }
                new_cur = new_n.saturating_add(1);
            }
        }
    }

    if !any_change {
        return None;
    }

    let mut filtered = StructuredHunk {
        old_start: hunk.old_start,
        new_start: hunk.new_start,
        old_lines: hunk.old_lines,
        new_lines: hunk.new_lines,
        context: hunk.context.clone(),
        lines: out_lines,
    };
    recount_structured_hunk(&mut filtered);
    Some(filtered)
}

/// Unstaged hunks left on `path` after `selection` (full hunks for id picks, line complement for ranges).
pub(crate) fn remaining_unstaged_structured_hunks_for_path(
    repo: &Repository,
    path: &str,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<Vec<StructuredHunk>, Error> {
    let opts = DiffOptions::from_config(repo.config()?.as_ref());
    let bundle = hunk_diff_bundle(repo, pathspecs, opts)?;
    let Some(file) = bundle.unstaged.iter().find(|f| f.path == path) else {
        return Ok(Vec::new());
    };
    if file.binary {
        return Ok(Vec::new());
    }
    let peer = bundle
        .peer_hunks
        .get(path)
        .map(|h| h.as_slice())
        .unwrap_or(&[]);
    let ids = hunk_ids_with_duplicate_peers(DiffSide::Unstaged, path, &file.hunks, peer);
    let selected_ids: HashSet<&HunkId> = selection.ids.iter().collect();
    let raw_intervals: Vec<(usize, usize)> = selection
        .ranges
        .iter()
        .filter(|r| r.path == path)
        .map(|r| (r.start, r.end))
        .collect();
    let intervals = merge_intervals(&raw_intervals);

    let mut remaining = Vec::new();
    for (hunk, id) in file.hunks.iter().zip(ids.iter()) {
        if selected_ids.contains(id) {
            continue;
        }
        if intervals.is_empty() {
            remaining.push(hunk.clone());
        } else if let Some(partial) = filter_hunk_complement_by_intervals(hunk, &intervals) {
            remaining.push(partial);
        }
    }
    Ok(remaining)
}

fn recount_structured_hunk(hunk: &mut StructuredHunk) {
    let mut old_start = None;
    let mut new_start = None;
    let mut old_count = 0usize;
    let mut new_count = 0usize;
    let mut old_line = 0usize;
    let mut new_line = 0usize;

    for line in &mut hunk.lines {
        match line.kind {
            StructuredLineKind::Context => {
                if old_start.is_none() {
                    old_start = line.old.or(Some(old_line));
                    new_start = line.new.or(Some(new_line));
                }
                old_count += 1;
                new_count += 1;
                if let Some(o) = line.old {
                    old_line = o.saturating_add(1);
                } else {
                    old_line = old_line.saturating_add(1);
                }
                if let Some(n) = line.new {
                    new_line = n.saturating_add(1);
                } else {
                    new_line = new_line.saturating_add(1);
                }
            }
            StructuredLineKind::Del => {
                if old_start.is_none() {
                    old_start = line.old.or(Some(old_line));
                    new_start = Some(new_line);
                }
                old_count += 1;
                if let Some(o) = line.old {
                    old_line = o.saturating_add(1);
                } else {
                    old_line = old_line.saturating_add(1);
                }
            }
            StructuredLineKind::Add => {
                if old_start.is_none() {
                    old_start = Some(old_line);
                    new_start = line.new.or(Some(new_line));
                }
                new_count += 1;
                if let Some(n) = line.new {
                    new_line = n.saturating_add(1);
                } else {
                    new_line = new_line.saturating_add(1);
                }
            }
        }
    }

    hunk.old_start = old_start.unwrap_or(1);
    hunk.new_start = new_start.unwrap_or(1);
    hunk.old_lines = old_count;
    hunk.new_lines = new_count;
}

fn resolve_line_ranges_unstaged(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
    selection: &HunkSelection,
    id_selected: &HashSet<HunkId>,
) -> std::result::Result<ResolvedSelection, HunkSelectError> {
    let bundle = hunk_diff_bundle(repo, pathspecs, opts)?;
    let mut by_path: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let mut labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for range in &selection.ranges {
        by_path
            .entry(range.path.clone())
            .or_default()
            .push((range.start, range.end));
        labels
            .entry(range.path.clone())
            .or_default()
            .push(range_label(range.start, range.end));
    }

    let mut patches = Vec::new();
    let mut staged_lines = BTreeMap::new();

    for (path, raw_intervals) in by_path {
        let intervals = merge_intervals(&raw_intervals);
        let file = bundle
            .unstaged
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| HunkSelectError::RangeSelectsNothing { path: path.clone() })?;
        if file.binary {
            return Err(HunkSelectError::Binary { path: path.clone() });
        }
        let max_line = new_side_line_count(repo, file)?;
        validate_line_ranges(&path, &intervals, max_line)?;
        let peer = bundle
            .peer_hunks
            .get(&path)
            .map(|h| h.as_slice())
            .unwrap_or(&[]);
        let ids = hunk_ids_with_duplicate_peers(DiffSide::Unstaged, &path, &file.hunks, peer);
        let mut partial = Vec::new();
        let mut selected_any = false;
        for (hunk, id) in file.hunks.iter().zip(ids.iter()) {
            if id_selected.contains(id) {
                continue;
            }
            if let Some(filtered) = filter_hunk_by_intervals(hunk, &intervals) {
                selected_any = true;
                partial.push(filtered);
            }
        }
        let id_covered_any = file
            .hunks
            .iter()
            .zip(ids.iter())
            .any(|(_, id)| id_selected.contains(id));
        if !selected_any && !id_covered_any {
            return Err(HunkSelectError::RangeSelectsNothing { path: path.clone() });
        }
        if selected_any {
            patches.push(file_patch_from_hunks(file, &partial)?);
        }
        if let Some(labs) = labels.remove(&path) {
            staged_lines.insert(path, labs);
        }
    }

    Ok(ResolvedSelection {
        patches,
        staged_hunks: BTreeMap::new(),
        staged_lines,
    })
}

fn resolve_line_ranges_staged(
    repo: &Repository,
    pathspecs: &[String],
    opts: DiffOptions,
    selection: &HunkSelection,
    id_selected: &HashSet<HunkId>,
) -> std::result::Result<ResolvedSelection, HunkSelectError> {
    let files = collect_files_for_target(repo, &DiffTarget::IndexVsHead, pathspecs, opts)?;
    let mut by_path: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let mut labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for range in &selection.ranges {
        by_path
            .entry(range.path.clone())
            .or_default()
            .push((range.start, range.end));
        labels
            .entry(range.path.clone())
            .or_default()
            .push(range_label(range.start, range.end));
    }

    let mut patches = Vec::new();
    let mut staged_lines = BTreeMap::new();

    for (path, raw_intervals) in by_path {
        let intervals = merge_intervals(&raw_intervals);
        let file = files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| HunkSelectError::RangeSelectsNothing { path: path.clone() })?;
        if file.binary {
            return Err(HunkSelectError::Binary { path: path.clone() });
        }
        let max_line = index_side_line_count(repo, file)?;
        validate_line_ranges(&path, &intervals, max_line)?;
        let ids = hunk_ids(DiffSide::Staged, &path, &file.hunks);
        let mut partial = Vec::new();
        let mut selected_any = false;
        for (hunk, id) in file.hunks.iter().zip(ids.iter()) {
            if id_selected.contains(id) {
                continue;
            }
            if let Some(filtered) = filter_hunk_by_intervals(hunk, &intervals) {
                selected_any = true;
                partial.push(filtered);
            }
        }
        let id_covered_any = file
            .hunks
            .iter()
            .zip(ids.iter())
            .any(|(_, id)| id_selected.contains(id));
        if !selected_any && !id_covered_any {
            return Err(HunkSelectError::RangeSelectsNothing { path: path.clone() });
        }
        if selected_any {
            patches.push(file_patch_from_hunks(file, &partial)?);
        }
        if let Some(labs) = labels.remove(&path) {
            staged_lines.insert(path, labs);
        }
    }

    Ok(ResolvedSelection {
        patches,
        staged_hunks: BTreeMap::new(),
        staged_lines,
    })
}

fn structured_hunk_to_apply_hunk(hunk: &StructuredHunk, first_body_line: usize) -> Hunk {
    let mut lines = Vec::new();
    for line in &hunk.lines {
        let text: String = line.segments.iter().map(|s| s.text.as_str()).collect();
        match line.kind {
            StructuredLineKind::Context => lines.push(HunkLine::Context(text)),
            StructuredLineKind::Add => lines.push(HunkLine::Add(text)),
            StructuredLineKind::Del => lines.push(HunkLine::Remove(text)),
        }
        if line.no_newline_at_eof {
            lines.push(HunkLine::NoNewline);
        }
    }
    Hunk {
        old_start: hunk.old_start,
        old_count: hunk.old_lines,
        new_start: hunk.new_start,
        new_count: hunk.new_lines,
        first_body_line,
        lines,
    }
}

/// Outcome of [`restore_hunks`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestoreHunksOutcome {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Paths whose working tree or index was updated.
    pub restored: Vec<String>,
    /// Hunk ids restored per path (repository-relative).
    pub restored_hunks: BTreeMap<String, Vec<HunkId>>,
    /// Line ranges restored per path (`start` or `start-end` strings).
    pub restored_lines: BTreeMap<String, Vec<String>>,
    /// Paths removed (reserved; empty for partial hunk restore).
    pub removed: Vec<String>,
}

/// Discard or unstage the hunks in `selection`.
///
/// With `restore_worktree` (default when `restore_staged` is false), reverse-applies unstaged
/// hunks onto the worktree using clean diff bytes and checkout smudge rules. With
/// `restore_staged`, reverse-applies staged hunks onto the index. Validates every patch before
/// writing anything.
///
/// # Errors
///
/// [`HunkSelectError`] for resolution failures; [`ApplyError`] for apply failures.
pub fn restore_hunks(
    repo: &Repository,
    selection: &HunkSelection,
    pathspecs: &[String],
    restore_staged: bool,
    restore_worktree: bool,
    dry_run: bool,
) -> std::result::Result<RestoreHunksOutcome, HunkSelectError> {
    let restore_worktree = restore_worktree || !restore_staged;
    let mut outcome = RestoreHunksOutcome {
        dry_run,
        ..RestoreHunksOutcome::default()
    };
    if restore_staged {
        let side = DiffSide::Staged;
        let resolved = resolve_selection_with_staged(repo, side, selection, pathspecs)?;
        let apply = apply_patch_to_index_and_write(
            repo,
            &resolved.patches,
            &IndexApplyOptions {
                dry_run,
                reverse: true,
                head_tree_oid: head_tree_oid(repo)?,
            },
        )?;
        outcome
            .restored
            .extend(apply.added.into_iter().chain(apply.modified));
        outcome.restored.sort();
        outcome.restored.dedup();
        outcome.restored_hunks = resolved.staged_hunks;
        outcome.restored_lines = resolved.staged_lines;
    }
    if restore_worktree {
        let resolved =
            resolve_selection_with_staged(repo, DiffSide::Unstaged, selection, pathspecs)?;
        let planned = plan_worktree_reverse(repo, &resolved.patches)?;
        if !dry_run {
            commit_worktree_reverse(repo, &planned, &mut outcome)?;
        } else {
            for plan in &planned {
                push_restored_path(&mut outcome.restored, &plan.path);
            }
        }
        merge_restored_maps(&mut outcome.restored_hunks, resolved.staged_hunks);
        merge_restored_line_maps(&mut outcome.restored_lines, resolved.staged_lines);
    }
    Ok(outcome)
}

fn merge_restored_maps(
    into: &mut BTreeMap<String, Vec<HunkId>>,
    other: BTreeMap<String, Vec<HunkId>>,
) {
    for (path, ids) in other {
        into.entry(path).or_default().extend(ids);
    }
}

fn merge_restored_line_maps(
    into: &mut BTreeMap<String, Vec<String>>,
    other: BTreeMap<String, Vec<String>>,
) {
    for (path, ranges) in other {
        into.entry(path).or_default().extend(ranges);
    }
}

fn push_restored_path(restored: &mut Vec<String>, path: &str) {
    if !restored.iter().any(|p| p == path) {
        restored.push(path.to_string());
    }
}

struct WorktreeReversePlan {
    path: String,
    clean_bytes: Vec<u8>,
    mode: u32,
}

fn plan_worktree_reverse(
    repo: &Repository,
    patches: &[FilePatch],
) -> std::result::Result<Vec<WorktreeReversePlan>, HunkSelectError> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| HunkSelectError::Repo("restore hunks requires a work tree".into()))?;
    let index = repo.index_handle().load().map_err(HunkSelectError::from)?;
    let rules = WorktreeRules::from_repository(repo, &index).map_err(HunkSelectError::from)?;
    let config = repo.config().map_err(HunkSelectError::from)?;
    let mut out = Vec::with_capacity(patches.len());
    for patch in patches {
        let path = patch
            .effective_path()
            .ok_or_else(|| HunkSelectError::Repo("patch has no path".into()))?
            .to_string();
        if patch.involves_gitlink() {
            return Err(HunkSelectError::Repo(format!(
                "submodule/gitlink restore is not supported for '{path}'"
            )));
        }
        let entry = index.get(path.as_bytes(), 0);
        let mode = entry.map(|e| e.mode).unwrap_or(MODE_REGULAR);
        if mode != MODE_REGULAR && mode != MODE_EXECUTABLE {
            return Err(HunkSelectError::Binary { path: path.clone() });
        }
        let abs = crate::repo_relpath::worktree_abs(work_tree, &path);
        let clean = if patch.is_deleted || !abs.exists() {
            Vec::new()
        } else {
            read_worktree_side_bytes_for_diff(
                repo.odb(),
                work_tree,
                &path,
                mode,
                config.as_ref(),
                entry,
                Some(&rules),
            )
            .map_err(HunkSelectError::from)?
        };
        let new_clean = apply_hunks_to_blob(&clean, &patch.hunks, &ApplyOptions { reverse: true })
            .map_err(HunkSelectError::Apply)?;
        out.push(WorktreeReversePlan {
            path,
            clean_bytes: new_clean,
            mode,
        });
    }
    Ok(out)
}

fn commit_worktree_reverse(
    repo: &Repository,
    planned: &[WorktreeReversePlan],
    outcome: &mut RestoreHunksOutcome,
) -> std::result::Result<(), HunkSelectError> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| HunkSelectError::Repo("restore hunks requires a work tree".into()))?;
    let handle = repo.index_handle();
    let mut lock = handle.begin_update().map_err(HunkSelectError::from)?;
    let mut index = handle.load().map_err(HunkSelectError::from)?;
    let rules = WorktreeRules::from_repository(repo, &index).map_err(HunkSelectError::from)?;
    let mut dir_cache = LeadingDirCache::new();
    for plan in planned {
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, &plan.clean_bytes)
            .map_err(|e| HunkSelectError::Repo(e.to_string()))?;
        let worktree_bytes = smudge_bytes_for_worktree_path(
            repo,
            &rules,
            &plan.path,
            plan.mode,
            &plan.clean_bytes,
            &oid,
        )
        .map_err(HunkSelectError::from)?;
        write_to_worktree_cached(
            work_tree,
            &plan.path,
            &worktree_bytes,
            plan.mode,
            &mut dir_cache,
        )
        .map_err(|e| HunkSelectError::Repo(e.to_string()))?;
        push_restored_path(&mut outcome.restored, &plan.path);
    }
    refresh_partial_restore_index_stats(repo, work_tree, &mut index, &outcome.restored)?;
    handle
        .commit_update(&mut lock, &mut index, false)
        .map_err(|e| HunkSelectError::Repo(e.to_string()))?;
    Ok(())
}

fn head_tree_oid(repo: &Repository) -> std::result::Result<Option<ObjectId>, HunkSelectError> {
    match resolve_head(repo.git_dir()).map_err(|e| HunkSelectError::Repo(e.to_string()))? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => {
            let obj = repo
                .odb()
                .read(&oid)
                .map_err(|e| HunkSelectError::Repo(e.to_string()))?;
            let commit =
                parse_commit(&obj.data).map_err(|e| HunkSelectError::Repo(e.to_string()))?;
            Ok(Some(commit.tree))
        }
        HeadState::Branch { .. } => Ok(None),
        HeadState::Invalid => Err(HunkSelectError::Repo("HEAD is in an unknown state".into())),
    }
}

fn refresh_partial_restore_index_stats(
    repo: &Repository,
    work_tree: &StdPath,
    index: &mut crate::index::Index,
    restored: &[String],
) -> std::result::Result<(), HunkSelectError> {
    if restored.is_empty() {
        return Ok(());
    }
    let touched: BTreeSet<&str> = restored.iter().map(String::as_str).collect();
    let index_mtime = index_file_mtime(&repo.index_path());
    let rules = WorktreeRules::from_repository(repo, index).ok();
    let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
    let mut subset = index.clone();
    subset
        .entries
        .retain(|e| e.stage() == 0 && touched.contains(String::from_utf8_lossy(&e.path).as_ref()));
    if subset.entries.is_empty() {
        return Ok(());
    }
    let _ = refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        &mut subset,
        work_tree,
        index_mtime,
        repo.config().ok().as_deref(),
        None,
        rules_arc.as_ref(),
    )
    .map_err(|e| HunkSelectError::Repo(e.to_string()))?;
    for entry in subset.entries {
        index.stage_file(entry);
    }
    Ok(())
}

/// Stage the hunks in `selection` on `side` (typically unstaged for `grit add --hunks`).
///
/// # Errors
///
/// [`HunkSelectError`] for resolution failures; [`ApplyError`] for index apply failures.
pub fn stage_hunks(
    repo: &Repository,
    side: DiffSide,
    selection: &HunkSelection,
    pathspecs: &[String],
    dry_run: bool,
) -> std::result::Result<StageHunksOutcome, HunkSelectError> {
    let resolved = resolve_selection_with_staged(repo, side, selection, pathspecs)?;
    let apply = apply_patch_to_index_and_write(
        repo,
        &resolved.patches,
        &IndexApplyOptions {
            dry_run,
            reverse: false,
            head_tree_oid: None,
        },
    )?;
    Ok(StageHunksOutcome::from_apply(
        dry_run,
        resolved.staged_hunks,
        resolved.staged_lines,
        apply,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::hunk_id::hunk_ids;
    use crate::index::write_tree::write_tree_from_index;
    use crate::porcelain::add::{stage, StageOptions};
    use crate::progress::NullProgress;
    use crate::repo::{InitOptions, Repository};
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    fn spaced_two_hunk_body() -> (String, String) {
        three_hunk_body()
    }

    fn three_hunk_body() -> (String, String) {
        let mut base = String::new();
        for i in 0..30 {
            base.push_str(&format!("line{i}\n"));
        }
        let mut edited = base.clone();
        edited = edited.replacen("line5\n", "LINE5\n", 1);
        edited = edited.replacen("line13\n", "LINE13\n", 1);
        edited = edited.replacen("line21\n", "LINE21\n", 1);
        (base, edited)
    }

    #[test]
    fn stage_hunks_subset_matches_git_apply_cached() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        let (base, edited) = spaced_two_hunk_body();
        std::fs::write(root.join("f.txt"), base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);

        std::fs::write(root.join("f.txt"), edited).unwrap();
        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        assert_eq!(files.len(), 1);
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        assert!(ids.len() >= 2);
        let selection = HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: Vec::new(),
        };

        let patch = resolve_selection(&repo, DiffSide::Unstaged, &selection, &[]).unwrap();
        let mut patch_text = String::new();
        for p in &patch {
            patch_text.push_str(&format_patch_for_git(p));
        }

        stage_hunks(&repo, DiffSide::Unstaged, &selection, &[], false).unwrap();
        let grit_tree =
            write_tree_from_index(repo.odb(), &repo.index_handle().load().unwrap(), "").unwrap();

        git(root, &["read-tree", "HEAD"]);
        let patch_path = root.join("partial.diff");
        std::fs::write(&patch_path, patch_text).unwrap();
        git(root, &["apply", "--cached", patch_path.to_str().unwrap()]);
        let git_tree = Command::new("git")
            .current_dir(root)
            .args(["write-tree"])
            .output()
            .expect("write-tree");
        let git_tree_hex = String::from_utf8_lossy(&git_tree.stdout).trim().to_string();
        assert_eq!(grit_tree.to_hex(), git_tree_hex);
        git(root, &["fsck"]);
    }

    fn write_hunk_line(out: &mut String, prefix: char, text: &str) {
        out.push(prefix);
        if text.ends_with('\n') {
            out.push_str(text);
        } else {
            out.push_str(text);
            out.push('\n');
        }
    }

    fn format_patch_for_git(patch: &FilePatch) -> String {
        let mut out = String::new();
        let path = patch.effective_path().unwrap_or("f");
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        if patch.is_new {
            out.push_str("new file mode 100644\n");
        }
        if patch.is_deleted {
            let mode = patch
                .old_mode
                .as_deref()
                .or(patch.new_mode.as_deref())
                .unwrap_or("100644");
            out.push_str(&format!("deleted file mode {mode}\n"));
        }
        let old = if patch.is_new {
            "/dev/null".to_string()
        } else {
            format!("a/{}", patch.old_path.as_deref().unwrap_or(path))
        };
        let new = if patch.is_deleted {
            "/dev/null".to_string()
        } else {
            format!("b/{}", patch.new_path.as_deref().unwrap_or(path))
        };
        out.push_str(&format!("--- {old}\n"));
        out.push_str(&format!("+++ {new}\n"));
        for h in &patch.hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.old_start, h.old_count, h.new_start, h.new_count
            ));
            for line in &h.lines {
                match line {
                    HunkLine::Context(s) => write_hunk_line(&mut out, ' ', s),
                    HunkLine::Add(s) => write_hunk_line(&mut out, '+', s),
                    HunkLine::Remove(s) => write_hunk_line(&mut out, '-', s),
                    HunkLine::NoNewline => out.push_str("\\ No newline at end of file\n"),
                }
            }
        }
        out
    }

    #[test]
    fn stage_hunks_sequential_ids_stay_valid() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        let (base, edited) = spaced_two_hunk_body();
        std::fs::write(root.join("f.txt"), base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), edited).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        assert_eq!(ids.len(), 3, "expected three hunks, got {}", ids.len());
        let id0 = ids[0].clone();
        let id2 = ids[2].clone();
        stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![id0],
                ranges: Vec::new(),
            },
            &[],
            false,
        )
        .unwrap();
        stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![id2],
                ranges: Vec::new(),
            },
            &[],
            false,
        )
        .unwrap();
        git(root, &["fsck"]);
    }

    #[test]
    fn stage_hunks_inter_hunk_context_matches_diff_ids() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        git(root, &["config", "diff.interHunkContext", "1"]);
        let (base, _edited) = three_hunk_body();
        let mut edited_two = base.clone();
        edited_two = edited_two.replacen("line5\n", "LINE5\n", 1);
        edited_two = edited_two.replacen("line13\n", "LINE13\n", 1);
        std::fs::write(root.join("f.txt"), base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), edited_two).unwrap();

        let opts = DiffOptions::from_config(repo.config().unwrap().as_ref());
        let (files, peers) = unstaged_file_diffs_with_peer_hunks(&repo, &[], opts.clone()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        assert_eq!(
            ids.len(),
            1,
            "interHunkContext=1 should merge edits at lines 5 and 13"
        );
        stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            },
            &[],
            false,
        )
        .unwrap();
        git(root, &["fsck"]);
    }

    #[test]
    fn stage_hunks_text_to_binary_returns_binary_error() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        let (base, edited) = spaced_two_hunk_body();
        std::fs::write(root.join("f.txt"), base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), edited).unwrap();
        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let id = ids[0].clone();
        let binary = b"before\0after".to_vec();
        std::fs::write(root.join("f.txt"), &binary).unwrap();
        let err = stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![id],
                ranges: Vec::new(),
            },
            &["f.txt".into()],
            false,
        )
        .unwrap_err();
        assert!(
            matches!(err, HunkSelectError::Binary { .. }),
            "expected Binary, got {err:?}"
        );
    }

    #[test]
    fn stage_hunks_stale_id_rejected_index_untouched() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"one\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"two\n").unwrap();
        let before = repo.index_handle().load().unwrap();
        let stale = HunkId::from_str("000000000001").unwrap();
        let err = stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![stale.clone()],
                ranges: Vec::new(),
            },
            &[],
            false,
        )
        .unwrap_err();
        assert!(matches!(err, HunkSelectError::StaleId { .. }));
        let after = repo.index_handle().load().unwrap();
        assert_eq!(before.entries.len(), after.entries.len());
    }

    #[test]
    fn stage_hunks_staged_side_id_rejected() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"one\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"two\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let staged_files =
            collect_files_for_target(&repo, &DiffTarget::IndexVsHead, &[], DiffOptions::default())
                .unwrap();
        let staged_id = hunk_ids(DiffSide::Staged, "f.txt", &staged_files[0].hunks)[0].clone();
        std::fs::write(root.join("f.txt"), b"three\n").unwrap();
        let err = stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![staged_id],
                ranges: Vec::new(),
            },
            &[],
            false,
        )
        .unwrap_err();
        assert!(matches!(err, HunkSelectError::WrongSide { .. }));
    }

    #[test]
    fn stage_hunks_new_file_whole() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("base.txt"), b"keep\n").unwrap();
        git(root, &["add", "base.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("new.txt"), b"hello\n").unwrap();
        stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["new.txt".into()],
                intent_to_add: true,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let new_file = files
            .iter()
            .find(|f| f.path == "new.txt")
            .expect("new file in unstaged diff");
        assert!(
            !new_file.hunks.is_empty(),
            "new file should have structured hunks"
        );
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "new.txt",
            &new_file.hunks,
            peers.get("new.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: ids.clone(),
                ranges: Vec::new(),
            },
            &[],
            false,
        )
        .unwrap();
        assert!(repo
            .index_handle()
            .load()
            .unwrap()
            .get(b"new.txt", 0)
            .is_some());
        git(root, &["fsck"]);
    }

    fn stage_lines_and_compare_git(
        repo: &Repository,
        root: &std::path::Path,
        path: &str,
        ranges: Vec<LineRange>,
    ) {
        let selection = HunkSelection {
            ids: Vec::new(),
            ranges,
        };
        let patch =
            resolve_selection(&repo, DiffSide::Unstaged, &selection, &[path.into()]).unwrap();
        let mut patch_text = String::new();
        for p in &patch {
            patch_text.push_str(&format_patch_for_git(p));
        }
        stage_hunks(&repo, DiffSide::Unstaged, &selection, &[path.into()], false).unwrap();
        let grit_tree =
            write_tree_from_index(repo.odb(), &repo.index_handle().load().unwrap(), "").unwrap();
        git(root, &["read-tree", "HEAD"]);
        let patch_path = root.join("partial.diff");
        std::fs::write(&patch_path, patch_text).unwrap();
        git(root, &["apply", "--cached", patch_path.to_str().unwrap()]);
        let git_tree = Command::new("git")
            .current_dir(root)
            .args(["write-tree"])
            .output()
            .expect("write-tree");
        let git_tree_hex = String::from_utf8_lossy(&git_tree.stdout).trim().to_string();
        assert_eq!(grit_tree.to_hex(), git_tree_hex);
        git(root, &["fsck"]);
    }

    #[test]
    fn lines_selects_part_of_hunk() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"a\nb\nc\nd\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"a\nB\nc\nD\n").unwrap();
        stage_lines_and_compare_git(
            &repo,
            root,
            "f.txt",
            vec![LineRange {
                path: "f.txt".into(),
                start: 2,
                end: 2,
            }],
        );
    }

    #[test]
    fn lines_unselected_deletion_becomes_context() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"keep\nremove\nstay\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"keep\nstay\n").unwrap();
        stage_lines_and_compare_git(
            &repo,
            root,
            "f.txt",
            vec![LineRange {
                path: "f.txt".into(),
                start: 1,
                end: 1,
            }],
        );
    }

    #[test]
    fn lines_selected_deletion_at_range_edge() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"a\nb\nc\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"a\nc\n").unwrap();
        stage_lines_and_compare_git(
            &repo,
            root,
            "f.txt",
            vec![LineRange {
                path: "f.txt".into(),
                start: 1,
                end: 1,
            }],
        );
    }

    #[test]
    fn lines_range_spanning_two_hunks() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        let (base, edited) = three_hunk_body();
        std::fs::write(root.join("f.txt"), base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), edited).unwrap();
        stage_lines_and_compare_git(
            &repo,
            root,
            "f.txt",
            vec![LineRange {
                path: "f.txt".into(),
                start: 5,
                end: 14,
            }],
        );
    }

    #[test]
    fn glob_pathspec_includes_file_in_hunk_bundle() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"a\nb\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"a\nB\n").unwrap();
        let bundle = hunk_diff_bundle(&repo, &[":(glob)*.txt".into()], DiffOptions::default())
            .expect("bundle");
        assert!(
            bundle.unstaged.iter().any(|f| f.path == "f.txt"),
            "expected f.txt in bundle, got {:?}",
            bundle
                .unstaged
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn lines_pathspec_glob_stages() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"a\nb\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"a\nB\n").unwrap();
        let selection = HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 2,
                end: 2,
            }],
        };
        stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &selection,
            &[":(glob)*.txt".into()],
            false,
        )
        .unwrap();
        git(root, &["fsck"]);
    }

    #[test]
    fn lines_mix_hunk_id_overlap_stages_via_id() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"one\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"two\n").unwrap();
        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let selection = HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 1,
                end: 1,
            }],
        };
        stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &selection,
            &["f.txt".into()],
            false,
        )
        .unwrap();
        git(root, &["fsck"]);
    }

    #[test]
    fn lines_unchanged_line_beyond_hunk_is_selects_nothing() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        let mut base = String::new();
        for i in 1..=100 {
            base.push_str(&format!("line{i}\n"));
        }
        std::fs::write(root.join("f.txt"), &base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        let mut edited = base.clone();
        edited = edited.replacen("line1\n", "LINE1\n", 1);
        std::fs::write(root.join("f.txt"), edited).unwrap();
        let err = stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: Vec::new(),
                ranges: vec![LineRange {
                    path: "f.txt".into(),
                    start: 100,
                    end: 100,
                }],
            },
            &[],
            false,
        )
        .unwrap_err();
        assert!(
            matches!(err, HunkSelectError::RangeSelectsNothing { .. }),
            "expected RangeSelectsNothing, got {err:?}"
        );
    }

    #[test]
    fn lines_out_of_bounds_error() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"one\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"two\n").unwrap();
        let err = stage_hunks(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: Vec::new(),
                ranges: vec![LineRange {
                    path: "f.txt".into(),
                    start: 1,
                    end: 99,
                }],
            },
            &[],
            false,
        )
        .unwrap_err();
        assert!(matches!(err, HunkSelectError::RangeOutOfBounds { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn lines_symlink_dangling_target_matches_git_apply_cached() {
        use std::os::unix::fs::symlink;

        let (dir, repo) = init_repo();
        let root = dir.path();
        symlink("old-target", root.join("link")).expect("symlink");
        git(root, &["add", "link"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::remove_file(root.join("link")).expect("remove link");
        symlink("new-target", root.join("link")).expect("retarget");
        stage_lines_and_compare_git(
            &repo,
            root,
            "link",
            vec![LineRange {
                path: "link".into(),
                start: 1,
                end: 1,
            }],
        );
    }

    #[test]
    fn lines_no_eof_newline() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"old").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"new").unwrap();
        stage_lines_and_compare_git(
            &repo,
            root,
            "f.txt",
            vec![LineRange {
                path: "f.txt".into(),
                start: 1,
                end: 1,
            }],
        );
    }

    fn two_hunk_edit() -> (String, String) {
        let mut base = String::new();
        for i in 0..24 {
            base.push_str(&format!("line{i}\n"));
        }
        let mut edited = base.clone();
        edited = edited.replacen("line5\n", "LINE5\n", 1);
        edited = edited.replacen("line18\n", "LINE18\n", 1);
        (base, edited)
    }

    fn git_status_porcelain(root: &std::path::Path) -> String {
        String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(root)
                .args(["status", "--porcelain"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("status")
                .stdout,
        )
        .into_owned()
    }

    fn git_diff_worktree(root: &std::path::Path) -> String {
        String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(root)
                .args(["diff"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("diff")
                .stdout,
        )
        .into_owned()
    }

    #[test]
    fn restore_hunks_worktree_matches_git_apply_reverse() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        let (base, edited) = two_hunk_edit();
        std::fs::write(root.join("f.txt"), &base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), &edited).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let selection = HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: Vec::new(),
        };
        let patch = resolve_selection(&repo, DiffSide::Unstaged, &selection, &[]).unwrap();
        let patch_text = format_patch_for_git(&patch[0]);

        restore_hunks(&repo, &selection, &[], false, true, false).unwrap();

        let grit_porcelain = git_status_porcelain(root);
        let grit_diff = git_diff_worktree(root);

        let (dir2, _) = init_repo();
        let root2 = dir2.path();
        std::fs::write(root2.join("f.txt"), &base).unwrap();
        git(root2, &["add", "f.txt"]);
        git(root2, &["commit", "-qm", "init"]);
        std::fs::write(root2.join("f.txt"), &edited).unwrap();
        let patch_path = root2.join("partial.diff");
        std::fs::write(&patch_path, &patch_text).unwrap();
        git(root2, &["apply", "-R", patch_path.to_str().unwrap()]);
        let _ = std::fs::remove_file(&patch_path);

        assert_eq!(git_status_porcelain(root2), grit_porcelain);
        assert_eq!(git_diff_worktree(root2), grit_diff);
        git(root, &["fsck"]);
    }

    #[test]
    fn restore_hunks_staged_unstages_only_selected() {
        use crate::diff::collect_files_for_target;
        use crate::index::write_tree::write_tree_from_index;

        let (dir, repo) = init_repo();
        let root = dir.path();
        let (base, edited) = two_hunk_edit();
        std::fs::write(root.join("f.txt"), &base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), &edited).unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let staged_files =
            collect_files_for_target(&repo, &DiffTarget::IndexVsHead, &[], DiffOptions::default())
                .unwrap();
        let staged_id = hunk_ids(DiffSide::Staged, "f.txt", &staged_files[0].hunks)[0].clone();
        let selection = HunkSelection {
            ids: vec![staged_id],
            ranges: Vec::new(),
        };
        let patch = resolve_selection(&repo, DiffSide::Staged, &selection, &[]).unwrap();
        let patch_text = format_patch_for_git(&patch[0]);

        restore_hunks(&repo, &selection, &[], true, false, false).unwrap();
        let grit_tree =
            write_tree_from_index(repo.odb(), &repo.index_handle().load().unwrap(), "").unwrap();

        let (dir2, _) = init_repo();
        let root2 = dir2.path();
        std::fs::write(root2.join("f.txt"), &base).unwrap();
        git(root2, &["add", "f.txt"]);
        git(root2, &["commit", "-qm", "init"]);
        std::fs::write(root2.join("f.txt"), &edited).unwrap();
        git(root2, &["add", "f.txt"]);
        let patch_path = root2.join("partial.diff");
        std::fs::write(&patch_path, &patch_text).unwrap();
        git(
            root2,
            &["apply", "--cached", "-R", patch_path.to_str().unwrap()],
        );
        let _ = std::fs::remove_file(&patch_path);
        let git_tree_hex = String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(root2)
                .args(["write-tree"])
                .output()
                .expect("write-tree")
                .stdout,
        )
        .trim()
        .to_string();
        assert_eq!(grit_tree.to_hex(), git_tree_hex);
        assert_eq!(git_status_porcelain(root), git_status_porcelain(root2));
    }

    #[test]
    fn restore_hunks_crlf_autocrlf_roundtrip() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        git(root, &["config", "core.autocrlf", "true"]);
        let (base, edited) = two_hunk_edit();
        std::fs::write(root.join("f.txt"), &base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), &edited).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let selection = HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: Vec::new(),
        };
        restore_hunks(&repo, &selection, &[], false, true, false).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "after"]);
        git(root, &["fsck"]);
    }

    #[test]
    fn restore_hunks_stale_id_no_write() {
        use std::str::FromStr;

        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"one\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"two\n").unwrap();
        let before_wt = std::fs::read(root.join("f.txt")).unwrap();
        let before_len = repo.index_handle().load().unwrap().entries().len();
        let stale = HunkId::from_str("000000000001").unwrap();
        let err = restore_hunks(
            &repo,
            &HunkSelection {
                ids: vec![stale],
                ranges: Vec::new(),
            },
            &[],
            false,
            true,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, HunkSelectError::StaleId { .. }));
        assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), before_wt);
        assert_eq!(
            repo.index_handle().load().unwrap().entries().len(),
            before_len
        );
    }

    #[test]
    fn restore_staged_lines_partial() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"base\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), b"changed\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let selection = HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 1,
                end: 1,
            }],
        };
        restore_hunks(&repo, &selection, &["f.txt".into()], true, false, false).unwrap();
        assert!(Command::new("git")
            .current_dir(root)
            .args(["diff", "--cached"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("diff cached")
            .stdout
            .is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(git_status_porcelain(root), " M f.txt\n");
    }

    #[test]
    fn restore_hunks_worktree_deletion_matches_git_apply_reverse() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"content\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::remove_file(root.join("f.txt")).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let selection = HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: Vec::new(),
        };
        let patch = resolve_selection(&repo, DiffSide::Unstaged, &selection, &[]).unwrap();
        let patch_text = format_patch_for_git(&patch[0]);

        restore_hunks(&repo, &selection, &[], false, true, false).unwrap();

        let grit_porcelain = git_status_porcelain(root);
        let grit_diff = git_diff_worktree(root);

        let (dir2, _) = init_repo();
        let root2 = dir2.path();
        std::fs::write(root2.join("f.txt"), b"content\n").unwrap();
        git(root2, &["add", "f.txt"]);
        git(root2, &["commit", "-qm", "init"]);
        std::fs::remove_file(root2.join("f.txt")).unwrap();
        let patch_path = root2.join("partial.diff");
        std::fs::write(&patch_path, &patch_text).unwrap();
        git(root2, &["apply", "-R", patch_path.to_str().unwrap()]);
        let _ = std::fs::remove_file(&patch_path);

        assert_eq!(git_status_porcelain(root2), grit_porcelain);
        assert_eq!(git_diff_worktree(root2), grit_diff);
        assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"content\n");
    }

    #[test]
    fn restore_hunks_staged_deletion_matches_git_apply_cached_reverse() {
        use crate::diff::collect_files_for_target;
        use crate::index::write_tree::write_tree_from_index;

        let (dir, repo) = init_repo();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"content\n").unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::remove_file(root.join("f.txt")).unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let staged_files =
            collect_files_for_target(&repo, &DiffTarget::IndexVsHead, &[], DiffOptions::default())
                .unwrap();
        let staged_id = hunk_ids(DiffSide::Staged, "f.txt", &staged_files[0].hunks)[0].clone();
        let selection = HunkSelection {
            ids: vec![staged_id],
            ranges: Vec::new(),
        };
        let patch = resolve_selection(&repo, DiffSide::Staged, &selection, &[]).unwrap();
        let patch_text = format_patch_for_git(&patch[0]);

        restore_hunks(&repo, &selection, &[], true, false, false).unwrap();
        let grit_tree =
            write_tree_from_index(repo.odb(), &repo.index_handle().load().unwrap(), "").unwrap();

        let (dir2, _) = init_repo();
        let root2 = dir2.path();
        std::fs::write(root2.join("f.txt"), b"content\n").unwrap();
        git(root2, &["add", "f.txt"]);
        git(root2, &["commit", "-qm", "init"]);
        std::fs::remove_file(root2.join("f.txt")).unwrap();
        git(root2, &["add", "f.txt"]);
        let patch_path = root2.join("partial.diff");
        std::fs::write(&patch_path, &patch_text).unwrap();
        git(
            root2,
            &["apply", "--cached", "-R", patch_path.to_str().unwrap()],
        );
        let _ = std::fs::remove_file(&patch_path);
        let git_tree_hex = String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(root2)
                .args(["write-tree"])
                .output()
                .expect("write-tree")
                .stdout,
        )
        .trim()
        .to_string();
        assert_eq!(grit_tree.to_hex(), git_tree_hex);
        assert_eq!(git_status_porcelain(root), git_status_porcelain(root2));
    }

    #[test]
    fn restore_lines_partial() {
        let (dir, repo) = init_repo();
        let root = dir.path();
        let (base, edited) = two_hunk_edit();
        std::fs::write(root.join("f.txt"), &base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("f.txt"), &edited).unwrap();

        let selection = HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 6,
                end: 6,
            }],
        };
        let patch =
            resolve_selection(&repo, DiffSide::Unstaged, &selection, &["f.txt".into()]).unwrap();
        let patch_text = format_patch_for_git(&patch[0]);
        restore_hunks(&repo, &selection, &["f.txt".into()], false, true, false).unwrap();

        let (dir2, _) = init_repo();
        let root2 = dir2.path();
        std::fs::write(root2.join("f.txt"), &base).unwrap();
        git(root2, &["add", "f.txt"]);
        git(root2, &["commit", "-qm", "init"]);
        std::fs::write(root2.join("f.txt"), &edited).unwrap();
        let patch_path = root2.join("partial.diff");
        std::fs::write(&patch_path, &patch_text).unwrap();
        git(root2, &["apply", "-R", patch_path.to_str().unwrap()]);
        let _ = std::fs::remove_file(&patch_path);
        assert_eq!(git_status_porcelain(root), git_status_porcelain(root2));
        assert_eq!(git_diff_worktree(root), git_diff_worktree(root2));
    }
}
