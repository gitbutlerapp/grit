//! Select structured diff hunks by stable id and apply them to the index.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use thiserror::Error;

use crate::apply::{
    apply_patch_to_index_and_write, FilePatch, Hunk, HunkLine, IndexApplyOptions, IndexApplyOutcome,
};
use crate::diff::hunk_id::{
    hunk_ids, hunk_ids_with_duplicate_peers, DiffSide, HunkId, HunkIdError,
};
use crate::diff::structured::{
    StructuredHunk, StructuredLine, StructuredLineKind, StructuredSegment,
};
use crate::diff::{
    collect_files_for_target, hunk_diff_bundle, unstaged_file_diffs_with_peer_hunks, DiffOptions,
    DiffStatus, DiffTarget, FileDiff,
};
use crate::error::{ApplyError, Error};
use crate::repo::Repository;

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
        /// Current ids on the stale path when cheap to compute.
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
            if !selection.ranges.is_empty() {
                return Err(HunkSelectError::RangeSelectsNothing {
                    path: selection.ranges[0].path.clone(),
                });
            }
            resolve_staged(repo, pathspecs, opts, &requested)
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

fn max_new_line_in_hunks(hunks: &[StructuredHunk]) -> usize {
    hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter_map(|l| l.new)
        .max()
        .unwrap_or(0)
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
        if !pathspecs.is_empty() && !pathspecs.iter().any(|p| p == &range.path) {
            continue;
        }
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
        let max_line = max_new_line_in_hunks(&file.hunks);
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
        if !selected_any {
            return Err(HunkSelectError::RangeSelectsNothing { path: path.clone() });
        }
        patches.push(file_patch_from_hunks(file, &partial)?);
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
    let apply =
        apply_patch_to_index_and_write(repo, &resolved.patches, &IndexApplyOptions { dry_run })?;
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
}
