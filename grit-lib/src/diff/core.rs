//! Diff machinery — compare trees, index entries, and working tree files.
//!
//! # Overview
//!
//! This module provides the core diffing infrastructure shared by `diff`,
//! `diff-index`, `status`, `log`, `show`, `commit`, and `merge`.
//!
//! ## Levels of comparison
//!
//! 1. **Tree-to-tree** — compare two tree objects (e.g. for `log`/`show`).
//! 2. **Tree-to-index** — compare a tree (usually HEAD) against the index
//!    (staged changes, used by `diff --cached` and `status`).
//! 3. **Index-to-worktree** — compare index against the working directory
//!    (unstaged changes, used by `diff` and `status`).
//!
//! ## Content diff
//!
//! Line-level diffing uses the `similar` crate (Myers, patience, minimal) and,
//! for Git's `histogram` algorithm, grit's own histogram engine for output compatible with
//! upstream Git.
//! Output formats: unified patch, raw (`:old-mode new-mode ...`), stat,
//! numstat.

use crate::git_path::CanonicalizeExt;
use std::borrow::Cow;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::diff_indent_heuristic;
use crate::diff_rename_parallel::{
    diff_rename_parallel_options, inexact_matrix_exceeds_limit, parallel_load_optional_blobs,
    parallel_map_indices, parse_rename_limit,
};
use crate::error::{Error, Result};
use crate::index::{CacheTreeNode, Index, IndexEntry, MODE_SYMLINK};
use crate::objects::{
    parse_commit, CommitData, FileMode, ObjectId, ObjectKind, TreeEntry, TreeEntryRef, TreeRefIter,
};
use crate::odb::{Odb, SessionTree};
use crate::parallel::ThreadPolicy;
use crate::repo::Repository;
use crate::userdiff::FuncnameMatcher;

use super::rename_lines::RenameLineTable;
use super::unified_emit::{
    emit_unified_body_from_edits, line_ending_only_edits, EditSpan, LineEnding, LineSide,
    UnifiedEmitOptions,
};

/// Comparison keys for every line of `side`, which must be [`LineSide::from_text`] of `text`.
///
/// Without whitespace normalization a key is the raw line including its `\n` (the same bytes as
/// `line_diff_token`), borrowed from `text`. Under `-w`/`-b`/`--ignore-space-at-eol` the keys
/// are normalized line bodies, kept in `normalized` so the returned slices can borrow them.
fn line_compare_keys<'a>(
    text: &'a str,
    side: &LineSide<'a>,
    ws: super::WhitespaceMode,
    normalized: &'a mut Vec<Cow<'a, str>>,
) -> Vec<&'a str> {
    if ws.normalizes_line_compare() {
        // Git -w/-b compare normalized bodies only; EOF newline is whitespace.
        normalized.extend(
            side.lines
                .iter()
                .map(|line| ws.normalize_line_for_compare(line.body)),
        );
        return normalized.iter().map(Cow::as_ref).collect();
    }
    // Each body is a subslice of `text` that stops just before its `\n`, so the key is the body
    // widened by that one byte.
    let base = text.as_ptr() as usize;
    side.lines
        .iter()
        .map(|line| {
            let start = line.body.as_ptr() as usize - base;
            let end = start + line.body.len() + usize::from(line.ending != LineEnding::Missing);
            &text[start..end]
        })
        .collect()
}

#[path = "../structured_diff.rs"]
pub mod structured;

pub(crate) fn imara_unified_hunk_slices(body: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    if body.starts_with("@@") {
        starts.push(0);
    }
    for (idx, _) in body.match_indices("\n@@ ") {
        starts.push(idx + 1);
    }
    starts.push(body.len());
    starts.windows(2).map(|w| &body[w[0]..w[1]]).collect()
}

/// Histogram line alignment (Git `xhistogram.c`), then `xdl_change_compact` and indent heuristic.
pub(crate) fn histogram_compacted_line_ops(
    old_content: &str,
    new_content: &str,
    indent_heuristic: bool,
    ws: super::WhitespaceMode,
) -> Vec<similar::DiffOp> {
    let before = LineSide::from_text(old_content);
    let after = LineSide::from_text(new_content);
    let old_lines: Vec<&str> = before.lines.iter().map(|l| l.body).collect();
    let new_lines: Vec<&str> = after.lines.iter().map(|l| l.body).collect();
    let old_len = old_lines.len();
    let new_len = new_lines.len();
    let (mut old_normalized, mut new_normalized) = (Vec::new(), Vec::new());
    let old_compare = line_compare_keys(old_content, &before, ws, &mut old_normalized);
    let new_compare = line_compare_keys(new_content, &after, ws, &mut new_normalized);

    let (changed1, changed2) = git_xdiff::histogram_changed_flags(&old_compare, &new_compare);

    if !changed1.iter().any(|c| *c) && !changed2.iter().any(|c| *c) {
        return Vec::new();
    }
    let raw = changed_flags_to_ops(&changed1, &changed2, old_len, new_len);
    diff_indent_heuristic::apply_change_compact_to_ops_with_ws_histogram(
        &raw,
        diff_indent_heuristic::CompactSide {
            lines: &old_lines,
            keys: &old_compare,
            compare_ids: None,
        },
        diff_indent_heuristic::CompactSide {
            lines: &new_lines,
            keys: &new_compare,
            compare_ids: None,
        },
        indent_heuristic,
        ws,
        true,
    )
}

pub(crate) fn histogram_unified_body_raw(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    indent_heuristic: bool,
    ws: super::WhitespaceMode,
) -> String {
    let before_side = LineSide::from_text(old_content);
    let after_side = LineSide::from_text(new_content);
    let ops = histogram_compacted_line_ops(old_content, new_content, indent_heuristic, ws);
    let mut edits = diff_ops_to_edit_spans(&ops);
    if edits.is_empty() {
        if old_content != new_content && !ws.normalizes_line_compare() {
            edits = line_ending_only_edits(&before_side, &after_side);
        }
        if edits.is_empty() {
            return String::new();
        }
    }
    emit_unified_body_from_edits(
        &before_side,
        &after_side,
        &edits,
        UnifiedEmitOptions {
            context_lines,
            inter_hunk_context,
            function_context: false,
        },
        None,
    )
}

/// Build the unified-diff body (no `---`/`+++` header) using Git's histogram
/// algorithm, but applying `--ignore-blank-lines` / `-I` semantics through a
/// Git-compatible implementation of the xdiff change-record machinery
/// (`xdl_mark_ignorable_lines` + `xdl_get_hunk` + `xdl_emit_diff`).
///
/// `is_ignorable_change` is called with the slices of removed and added lines
/// for one change record (each item is the line text without its trailing
/// newline); it returns `true` when the whole change is ignorable (Git's
/// `xch->ignore`). Ignorable changes that are far from substantive changes are
/// dropped, and hunk boundaries are computed with Git's `max_ignorable`
/// distance rules so the line numbers and surviving context match Git exactly.
///
/// Returns the body only; an empty string means no substantive change survives.
/// Optional `--function-context` (`-W`) hunk expansion is applied *after*
/// ignorable change-record pruning so the boundaries/funcname header match Git.
#[allow(clippy::too_many_arguments)]
fn diff_ops_to_ignorable_edit_spans<F>(
    ops: &[similar::DiffOp],
    old_lines: &[&str],
    new_lines: &[&str],
    is_ignorable_change: F,
) -> Vec<EditSpan>
where
    F: Fn(&[&str], &[&str]) -> bool,
{
    use similar::DiffOp;
    let mut out = Vec::new();
    for op in ops {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                let removed = &old_lines[old_index..old_index + old_len];
                out.push(EditSpan {
                    before_start: old_index as i64,
                    before_len: old_len as i64,
                    after_start: new_index as i64,
                    after_len: 0,
                    skip_in_output: is_ignorable_change(removed, &[]),
                });
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                let added = &new_lines[new_index..new_index + new_len];
                out.push(EditSpan {
                    before_start: old_index as i64,
                    before_len: 0,
                    after_start: new_index as i64,
                    after_len: new_len as i64,
                    skip_in_output: is_ignorable_change(&[], added),
                });
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let removed = &old_lines[old_index..old_index + old_len];
                let added = &new_lines[new_index..new_index + new_len];
                out.push(EditSpan {
                    before_start: old_index as i64,
                    before_len: old_len as i64,
                    after_start: new_index as i64,
                    after_len: new_len as i64,
                    skip_in_output: is_ignorable_change(removed, added),
                });
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn histogram_unified_body_ignore_fc<F>(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    function_context: bool,
    funcname_matcher: Option<&FuncnameMatcher>,
    indent_heuristic: bool,
    ws: super::WhitespaceMode,
    is_ignorable_change: F,
) -> String
where
    F: Fn(&[&str], &[&str]) -> bool,
{
    let before = LineSide::from_text(old_content);
    let after = LineSide::from_text(new_content);
    let old_lines: Vec<&str> = before.lines.iter().map(|l| l.body).collect();
    let new_lines: Vec<&str> = after.lines.iter().map(|l| l.body).collect();
    let ops = histogram_compacted_line_ops(old_content, new_content, indent_heuristic, ws);
    if ops.is_empty() {
        return String::new();
    }
    let edits = diff_ops_to_ignorable_edit_spans(&ops, &old_lines, &new_lines, is_ignorable_change);
    emit_unified_body_from_edits(
        &before,
        &after,
        &edits,
        UnifiedEmitOptions {
            context_lines,
            inter_hunk_context,
            function_context,
        },
        funcname_matcher,
    )
}

/// Line bodies of `side`, as the change compaction reads them.
fn line_bodies<'a>(side: &LineSide<'a>) -> Vec<&'a str> {
    side.lines.iter().map(|l| l.body).collect()
}

/// Count insertions and deletions from compacted line diff ops (patch/stat alignment).
#[must_use]
fn count_diff_ops(ops: &[similar::DiffOp]) -> (usize, usize) {
    use similar::DiffOp;
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for op in ops {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_len, .. } => deletions += old_len,
            DiffOp::Insert { new_len, .. } => insertions += new_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                deletions += old_len;
                insertions += new_len;
            }
        }
    }
    (insertions, deletions)
}

/// Line alignment ops shared by unified patches, `--numstat`, and `-G`.
pub(crate) fn compacted_line_ops_for_options(
    old_content: &str,
    new_content: &str,
    options: &crate::diff::options::DiffOptions,
) -> Vec<similar::DiffOp> {
    use crate::diff::options::DiffAlgorithm;

    let ws = options.whitespace;
    match options.algorithm {
        DiffAlgorithm::Histogram => {
            histogram_compacted_line_ops(old_content, new_content, options.indent_heuristic, ws)
        }
        DiffAlgorithm::Patience => {
            let before = LineSide::from_text(old_content);
            let after = LineSide::from_text(new_content);
            if before.is_empty() && after.is_empty() {
                return Vec::new();
            }
            let old_refs: Vec<&str> = before.lines.iter().map(|l| l.body).collect();
            let new_refs: Vec<&str> = after.lines.iter().map(|l| l.body).collect();
            let (mut old_normalized, mut new_normalized) = (Vec::new(), Vec::new());
            let old_compare = line_compare_keys(old_content, &before, ws, &mut old_normalized);
            let new_compare = line_compare_keys(new_content, &after, ws, &mut new_normalized);
            let (changed1, changed2) =
                git_xdiff::patience_changed_flags(&old_compare, &new_compare);
            let raw = changed_flags_to_ops(&changed1, &changed2, old_refs.len(), new_refs.len());
            diff_indent_heuristic::apply_change_compact_to_ops_with_ws_histogram(
                &raw,
                diff_indent_heuristic::CompactSide {
                    lines: &old_refs,
                    keys: &old_compare,
                    compare_ids: None,
                },
                diff_indent_heuristic::CompactSide {
                    lines: &new_refs,
                    keys: &new_compare,
                    compare_ids: None,
                },
                options.indent_heuristic,
                ws,
                false,
            )
        }
        DiffAlgorithm::Myers | DiffAlgorithm::Minimal => {
            let before = LineSide::from_text(old_content);
            let after = LineSide::from_text(new_content);
            if before.is_empty() && after.is_empty() {
                return Vec::new();
            }
            let (mut old_normalized, mut new_normalized) = (Vec::new(), Vec::new());
            let old_compare = line_compare_keys(old_content, &before, ws, &mut old_normalized);
            let new_compare = line_compare_keys(new_content, &after, ws, &mut new_normalized);
            git_xdiff_compacted_line_ops(
                &old_compare,
                &new_compare,
                &before,
                &after,
                options.indent_heuristic,
                options.algorithm == DiffAlgorithm::Minimal,
            )
        }
    }
}

/// Count changed lines using the same alignment as structured patches and `--numstat`.
#[must_use]
pub(crate) fn count_line_changes_with_options(
    old_content: &str,
    new_content: &str,
    options: &crate::diff::options::DiffOptions,
) -> (usize, usize) {
    let ws = options.whitespace;
    let ops = compacted_line_ops_for_options(old_content, new_content, options);
    let (mut insertions, mut deletions) = count_diff_ops(&ops);
    if insertions == 0
        && deletions == 0
        && old_content != new_content
        && !ws.normalizes_line_compare()
    {
        let before = LineSide::from_text(old_content);
        let after = LineSide::from_text(new_content);
        for edit in line_ending_only_edits(&before, &after) {
            insertions += edit.after_len as usize;
            deletions += edit.before_len as usize;
        }
    }
    (insertions, deletions)
}

/// Myers changed flags (via [`git_xdiff::changed_flags_with_shared`]) plus interned compare keys.
type XdiffTrimmedIds = ((Vec<bool>, Vec<bool>), Vec<u32>, Vec<u32>);

fn xdiff_changed_flags_trimmed_ids(
    old_compare: &[&str],
    new_compare: &[&str],
    need_min: bool,
) -> XdiffTrimmedIds {
    use crate::line_interner::LineInterner;

    let nrec1 = old_compare.len();
    let nrec2 = new_compare.len();
    if nrec1 == 0 && nrec2 == 0 {
        return ((Vec::new(), Vec::new()), Vec::new(), Vec::new());
    }
    let mut interner = LineInterner::with_capacity(nrec1 + nrec2);
    let ids1_full = interner.intern_all(old_compare);
    let ids2_full = interner.intern_all(new_compare);

    let lim = nrec1.min(nrec2);
    let mut head = 0usize;
    while head < lim && ids1_full[head] == ids2_full[head] {
        head += 1;
    }
    let mut tail = 0usize;
    let rem = lim - head;
    while tail < rem && ids1_full[nrec1 - 1 - tail] == ids2_full[nrec2 - 1 - tail] {
        tail += 1;
    }
    let shared_records = head + tail;
    let (full1, full2) = if shared_records == 0 {
        git_xdiff::changed_flags(&ids1_full, &ids2_full, need_min)
    } else {
        let cut_ids1 = &ids1_full[head..nrec1 - tail];
        let cut_ids2 = &ids2_full[head..nrec2 - tail];
        let nclasses = ids1_full
            .iter()
            .chain(&ids2_full)
            .max()
            .map_or(0, |&max| max as usize + 1);
        let mut shared_counts = vec![0u32; nclasses];
        for &id in &ids1_full[..head] {
            shared_counts[id as usize] += 1;
        }
        for &id in &ids1_full[nrec1 - tail..] {
            shared_counts[id as usize] += 1;
        }
        let (mid1, mid2) = git_xdiff::changed_flags_with_shared(
            cut_ids1,
            cut_ids2,
            &shared_counts,
            shared_records,
            need_min,
        );
        let mut full1 = vec![false; nrec1];
        let mut full2 = vec![false; nrec2];
        if !mid1.is_empty() {
            full1[head..nrec1 - tail].copy_from_slice(&mid1);
        }
        if !mid2.is_empty() {
            full2[head..nrec2 - tail].copy_from_slice(&mid2);
        }
        (full1, full2)
    };
    ((full1, full2), ids1_full, ids2_full)
}

pub(crate) fn git_xdiff_compacted_line_ops(
    old_compare: &[&str],
    new_compare: &[&str],
    before: &LineSide<'_>,
    after: &LineSide<'_>,
    indent_heuristic: bool,
    need_min: bool,
) -> Vec<similar::DiffOp> {
    let ((changed1, changed2), old_key_ids, new_key_ids) =
        xdiff_changed_flags_trimmed_ids(old_compare, new_compare, need_min);
    let old_lines = line_bodies(before);
    let new_lines = line_bodies(after);
    diff_indent_heuristic::apply_change_compact_to_masks(
        changed1,
        changed2,
        diff_indent_heuristic::CompactSide {
            lines: &old_lines,
            keys: old_compare,
            compare_ids: Some(&old_key_ids),
        },
        diff_indent_heuristic::CompactSide {
            lines: &new_lines,
            keys: new_compare,
            compare_ids: Some(&new_key_ids),
        },
        indent_heuristic,
    )
}

fn diff_ops_to_edit_spans(ops: &[similar::DiffOp]) -> Vec<EditSpan> {
    use similar::DiffOp;
    let mut out = Vec::new();
    for op in ops {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => out.push(EditSpan {
                before_start: old_index as i64,
                before_len: old_len as i64,
                after_start: new_index as i64,
                after_len: 0,
                skip_in_output: false,
            }),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => out.push(EditSpan {
                before_start: old_index as i64,
                before_len: 0,
                after_start: new_index as i64,
                after_len: new_len as i64,
                skip_in_output: false,
            }),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => out.push(EditSpan {
                before_start: old_index as i64,
                before_len: old_len as i64,
                after_start: new_index as i64,
                after_len: new_len as i64,
                skip_in_output: false,
            }),
        }
    }
    out
}

/// Unified hunk body using Git's xdiff engine (no `---` / `+++`).
fn git_xdiff_unified_body_raw(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    indent_heuristic: bool,
    need_min: bool,
    ws: super::WhitespaceMode,
) -> String {
    git_xdiff_unified_body_raw_with_ignorable(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        indent_heuristic,
        need_min,
        ws,
        None::<fn(&[&str], &[&str]) -> bool>,
    )
}

#[allow(clippy::too_many_arguments)]
fn git_xdiff_unified_body_raw_with_ignorable<F>(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    indent_heuristic: bool,
    need_min: bool,
    ws: super::WhitespaceMode,
    is_ignorable_change: Option<F>,
) -> String
where
    F: Fn(&[&str], &[&str]) -> bool,
{
    let before = LineSide::from_text(old_content);
    let after = LineSide::from_text(new_content);
    if before.is_empty() && after.is_empty() {
        return String::new();
    }
    let (mut old_normalized, mut new_normalized) = (Vec::new(), Vec::new());
    let old_compare = line_compare_keys(old_content, &before, ws, &mut old_normalized);
    let new_compare = line_compare_keys(new_content, &after, ws, &mut new_normalized);
    let ops = git_xdiff_compacted_line_ops(
        &old_compare,
        &new_compare,
        &before,
        &after,
        indent_heuristic,
        need_min,
    );
    let mut edits = if let Some(is_ignorable) = is_ignorable_change {
        diff_ops_to_ignorable_edit_spans(
            &ops,
            &line_bodies(&before),
            &line_bodies(&after),
            is_ignorable,
        )
    } else {
        diff_ops_to_edit_spans(&ops)
    };
    if edits.is_empty() {
        if old_content != new_content && !ws.normalizes_line_compare() {
            edits = line_ending_only_edits(&before, &after);
        }
        if edits.is_empty() {
            return String::new();
        }
    }
    emit_unified_body_from_edits(
        &before,
        &after,
        &edits,
        UnifiedEmitOptions {
            context_lines,
            inter_hunk_context,
            function_context: false,
        },
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn git_patience_unified_body_raw_with_ignorable<F>(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    indent_heuristic: bool,
    ws: super::WhitespaceMode,
    is_ignorable_change: Option<F>,
) -> String
where
    F: Fn(&[&str], &[&str]) -> bool,
{
    let before = LineSide::from_text(old_content);
    let after = LineSide::from_text(new_content);
    if before.is_empty() && after.is_empty() {
        return String::new();
    }
    let old_refs: Vec<&str> = before.lines.iter().map(|l| l.body).collect();
    let new_refs: Vec<&str> = after.lines.iter().map(|l| l.body).collect();
    let (mut old_normalized, mut new_normalized) = (Vec::new(), Vec::new());
    let old_compare = line_compare_keys(old_content, &before, ws, &mut old_normalized);
    let new_compare = line_compare_keys(new_content, &after, ws, &mut new_normalized);
    let (changed1, changed2) = git_xdiff::patience_changed_flags(&old_compare, &new_compare);
    let raw = changed_flags_to_ops(&changed1, &changed2, old_refs.len(), new_refs.len());
    let ops = diff_indent_heuristic::apply_change_compact_to_ops_with_ws_histogram(
        &raw,
        diff_indent_heuristic::CompactSide {
            lines: &old_refs,
            keys: &old_compare,
            compare_ids: None,
        },
        diff_indent_heuristic::CompactSide {
            lines: &new_refs,
            keys: &new_compare,
            compare_ids: None,
        },
        indent_heuristic,
        ws,
        false,
    );
    let mut edits = if let Some(is_ignorable) = is_ignorable_change {
        diff_ops_to_ignorable_edit_spans(&ops, &old_refs, &new_refs, is_ignorable)
    } else {
        diff_ops_to_edit_spans(&ops)
    };
    if edits.is_empty() {
        if old_content != new_content && !ws.normalizes_line_compare() {
            edits = line_ending_only_edits(&before, &after);
        }
        if edits.is_empty() {
            return String::new();
        }
    }
    emit_unified_body_from_edits(
        &before,
        &after,
        &edits,
        UnifiedEmitOptions {
            context_lines,
            inter_hunk_context,
            function_context: false,
        },
        None,
    )
}

fn blank_line_ignorable(ws: super::WhitespaceMode) -> impl Fn(&[&str], &[&str]) -> bool + Copy {
    move |removed: &[&str], added: &[&str]| {
        ws.ignore_blank_lines
            && removed.iter().all(|l| crate::ws::ws_blank_line(l))
            && added.iter().all(|l| crate::ws::ws_blank_line(l))
    }
}

/// Unified hunk bodies (`@@` …) for [`DiffOptions`], shared by structured and log-patch output.
pub(crate) fn unified_hunk_body_for_diff_options(
    old_content: &str,
    new_content: &str,
    options: &crate::diff::options::DiffOptions,
) -> String {
    use crate::diff::options::DiffAlgorithm;

    let ws = options.whitespace;
    if ws.is_active() {
        let blank_only = blank_line_ignorable(ws);
        if ws.normalizes_line_compare() {
            let ignorable = ws.ignore_blank_lines.then_some(blank_only);
            return match options.algorithm {
                DiffAlgorithm::Myers => git_xdiff_unified_body_raw_with_ignorable(
                    old_content,
                    new_content,
                    options.context_lines,
                    options.inter_hunk_context,
                    options.indent_heuristic,
                    false,
                    ws,
                    ignorable,
                ),
                DiffAlgorithm::Minimal => git_xdiff_unified_body_raw_with_ignorable(
                    old_content,
                    new_content,
                    options.context_lines,
                    options.inter_hunk_context,
                    options.indent_heuristic,
                    true,
                    ws,
                    ignorable,
                ),
                DiffAlgorithm::Patience => git_patience_unified_body_raw_with_ignorable(
                    old_content,
                    new_content,
                    options.context_lines,
                    options.inter_hunk_context,
                    options.indent_heuristic,
                    ws,
                    ignorable,
                ),
                DiffAlgorithm::Histogram => {
                    if ws.ignore_blank_lines {
                        histogram_unified_body_ignore_fc(
                            old_content,
                            new_content,
                            options.context_lines,
                            options.inter_hunk_context,
                            false,
                            None,
                            options.indent_heuristic,
                            ws,
                            blank_only,
                        )
                    } else {
                        histogram_unified_body_raw(
                            old_content,
                            new_content,
                            options.context_lines,
                            options.inter_hunk_context,
                            options.indent_heuristic,
                            ws,
                        )
                    }
                }
            };
        }
        if ws.ignore_blank_lines {
            return match options.algorithm {
                DiffAlgorithm::Myers => git_xdiff_unified_body_raw_with_ignorable(
                    old_content,
                    new_content,
                    options.context_lines,
                    options.inter_hunk_context,
                    options.indent_heuristic,
                    false,
                    ws,
                    Some(blank_only),
                ),
                DiffAlgorithm::Minimal => git_xdiff_unified_body_raw_with_ignorable(
                    old_content,
                    new_content,
                    options.context_lines,
                    options.inter_hunk_context,
                    options.indent_heuristic,
                    true,
                    ws,
                    Some(blank_only),
                ),
                DiffAlgorithm::Patience => git_patience_unified_body_raw_with_ignorable(
                    old_content,
                    new_content,
                    options.context_lines,
                    options.inter_hunk_context,
                    options.indent_heuristic,
                    ws,
                    Some(blank_only),
                ),
                DiffAlgorithm::Histogram => histogram_unified_body_ignore_fc(
                    old_content,
                    new_content,
                    options.context_lines,
                    options.inter_hunk_context,
                    false,
                    None,
                    options.indent_heuristic,
                    ws,
                    blank_only,
                ),
            };
        }
    }
    match options.algorithm {
        DiffAlgorithm::Myers => git_xdiff_unified_body_raw(
            old_content,
            new_content,
            options.context_lines,
            options.inter_hunk_context,
            options.indent_heuristic,
            false,
            ws,
        ),
        DiffAlgorithm::Minimal => git_xdiff_unified_body_raw(
            old_content,
            new_content,
            options.context_lines,
            options.inter_hunk_context,
            options.indent_heuristic,
            true,
            ws,
        ),
        DiffAlgorithm::Patience => git_patience_unified_body_raw(
            old_content,
            new_content,
            options.context_lines,
            options.inter_hunk_context,
            options.indent_heuristic,
        ),
        DiffAlgorithm::Histogram => histogram_unified_body_raw(
            old_content,
            new_content,
            options.context_lines,
            options.inter_hunk_context,
            options.indent_heuristic,
            ws,
        ),
    }
}

/// Unified hunk body using Git's patience diff (no `---` / `+++`).
fn git_patience_unified_body_raw(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    indent_heuristic: bool,
) -> String {
    let before = LineSide::from_text(old_content);
    let after = LineSide::from_text(new_content);
    if before.is_empty() && after.is_empty() {
        return String::new();
    }
    let old_refs: Vec<&str> = before.lines.iter().map(|l| l.body).collect();
    let new_refs: Vec<&str> = after.lines.iter().map(|l| l.body).collect();
    let ws = super::WhitespaceMode::default();
    let (mut old_normalized, mut new_normalized) = (Vec::new(), Vec::new());
    let old_compare = line_compare_keys(old_content, &before, ws, &mut old_normalized);
    let new_compare = line_compare_keys(new_content, &after, ws, &mut new_normalized);
    let (changed1, changed2) = git_xdiff::patience_changed_flags(&old_compare, &new_compare);
    let raw = changed_flags_to_ops(&changed1, &changed2, old_refs.len(), new_refs.len());
    let ops = diff_indent_heuristic::apply_change_compact_to_ops_with_ws_histogram(
        &raw,
        diff_indent_heuristic::CompactSide {
            lines: &old_refs,
            keys: &old_compare,
            compare_ids: None,
        },
        diff_indent_heuristic::CompactSide {
            lines: &new_refs,
            keys: &new_compare,
            compare_ids: None,
        },
        indent_heuristic,
        ws,
        false,
    );
    let mut edits = diff_ops_to_edit_spans(&ops);
    if edits.is_empty() {
        if old_content != new_content {
            edits = line_ending_only_edits(&before, &after);
        }
        if edits.is_empty() {
            return String::new();
        }
    }
    emit_unified_body_from_edits(
        &before,
        &after,
        &edits,
        UnifiedEmitOptions {
            context_lines,
            inter_hunk_context,
            function_context: false,
        },
        None,
    )
}

/// Parse `@@ -old +new @@` into 1-based starts and line counts (Git display form).
pub(crate) fn unified_hunk_header_counts(
    header_line: &str,
) -> Option<(usize, usize, usize, usize)> {
    let rest = header_line.strip_prefix("@@ ")?;
    let (old_part, rest2) = rest.split_once(" +")?;
    let old_tok = old_part.strip_prefix('-')?.split(' ').next()?;
    let new_tok = rest2.split(" @@").next()?.split(' ').next()?;

    fn parse_side(spec: &str) -> (usize, usize) {
        if let Some((start, count)) = spec.split_once(',') {
            (start.parse().unwrap_or(0), count.parse().unwrap_or(0))
        } else {
            (spec.parse().unwrap_or(0), 1)
        }
    }

    let (os, ol) = parse_side(old_tok);
    let (ns, nl) = parse_side(new_tok);
    Some((os, ol, ns, nl))
}

/// Write `---` / `+++` and unified hunks using [`unified_hunk_body_for_diff_options`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_unified_diff_from_diff_options<W: std::io::Write>(
    out: &mut W,
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    options: &crate::diff::options::DiffOptions,
    quote_path_fully: bool,
) -> Result<()> {
    use crate::quote_path::format_diff_path_with_prefix;

    if old_path == "/dev/null" {
        out.write_all(b"--- /dev/null\n")?;
    } else if src_prefix.is_empty() {
        writeln!(out, "--- {old_path}")?;
    } else {
        out.write_all(b"--- ")?;
        out.write_all(
            format_diff_path_with_prefix(src_prefix, old_path, quote_path_fully).as_bytes(),
        )?;
        out.write_all(b"\n")?;
    }
    if new_path == "/dev/null" {
        out.write_all(b"+++ /dev/null\n")?;
    } else if dst_prefix.is_empty() {
        writeln!(out, "+++ {new_path}")?;
    } else {
        out.write_all(b"+++ ")?;
        out.write_all(
            format_diff_path_with_prefix(dst_prefix, new_path, quote_path_fully).as_bytes(),
        )?;
        out.write_all(b"\n")?;
    }

    let body = unified_hunk_body_for_diff_options(old_content, new_content, options);
    if body.is_empty() {
        return Ok(());
    }
    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher, Some(old_path))
            {
                out.write_all(header_line.as_bytes())?;
                out.write_all(b" ")?;
                out.write_all(func_ctx.as_bytes())?;
                out.write_all(rest.as_bytes())?;
            } else {
                out.write_all(hunk_str.as_bytes())?;
            }
        } else {
            out.write_all(hunk_str.as_bytes())?;
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn unified_diff_git_myers_with_prefix_and_funcname(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;

    let body = git_xdiff_unified_body_raw(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        indent_heuristic,
        false,
        super::WhitespaceMode::default(),
    );

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher, Some(old_path))
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(hunk_str);
            }
        } else {
            output.push_str(hunk_str);
        }
    }

    annotate_unified_diff_no_newline_at_eof(&output, old_content, new_content)
}

#[expect(clippy::too_many_arguments)]
fn unified_diff_git_minimal_with_prefix_and_funcname(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;

    let body = git_xdiff_unified_body_raw(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        indent_heuristic,
        true,
        super::WhitespaceMode::default(),
    );

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher, Some(old_path))
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(hunk_str);
            }
        } else {
            output.push_str(hunk_str);
        }
    }

    annotate_unified_diff_no_newline_at_eof(&output, old_content, new_content)
}

#[expect(clippy::too_many_arguments)]
fn unified_diff_git_patience_with_prefix_and_funcname(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;

    let body = git_patience_unified_body_raw(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        indent_heuristic,
    );

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher, Some(old_path))
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(hunk_str);
            }
        } else {
            output.push_str(hunk_str);
        }
    }

    annotate_unified_diff_no_newline_at_eof(&output, old_content, new_content)
}

/// Unified diff hunks for Git's histogram algorithm (no `---` / `+++` lines).
///
/// Used by `--no-index` when whitespace normalization is off so the patch matches upstream Git.
#[must_use]
pub fn unified_diff_histogram_hunks_only(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    indent_heuristic: bool,
) -> String {
    histogram_unified_body_raw(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        indent_heuristic,
        super::WhitespaceMode::default(),
    )
}

#[expect(clippy::too_many_arguments)]
fn unified_diff_histogram_with_prefix_and_funcname_indent(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;

    let body = histogram_unified_body_raw(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        indent_heuristic,
        super::WhitespaceMode::default(),
    );

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher, Some(old_path))
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(hunk_str);
            }
        } else {
            output.push_str(hunk_str);
        }
    }

    annotate_unified_diff_no_newline_at_eof(&output, old_content, new_content)
}

/// Full unified diff (`---` / `+++` / hunks) using Git's histogram algorithm,
/// applying `--ignore-blank-lines` / `-I` change-record suppression
/// (`histogram_unified_body_ignore_fc`) and then attaching function-name hunk
/// headers like `unified_diff_histogram_with_prefix_and_funcname_indent`.
///
/// `is_ignorable_change` is given the removed/added line slices (without
/// trailing newlines) of one change record and returns whether the whole change
/// is ignorable. Returns an empty string when no substantive change survives so
/// the caller can hide the file entry.
#[allow(clippy::too_many_arguments)]
pub fn unified_diff_histogram_ignore_with_prefix_and_funcname<F>(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    quote_path_fully: bool,
    function_context: bool,
    indent_heuristic: bool,
    is_ignorable_change: F,
) -> String
where
    F: Fn(&[&str], &[&str]) -> bool,
{
    use crate::quote_path::format_diff_path_with_prefix;

    let body = histogram_unified_body_ignore_fc(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        function_context,
        funcname_matcher,
        indent_heuristic,
        super::WhitespaceMode::default(),
        is_ignorable_change,
    );
    if body.is_empty() {
        return String::new();
    }

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher, Some(old_path))
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(hunk_str);
            }
        } else {
            output.push_str(hunk_str);
        }
    }

    output
}

/// `diff.indentHeuristic` from config (Git defaults to true when unset).
#[must_use]
pub fn indent_heuristic_from_config(config: &ConfigSet) -> bool {
    match config.get_bool("diff.indentHeuristic") {
        Some(Ok(b)) => b,
        Some(Err(_)) | None => true,
    }
}

/// Resolve indent heuristic: `--no-indent-heuristic` and `--indent-heuristic` override config.
#[must_use]
pub fn resolve_indent_heuristic(
    config: &ConfigSet,
    cli_indent_heuristic: bool,
    cli_no_indent_heuristic: bool,
) -> bool {
    if cli_no_indent_heuristic {
        false
    } else if cli_indent_heuristic {
        true
    } else {
        indent_heuristic_from_config(config)
    }
}

// ---------------------------------------------------------------------------
// Git-compatible implementation of the xdiff Myers engine,
// restricted to what the word-diff path needs (no rename/ignore-whitespace
// flags). Git's word diff runs the *default* xdiff Myers over the per-word
// token streams; matching Git's exact change-record selection requires
// reproducing its preprocessing (record classification + `xdl_cleanup_records`
// + `xdl_trim_ends`) and its divide-and-conquer split, because the histogram
// engine (and `similar`) pick different — but equally minimal — alignments for inputs
// with many repeated tokens (e.g. the `{`/`}`/`,` in the `bibtex` driver).
mod git_xdiff {
    use std::cell::RefCell;

    const XDL_MAX_COST_MIN: i64 = 256;
    const XDL_HEUR_MIN_COST: i64 = 256;
    const XDL_SNAKE_CNT: i64 = 20;
    const XDL_K_HEUR: i64 = 4;
    const XDL_LINE_MAX: i64 = i64::MAX;
    const XDL_KPDIS_RUN: i64 = 4;
    const XDL_MAX_EQLIMIT: i64 = 1024;
    const XDL_SIMSCAN_WINDOW: i64 = 100;

    // record-classification actions (xprepare.c)
    const DISCARD: u8 = 0;
    const KEEP: u8 = 1;
    const INVESTIGATE: u8 = 2;

    /// Classical integer square root approximation using shifts (`xdl_bogosqrt`).
    fn bogosqrt(mut n: i64) -> i64 {
        let mut i: i64 = 1;
        while n > 0 {
            i <<= 1;
            n >>= 2;
        }
        i
    }

    struct XdAlgoEnv {
        mxcost: i64,
        snake_cnt: i64,
        heur_min: i64,
    }

    struct XdpSplit {
        i1: i64,
        i2: i64,
        min_lo: bool,
        min_hi: bool,
    }

    /// A single side of the diff: token ids plus the `changed` flags and the
    /// `reference_index` mapping from effective record -> original record.
    struct XdFile<'a> {
        #[allow(dead_code)]
        ids: &'a [u32],
        changed: Vec<u8>,
        reference_index: Vec<usize>,
        /// Interned token id per effective record (parallel to `reference_index`).
        effective_ids: Vec<u32>,
        dstart: i64,
        dend: i64,
        nreff: i64,
    }

    #[inline]
    fn effective_id(xdf: &XdFile<'_>, idx: i64) -> u32 {
        xdf.effective_ids[idx as usize]
    }

    struct XdiffScratch {
        count1: Vec<i64>,
        count2: Vec<i64>,
        action1: Vec<u8>,
        action2: Vec<u8>,
        kvd: Vec<i64>,
    }

    impl XdiffScratch {
        fn prepare_counts(&mut self, nclasses: usize) {
            if self.count1.len() < nclasses {
                self.count1.resize(nclasses, 0);
                self.count2.resize(nclasses, 0);
            }
            self.count1[..nclasses].fill(0);
            self.count2[..nclasses].fill(0);
        }

        fn prepare_actions(&mut self, nrec1: usize, nrec2: usize) {
            let a1 = nrec1 + 1;
            let a2 = nrec2 + 1;
            if self.action1.len() < a1 {
                self.action1.resize(a1, 0);
            } else {
                self.action1[..a1].fill(0);
            }
            if self.action2.len() < a2 {
                self.action2.resize(a2, 0);
            } else {
                self.action2[..a2].fill(0);
            }
        }

        fn prepare_kvd(&mut self, kvd_len: usize) {
            if self.kvd.len() < kvd_len {
                self.kvd.resize(kvd_len, 0);
            }
        }
    }

    thread_local! { // hygiene: per-thread xdiff Myers scratch (class counts, K-vectors) reused across files
        static XDIFF_SCRATCH: RefCell<XdiffScratch> = const {
            RefCell::new(XdiffScratch {
                count1: Vec::new(),
                count2: Vec::new(),
                action1: Vec::new(),
                action2: Vec::new(),
                kvd: Vec::new(),
            })
        };
    }

    fn bool_changed(changed: Vec<u8>) -> Vec<bool> {
        changed.into_iter().map(|b| b != 0).collect()
    }

    /// Faithful port of `xdl_split` (the middle-snake finder). `kvdf`/`kvdb` are
    /// the forward/backward k-vectors, offset so index 0 maps to diagonal 0.
    #[allow(clippy::too_many_arguments)]
    fn xdl_split(
        xdf1: &XdFile<'_>,
        off1: i64,
        lim1: i64,
        xdf2: &XdFile<'_>,
        off2: i64,
        lim2: i64,
        kvdf: &mut [i64],
        kvdb: &mut [i64],
        koff: i64,
        need_min: bool,
        spl: &mut XdpSplit,
        xenv: &XdAlgoEnv,
    ) -> i64 {
        let dmin = off1 - lim2;
        let dmax = lim1 - off2;
        let fmid = off1 - off2;
        let bmid = lim1 - lim2;
        let odd = ((fmid - bmid) & 1) != 0;
        let mut fmin = fmid;
        let mut fmax = fmid;
        let mut bmin = bmid;
        let mut bmax = bmid;

        let kf = |k: i64| -> usize { (k + koff) as usize };

        kvdf[kf(fmid)] = off1;
        kvdb[kf(bmid)] = lim1;

        let mut ec: i64 = 1;
        loop {
            let mut got_snake = false;

            if fmin > dmin {
                fmin -= 1;
                kvdf[kf(fmin - 1)] = -1;
            } else {
                fmin += 1;
            }
            if fmax < dmax {
                fmax += 1;
                kvdf[kf(fmax + 1)] = -1;
            } else {
                fmax -= 1;
            }

            let mut d = fmax;
            while d >= fmin {
                let mut i1 = if kvdf[kf(d - 1)] >= kvdf[kf(d + 1)] {
                    kvdf[kf(d - 1)] + 1
                } else {
                    kvdf[kf(d + 1)]
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 < lim1 && i2 < lim2 && effective_id(xdf1, i1) == effective_id(xdf2, i2) {
                    i1 += 1;
                    i2 += 1;
                }
                if i1 - prev1 > xenv.snake_cnt {
                    got_snake = true;
                }
                kvdf[kf(d)] = i1;
                if odd && bmin <= d && d <= bmax && kvdb[kf(d)] <= i1 {
                    spl.i1 = i1;
                    spl.i2 = i2;
                    spl.min_lo = true;
                    spl.min_hi = true;
                    return ec;
                }
                d -= 2;
            }

            if bmin > dmin {
                bmin -= 1;
                kvdb[kf(bmin - 1)] = XDL_LINE_MAX;
            } else {
                bmin += 1;
            }
            if bmax < dmax {
                bmax += 1;
                kvdb[kf(bmax + 1)] = XDL_LINE_MAX;
            } else {
                bmax -= 1;
            }

            let mut d = bmax;
            while d >= bmin {
                let mut i1 = if kvdb[kf(d - 1)] < kvdb[kf(d + 1)] {
                    kvdb[kf(d - 1)]
                } else {
                    kvdb[kf(d + 1)] - 1
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 > off1
                    && i2 > off2
                    && effective_id(xdf1, i1 - 1) == effective_id(xdf2, i2 - 1)
                {
                    i1 -= 1;
                    i2 -= 1;
                }
                if prev1 - i1 > xenv.snake_cnt {
                    got_snake = true;
                }
                kvdb[kf(d)] = i1;
                if !odd && fmin <= d && d <= fmax && i1 <= kvdf[kf(d)] {
                    spl.i1 = i1;
                    spl.i2 = i2;
                    spl.min_lo = true;
                    spl.min_hi = true;
                    return ec;
                }
                d -= 2;
            }

            if need_min {
                ec += 1;
                continue;
            }

            if got_snake && ec > xenv.heur_min {
                let mut best = 0i64;
                let mut d = fmax;
                while d >= fmin {
                    let dd = if d > fmid { d - fmid } else { fmid - d };
                    let i1 = kvdf[kf(d)];
                    let i2 = i1 - d;
                    let v = (i1 - off1) + (i2 - off2) - dd;
                    if v > XDL_K_HEUR * ec
                        && v > best
                        && off1 + xenv.snake_cnt <= i1
                        && i1 < lim1
                        && off2 + xenv.snake_cnt <= i2
                        && i2 < lim2
                    {
                        let mut k = 1i64;
                        while effective_id(xdf1, i1 - k) == effective_id(xdf2, i2 - k) {
                            if k == xenv.snake_cnt {
                                best = v;
                                spl.i1 = i1;
                                spl.i2 = i2;
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if best > 0 {
                    spl.min_lo = true;
                    spl.min_hi = false;
                    return ec;
                }

                let mut best = 0i64;
                let mut d = bmax;
                while d >= bmin {
                    let dd = if d > bmid { d - bmid } else { bmid - d };
                    let i1 = kvdb[kf(d)];
                    let i2 = i1 - d;
                    let v = (lim1 - i1) + (lim2 - i2) - dd;
                    if v > XDL_K_HEUR * ec
                        && v > best
                        && off1 < i1
                        && i1 <= lim1 - xenv.snake_cnt
                        && off2 < i2
                        && i2 <= lim2 - xenv.snake_cnt
                    {
                        let mut k = 0i64;
                        while effective_id(xdf1, i1 + k) == effective_id(xdf2, i2 + k) {
                            if k == xenv.snake_cnt - 1 {
                                best = v;
                                spl.i1 = i1;
                                spl.i2 = i2;
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if best > 0 {
                    spl.min_lo = false;
                    spl.min_hi = true;
                    return ec;
                }
            }

            if ec >= xenv.mxcost {
                let mut fbest = -1i64;
                let mut fbest1 = -1i64;
                let mut d = fmax;
                while d >= fmin {
                    let mut i1 = kvdf[kf(d)].min(lim1);
                    let mut i2 = i1 - d;
                    if lim2 < i2 {
                        i1 = lim2 + d;
                        i2 = lim2;
                    }
                    if fbest < i1 + i2 {
                        fbest = i1 + i2;
                        fbest1 = i1;
                    }
                    d -= 2;
                }

                let mut bbest = XDL_LINE_MAX;
                let mut bbest1 = XDL_LINE_MAX;
                let mut d = bmax;
                while d >= bmin {
                    let mut i1 = off1.max(kvdb[kf(d)]);
                    let mut i2 = i1 - d;
                    if i2 < off2 {
                        i1 = off2 + d;
                        i2 = off2;
                    }
                    if i1 + i2 < bbest {
                        bbest = i1 + i2;
                        bbest1 = i1;
                    }
                    d -= 2;
                }

                if (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                    spl.i1 = fbest1;
                    spl.i2 = fbest - fbest1;
                    spl.min_lo = true;
                    spl.min_hi = false;
                } else {
                    spl.i1 = bbest1;
                    spl.i2 = bbest - bbest1;
                    spl.min_lo = false;
                    spl.min_hi = true;
                }
                return ec;
            }

            ec += 1;
        }
    }

    /// Faithful port of `xdl_recs_cmp` (divide & conquer). Marks `changed` flags.
    #[allow(clippy::too_many_arguments)]
    fn xdl_recs_cmp(
        xdf1: &mut XdFile<'_>,
        mut off1: i64,
        mut lim1: i64,
        xdf2: &mut XdFile<'_>,
        mut off2: i64,
        mut lim2: i64,
        kvdf: &mut [i64],
        kvdb: &mut [i64],
        koff: i64,
        need_min: bool,
        xenv: &XdAlgoEnv,
    ) {
        while off1 < lim1 && off2 < lim2 && effective_id(xdf1, off1) == effective_id(xdf2, off2) {
            off1 += 1;
            off2 += 1;
        }
        while off1 < lim1
            && off2 < lim2
            && effective_id(xdf1, lim1 - 1) == effective_id(xdf2, lim2 - 1)
        {
            lim1 -= 1;
            lim2 -= 1;
        }

        if off1 == lim1 {
            while off2 < lim2 {
                let r = xdf2.reference_index[off2 as usize];
                xdf2.changed[r] = 1;
                off2 += 1;
            }
        } else if off2 == lim2 {
            while off1 < lim1 {
                let r = xdf1.reference_index[off1 as usize];
                xdf1.changed[r] = 1;
                off1 += 1;
            }
        } else {
            let mut spl = XdpSplit {
                i1: 0,
                i2: 0,
                min_lo: false,
                min_hi: false,
            };
            xdl_split(
                xdf1, off1, lim1, xdf2, off2, lim2, kvdf, kvdb, koff, need_min, &mut spl, xenv,
            );
            xdl_recs_cmp(
                xdf1, off1, spl.i1, xdf2, off2, spl.i2, kvdf, kvdb, koff, spl.min_lo, xenv,
            );
            xdl_recs_cmp(
                xdf1, spl.i1, lim1, xdf2, spl.i2, lim2, kvdf, kvdb, koff, spl.min_hi, xenv,
            );
        }
    }

    /// `xdl_clean_mmatch`: decide whether a multimatch record should be discarded.
    fn clean_mmatch(action: &[u8], i: i64, mut s: i64, mut e: i64) -> bool {
        if i - s > XDL_SIMSCAN_WINDOW {
            s = i - XDL_SIMSCAN_WINDOW;
        }
        if e - i > XDL_SIMSCAN_WINDOW {
            e = i + XDL_SIMSCAN_WINDOW;
        }

        let mut rdis0 = 0i64;
        let mut rpdis0 = 1i64;
        let mut r = 1i64;
        while i - r >= s {
            match action[(i - r) as usize] {
                DISCARD => rdis0 += 1,
                INVESTIGATE => rpdis0 += 1,
                _ => break, // KEEP
            }
            r += 1;
        }
        if rdis0 == 0 {
            return false;
        }
        let mut rdis1 = 0i64;
        let mut rpdis1 = 1i64;
        let mut r = 1i64;
        while i + r <= e {
            match action[(i + r) as usize] {
                DISCARD => rdis1 += 1,
                INVESTIGATE => rpdis1 += 1,
                _ => break, // KEEP
            }
            r += 1;
        }
        if rdis1 == 0 {
            return false;
        }
        rdis1 += rdis0;
        rpdis1 += rpdis0;
        rpdis1 * XDL_KPDIS_RUN < (rpdis1 + rdis1)
    }

    /// Build the `changed` flags for both token streams using Git's xdiff engine.
    /// When `need_min` is true, Git's minimal diff (`--minimal`) is used.
    /// `ids1`/`ids2` are dense interned token ids from one [`crate::line_interner::LineInterner`]
    /// (equal id <=> equal token); per-id tables are sized by the largest id.
    /// Returns `(changed1, changed2)`, one bool per original token.
    pub fn changed_flags(ids1: &[u32], ids2: &[u32], need_min: bool) -> (Vec<bool>, Vec<bool>) {
        changed_flags_with_shared(ids1, ids2, &[], 0, need_min)
    }

    /// Like [`changed_flags`] for slices cut from two larger files whose other records are
    /// identical on both sides.
    ///
    /// Git classifies each record by how often it occurs in the whole other file, so the
    /// records left out still count: `shared_counts[id]` is how many of them have token `id`
    /// (ids past its end have none), and `shared_records` is how many there are on each side.
    pub fn changed_flags_with_shared(
        ids1: &[u32],
        ids2: &[u32],
        shared_counts: &[u32],
        shared_records: usize,
        need_min: bool,
    ) -> (Vec<bool>, Vec<bool>) {
        XDIFF_SCRATCH.with(|cell| {
            changed_flags_with_shared_inner(
                cell,
                ids1,
                ids2,
                shared_counts,
                shared_records,
                need_min,
            )
        })
    }

    fn changed_flags_with_shared_inner(
        scratch: &RefCell<XdiffScratch>,
        ids1: &[u32],
        ids2: &[u32],
        shared_counts: &[u32],
        shared_records: usize,
        need_min: bool,
    ) -> (Vec<bool>, Vec<bool>) {
        let nrec1 = ids1.len();
        let nrec2 = ids2.len();

        // Per-token occurrence counts on each side (class len1/len2), indexed by the dense ids.
        let nclasses = ids1
            .iter()
            .chain(ids2)
            .max()
            .map_or(0, |&max| max as usize + 1);
        let mut work = scratch.borrow_mut();
        work.prepare_counts(nclasses);
        for &id in ids1 {
            work.count1[id as usize] += 1;
        }
        for &id in ids2 {
            work.count2[id as usize] += 1;
        }
        for (id, &shared) in shared_counts.iter().enumerate().take(nclasses) {
            work.count1[id] += i64::from(shared);
            work.count2[id] += i64::from(shared);
        }

        let mut xdf1 = XdFile {
            ids: ids1,
            changed: vec![0u8; nrec1],
            reference_index: Vec::new(),
            effective_ids: Vec::new(),
            dstart: 0,
            dend: nrec1 as i64 - 1,
            nreff: 0,
        };
        let mut xdf2 = XdFile {
            ids: ids2,
            changed: vec![0u8; nrec2],
            reference_index: Vec::new(),
            effective_ids: Vec::new(),
            dstart: 0,
            dend: nrec2 as i64 - 1,
            nreff: 0,
        };

        // xdl_trim_ends: trim leading/trailing matching records.
        let lim = nrec1.min(nrec2) as i64;
        let mut i = 0i64;
        while i < lim && ids1[i as usize] == ids2[i as usize] {
            i += 1;
        }
        xdf1.dstart = i;
        xdf2.dstart = i;
        let mut j = 0i64;
        let rem = lim - i;
        while j < rem && ids1[nrec1 - 1 - j as usize] == ids2[nrec2 - 1 - j as usize] {
            j += 1;
        }
        xdf1.dend = nrec1 as i64 - j - 1;
        xdf2.dend = nrec2 as i64 - j - 1;

        // xdl_cleanup_records: classify and reduce to effective records.
        work.prepare_actions(nrec1, nrec2);

        let mlim1 = if need_min {
            i64::MAX
        } else {
            let mut mlim = bogosqrt((nrec1 + shared_records) as i64);
            if mlim > XDL_MAX_EQLIMIT {
                mlim = XDL_MAX_EQLIMIT;
            }
            mlim
        };
        let mut idx = xdf1.dstart;
        while idx <= xdf1.dend {
            let id = ids1[idx as usize];
            let nm = work.count2[id as usize];
            work.action1[idx as usize] = if nm == 0 {
                DISCARD
            } else if nm >= mlim1 {
                INVESTIGATE
            } else {
                KEEP
            };
            idx += 1;
        }

        let mlim2 = if need_min {
            i64::MAX
        } else {
            let mut mlim = bogosqrt((nrec2 + shared_records) as i64);
            if mlim > XDL_MAX_EQLIMIT {
                mlim = XDL_MAX_EQLIMIT;
            }
            mlim
        };
        let mut idx = xdf2.dstart;
        while idx <= xdf2.dend {
            let id = ids2[idx as usize];
            let nm = work.count1[id as usize];
            work.action2[idx as usize] = if nm == 0 {
                DISCARD
            } else if nm >= mlim2 {
                INVESTIGATE
            } else {
                KEEP
            };
            idx += 1;
        }

        let mut idx = xdf1.dstart;
        while idx <= xdf1.dend {
            let id = ids1[idx as usize];
            let a = work.action1[idx as usize];
            if a == KEEP
                || (a == INVESTIGATE
                    && !clean_mmatch(&work.action1[..nrec1 + 1], idx, xdf1.dstart, xdf1.dend))
            {
                xdf1.reference_index.push(idx as usize);
                xdf1.effective_ids.push(id);
            } else {
                xdf1.changed[idx as usize] = 1;
            }
            idx += 1;
        }
        xdf1.nreff = xdf1.reference_index.len() as i64;

        let mut idx = xdf2.dstart;
        while idx <= xdf2.dend {
            let id = ids2[idx as usize];
            let a = work.action2[idx as usize];
            if a == KEEP
                || (a == INVESTIGATE
                    && !clean_mmatch(&work.action2[..nrec2 + 1], idx, xdf2.dstart, xdf2.dend))
            {
                xdf2.reference_index.push(idx as usize);
                xdf2.effective_ids.push(id);
            } else {
                xdf2.changed[idx as usize] = 1;
            }
            idx += 1;
        }
        xdf2.nreff = xdf2.reference_index.len() as i64;

        // Allocate K vectors (xdl_do_diff). koff lets us use negative diagonals.
        let ndiags = xdf1.nreff + xdf2.nreff + 3;
        let kvd_len = (2 * ndiags + 2) as usize;
        work.prepare_kvd(kvd_len);
        let kvd = &mut work.kvd[..kvd_len];
        // kvdf base offset = nreff2 + 1; kvdb base offset = ndiags + nreff2 + 1.
        let koff = xdf2.nreff + 1;
        let (kvdf_slice, kvdb_slice) = kvd.split_at_mut(ndiags as usize);
        // Both slices are indexed as [k + koff]; their length covers the diagonal range.

        let xenv = XdAlgoEnv {
            mxcost: bogosqrt(ndiags).max(XDL_MAX_COST_MIN),
            snake_cnt: XDL_SNAKE_CNT,
            heur_min: XDL_HEUR_MIN_COST,
        };

        let nreff1 = xdf1.nreff;
        let nreff2 = xdf2.nreff;
        xdl_recs_cmp(
            &mut xdf1, 0, nreff1, &mut xdf2, 0, nreff2, kvdf_slice, kvdb_slice, koff, need_min,
            &xenv,
        );

        (bool_changed(xdf1.changed), bool_changed(xdf2.changed))
    }

    /// Patience diff: match unique lines, then recurse on gaps with xdiff Myers.
    pub fn patience_changed_flags(lines1: &[&str], lines2: &[&str]) -> (Vec<bool>, Vec<bool>) {
        let mut changed1 = vec![false; lines1.len()];
        let mut changed2 = vec![false; lines2.len()];
        patience_recurse(
            lines1,
            lines2,
            &mut changed1,
            &mut changed2,
            0,
            lines1.len(),
            0,
            lines2.len(),
        );
        (changed1, changed2)
    }

    #[allow(clippy::too_many_arguments)]
    fn patience_recurse(
        lines1: &[&str],
        lines2: &[&str],
        changed1: &mut [bool],
        changed2: &mut [bool],
        o_start: usize,
        o_end: usize,
        n_start: usize,
        n_end: usize,
    ) {
        if o_start >= o_end && n_start >= n_end {
            return;
        }
        if o_start >= o_end {
            changed2[n_start..n_end].fill(true);
            return;
        }
        if n_start >= n_end {
            changed1[o_start..o_end].fill(true);
            return;
        }

        let anchors = patience_anchors(lines1, lines2, o_start, o_end, n_start, n_end);
        if anchors.is_empty() {
            merge_myers_region(
                lines1, lines2, changed1, changed2, o_start, o_end, n_start, n_end,
            );
            return;
        }

        let mut prev_o = o_start;
        let mut prev_n = n_start;
        for (ao, an) in anchors {
            patience_recurse(lines1, lines2, changed1, changed2, prev_o, ao, prev_n, an);
            prev_o = ao + 1;
            prev_n = an + 1;
        }
        patience_recurse(
            lines1, lines2, changed1, changed2, prev_o, o_end, prev_n, n_end,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn patience_anchors(
        lines1: &[&str],
        lines2: &[&str],
        o_start: usize,
        o_end: usize,
        n_start: usize,
        n_end: usize,
    ) -> Vec<(usize, usize)> {
        use std::collections::HashMap;

        let old_slice = &lines1[o_start..o_end];
        let new_slice = &lines2[n_start..n_end];
        let mut old_pos: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut new_pos: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, line) in old_slice.iter().enumerate() {
            old_pos.entry(*line).or_default().push(o_start + i);
        }
        for (i, line) in new_slice.iter().enumerate() {
            new_pos.entry(*line).or_default().push(n_start + i);
        }

        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for (line, o_indices) in &old_pos {
            if o_indices.len() != 1 {
                continue;
            }
            if let Some(n_indices) = new_pos.get(line) {
                if n_indices.len() == 1 {
                    pairs.push((o_indices[0], n_indices[0]));
                }
            }
        }
        pairs.sort_by_key(|p| p.0);
        longest_increasing_by_second(&pairs)
    }

    fn longest_increasing_by_second(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
        if pairs.is_empty() {
            return Vec::new();
        }
        let n = pairs.len();
        let mut tails = vec![0usize; n];
        let mut prev = vec![usize::MAX; n];
        let mut len = 0usize;
        for i in 0..n {
            let v = pairs[i].1;
            let mut lo = 0usize;
            let mut hi = len;
            while lo < hi {
                let mid = (lo + hi) / 2;
                if pairs[tails[mid]].1 < v {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if lo > 0 {
                prev[i] = tails[lo - 1];
            }
            tails[lo] = i;
            if lo == len {
                len += 1;
            }
        }
        let mut out = Vec::with_capacity(len);
        let mut k = tails[len.saturating_sub(1)];
        while k != usize::MAX {
            out.push(pairs[k]);
            k = prev[k];
        }
        out.reverse();
        out
    }

    pub(crate) fn histogram_changed_flags(
        lines1: &[&str],
        lines2: &[&str],
    ) -> (Vec<bool>, Vec<bool>) {
        crate::diff::git_histogram::histogram_changed_flags(
            lines1,
            lines2,
            |changed1, changed2, o_start, o_end, n_start, n_end| {
                merge_myers_region(
                    lines1, lines2, changed1, changed2, o_start, o_end, n_start, n_end,
                );
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_myers_region(
        lines1: &[&str],
        lines2: &[&str],
        changed1: &mut [bool],
        changed2: &mut [bool],
        o_start: usize,
        o_end: usize,
        n_start: usize,
        n_end: usize,
    ) {
        use crate::line_interner::LineInterner;

        let old_slice = &lines1[o_start..o_end];
        let new_slice = &lines2[n_start..n_end];
        if old_slice.is_empty() && new_slice.is_empty() {
            return;
        }
        let mut interner = LineInterner::with_capacity(old_slice.len() + new_slice.len());
        let ids1 = interner.intern_all(old_slice);
        let ids2 = interner.intern_all(new_slice);
        let (sub1, sub2) = changed_flags(&ids1, &ids2, false);
        for (i, ch) in sub1.iter().enumerate() {
            if *ch {
                changed1[o_start + i] = true;
            }
        }
        for (j, ch) in sub2.iter().enumerate() {
            if *ch {
                changed2[n_start + j] = true;
            }
        }
    }
}

/// Myers changed-line masks for a line slice, honoring [`super::WhitespaceMode`] like Git `xdl_fall_back_diff`.
pub(crate) fn myers_rediff_changed_masks_for_region<'a>(
    old_lines: &[&'a str],
    new_lines: &[&'a str],
    ws: super::WhitespaceMode,
) -> (Vec<bool>, Vec<bool>) {
    use crate::line_interner::LineInterner;

    if old_lines.is_empty() && new_lines.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let normalize = |lines: &[&'a str]| -> Vec<Cow<'a, str>> {
        lines
            .iter()
            .map(|line| ws.normalize_line_for_compare(line))
            .collect()
    };
    let (old_normalized, new_normalized) = (normalize(old_lines), normalize(new_lines));
    let old_keys: Vec<&str> = old_normalized.iter().map(Cow::as_ref).collect();
    let new_keys: Vec<&str> = new_normalized.iter().map(Cow::as_ref).collect();
    let mut interner = LineInterner::with_capacity(old_keys.len() + new_keys.len());
    let ids1 = interner.intern_all(&old_keys);
    let ids2 = interner.intern_all(&new_keys);
    git_xdiff::changed_flags(&ids1, &ids2, false)
}

/// Convert per-token `changed` flags (Git xdiff style) into `similar::DiffOp`s.
fn changed_flags_to_ops(
    changed1: &[bool],
    changed2: &[bool],
    old_len: usize,
    new_len: usize,
) -> Vec<similar::DiffOp> {
    use similar::DiffOp;
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_len || j < new_len {
        let del = i < old_len && changed1[i];
        let ins = j < new_len && changed2[j];
        if !del && !ins {
            if i < old_len && j < new_len {
                // Equal run on both sides.
                let start_i = i;
                let start_j = j;
                while i < old_len && j < new_len && !changed1[i] && !changed2[j] {
                    i += 1;
                    j += 1;
                }
                ops.push(DiffOp::Equal {
                    old_index: start_i,
                    new_index: start_j,
                    len: i - start_i,
                });
            } else if i < old_len {
                let start_i = i;
                while i < old_len {
                    i += 1;
                }
                ops.push(DiffOp::Delete {
                    old_index: start_i,
                    old_len: i - start_i,
                    new_index: j,
                });
            } else if j < new_len {
                let start_j = j;
                while j < new_len {
                    j += 1;
                }
                ops.push(DiffOp::Insert {
                    old_index: i,
                    new_index: start_j,
                    new_len: j - start_j,
                });
            }
        } else {
            // Changed run: consecutive deletions then/and insertions.
            let start_i = i;
            let start_j = j;
            while i < old_len && changed1[i] {
                i += 1;
            }
            while j < new_len && changed2[j] {
                j += 1;
            }
            let dlen = i - start_i;
            let ilen = j - start_j;
            if dlen > 0 && ilen > 0 {
                ops.push(DiffOp::Replace {
                    old_index: start_i,
                    old_len: dlen,
                    new_index: start_j,
                    new_len: ilen,
                });
            } else if dlen > 0 {
                ops.push(DiffOp::Delete {
                    old_index: start_i,
                    old_len: dlen,
                    new_index: start_j,
                });
            } else if ilen > 0 {
                ops.push(DiffOp::Insert {
                    old_index: start_i,
                    new_index: start_j,
                    new_len: ilen,
                });
            }
        }
    }
    ops
}

/// Line-diff ops for string slices after Git `xdl_change_compact` (and optional indent heuristic).
#[must_use]
pub fn diff_slice_ops_compacted(
    old_lines: &[&str],
    new_lines: &[&str],
    algorithm: similar::Algorithm,
    indent_heuristic: bool,
) -> Vec<similar::DiffOp> {
    diff_indent_heuristic::diff_slice_ops_compacted(
        old_lines,
        new_lines,
        algorithm,
        indent_heuristic,
    )
}

/// Compacted line-diff ops from Git's xdiff Myers engine over precomputed line ids for each side
/// (default blame path; faster than `similar` on large files).
#[must_use]
///
/// `shared_counts` and `shared_records` describe identical records of both files left out of
/// the slices, as for [`git_xdiff::changed_flags_with_shared`]; pass `&[]` and `0` for whole files.
#[allow(clippy::too_many_arguments)]
pub(crate) fn line_diff_ops_git_xdiff_compacted_ids(
    old_ids: &[u32],
    new_ids: &[u32],
    shared_counts: &[u32],
    shared_records: usize,
    old_lines: &[&str],
    new_lines: &[&str],
    indent_heuristic: bool,
    ws: super::WhitespaceMode,
) -> Vec<similar::DiffOp> {
    let (changed1, changed2) = git_xdiff::changed_flags_with_shared(
        old_ids,
        new_ids,
        shared_counts,
        shared_records,
        false,
    );
    let ops = changed_flags_to_ops(&changed1, &changed2, old_lines.len(), new_lines.len());
    diff_indent_heuristic::apply_change_compact_to_ops_with_ws(
        &ops,
        old_lines,
        new_lines,
        indent_heuristic,
        ws,
    )
}

/// Whether each new-side line is marked changed by Git's xdiff engine (after change compaction).
#[cfg(test)]
#[must_use]
pub(crate) fn xdiff_new_line_changed_flags(
    old_lines: &[&str],
    new_lines: &[&str],
    indent_heuristic: bool,
) -> Vec<bool> {
    let mut interner =
        crate::line_interner::LineInterner::with_capacity(old_lines.len() + new_lines.len());
    let ids1 = interner.intern_all(old_lines);
    let ids2 = interner.intern_all(new_lines);
    let (changed1, changed2) = git_xdiff::changed_flags(&ids1, &ids2, false);
    let ops = changed_flags_to_ops(&changed1, &changed2, old_lines.len(), new_lines.len());
    let compacted = diff_indent_heuristic::apply_change_compact_to_ops(
        &ops,
        old_lines,
        new_lines,
        indent_heuristic,
    );
    let mut flags = vec![false; new_lines.len()];
    for op in compacted {
        match op.tag() {
            similar::DiffTag::Insert | similar::DiffTag::Replace => {
                for idx in op.new_range() {
                    if idx < flags.len() {
                        flags[idx] = true;
                    }
                }
            }
            similar::DiffTag::Equal | similar::DiffTag::Delete => {}
        }
    }
    flags
}

/// The kind of change between two sides of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum DiffStatus {
    /// File was added.
    Added,
    /// File was deleted.
    Deleted,
    /// File was modified (content or mode change).
    Modified,
    /// File was renamed (with optional content change).
    Renamed,
    /// File was copied.
    Copied,
    /// File type changed (e.g. regular → symlink).
    TypeChanged,
    /// Unmerged (conflict).
    Unmerged,
}

impl DiffStatus {
    /// Single-character status letter used in raw diff output.
    #[must_use]
    pub fn letter(&self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::TypeChanged => 'T',
            Self::Unmerged => 'U',
        }
    }

    /// Status for a path present on both sides with differing content or mode.
    ///
    /// Returns [`DiffStatus::TypeChanged`] when the object type bits of the two
    /// modes differ (regular file, symlink, gitlink) and [`DiffStatus::Modified`]
    /// otherwise, so a permission-only change stays `Modified`. Every diff mode
    /// (tree, index, work tree) shares this rule.
    #[must_use]
    pub(crate) fn for_content_change(old_mode: u32, new_mode: u32) -> Self {
        if old_mode & 0o170000 != new_mode & 0o170000 {
            Self::TypeChanged
        } else {
            Self::Modified
        }
    }
}

/// A single diff entry representing one changed path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct DiffEntry {
    /// The status of this change.
    pub status: DiffStatus,
    /// Path in the "old" side (None for Added).
    pub old_path: Option<String>,
    /// Path in the "new" side (None for Deleted).
    pub new_path: Option<String>,
    /// Old file mode ([`FileMode::ABSENT`] for Added).
    pub old_mode: FileMode,
    /// New file mode ([`FileMode::ABSENT`] for Deleted).
    pub new_mode: FileMode,
    /// Old object ID (zero OID for Added).
    pub old_oid: ObjectId,
    /// New object ID (zero OID for Deleted, or for Modified index↔worktree / tree↔worktree
    /// entries whose content lives only on disk — raw output uses the null OID on that side).
    pub new_oid: ObjectId,
    /// Similarity score (0–100) for renames/copies.
    pub score: Option<u32>,
}

impl DiffEntry {
    /// The primary path for display (new_path for adds, old_path for deletes).
    #[must_use]
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    /// Return a human-oriented path display for this entry.
    ///
    /// For renames and copies this returns `old -> new`; for all other entry
    /// kinds this returns the primary path.
    #[must_use]
    pub fn display_path(&self) -> String {
        match self.status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let old = self.old_path.as_deref().unwrap_or("");
                let new = self.new_path.as_deref().unwrap_or("");
                if old.is_empty() || new.is_empty() {
                    self.path().to_owned()
                } else {
                    format!("{old} -> {new}")
                }
            }
            _ => self.path().to_owned(),
        }
    }
}

/// Return the zero ObjectId.
#[must_use]
pub fn zero_oid() -> ObjectId {
    ObjectId::from_bytes(&[0u8; 20]).unwrap_or_else(|_| {
        // This should never fail since we pass exactly 20 bytes
        panic!("internal error: failed to create zero OID");
    })
}

/// Return the ObjectId for the empty blob object.
#[must_use]
pub fn empty_blob_oid() -> ObjectId {
    ObjectId::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap_or_else(|_| {
        // This should never fail since the object ID literal is valid.
        panic!("internal error: failed to create empty blob OID");
    })
}

// ── Tree-to-tree diff ───────────────────────────────────────────────

/// Compare two trees and return the list of changed entries.
///
/// # Parameters
///
/// - `odb` — object database to read tree objects from.
/// - `old_tree_oid` — OID of the old tree (or `None` for comparison against empty).
/// - `new_tree_oid` — OID of the new tree (or `None` for comparison against empty).
/// - `prefix` — path prefix for nested tree recursion (empty string for root).
///
/// # Errors
///
/// Returns errors from object database reads.
pub fn diff_trees(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
) -> Result<Vec<DiffEntry>> {
    crate::trace::trace_span!("grit.diff.tree");
    diff_trees_opts(odb, old_tree_oid, new_tree_oid, prefix, false, None)
}

/// Like [`diff_trees`], but when `index` matches `old_tree_oid` deleted subtrees are expanded
/// from sorted index entries instead of reading every tree object from the ODB.
pub(crate) fn diff_trees_with_index(
    odb: &Odb,
    index: &Index,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
    trust_index_for_old_tree: bool,
) -> Result<Vec<DiffEntry>> {
    let hint = (trust_index_for_old_tree
        && old_tree_oid.is_some()
        && index.entries_sorted
        && !index.entries.iter().any(|e| e.stage() != 0))
    .then_some(DiffTreesWalkHint {
        index: Some(index),
        old_tree_matches: true,
        ..DiffTreesWalkHint::default()
    });
    diff_trees_opts(
        odb,
        old_tree_oid,
        new_tree_oid,
        prefix,
        false,
        hint.as_ref(),
    )
}

/// Like [`diff_trees`], but subtrees at directories `index` holds as sparse-directory rows are
/// not descended into, so no entry lies inside one. Paths of those directories themselves (a
/// blob replaced by a tree, say) are still reported.
pub(crate) fn diff_trees_outside_sparse_directories(
    odb: &Odb,
    index: &Index,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
) -> Result<Vec<DiffEntry>> {
    crate::trace::trace_span!("grit.diff.tree");
    let hint = index
        .has_sparse_directory_placeholders()
        .then_some(DiffTreesWalkHint {
            index: Some(index),
            skip_sparse_directories: true,
            ..DiffTreesWalkHint::default()
        });
    diff_trees_opts(odb, old_tree_oid, new_tree_oid, "", false, hint.as_ref())
}

/// Like [`diff_trees`], but only paths `pathspecs` select (Git's pathspec-limited tree diff).
///
/// Directories no pathspec can reach are not read. An empty list selects every path.
pub(crate) fn diff_trees_for_pathspecs(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    pathspecs: &[String],
) -> Result<Vec<DiffEntry>> {
    if pathspecs.is_empty() {
        return diff_trees(odb, old_tree_oid, new_tree_oid, "");
    }
    crate::trace::trace_span!("grit.diff.tree");
    let hint = DiffTreesWalkHint {
        pathspecs,
        ..DiffTreesWalkHint::default()
    };
    let mut entries = diff_trees_opts(odb, old_tree_oid, new_tree_oid, "", false, Some(&hint))?;
    entries.retain(|entry| {
        let path = entry.path();
        !path.is_empty() && crate::pathspec::matches_pathspec_list(path, pathspecs)
    });
    Ok(entries)
}

/// Directories a tree diff may skip, and an index that can stand in for the old tree.
#[derive(Clone, Copy, Default)]
struct DiffTreesWalkHint<'a> {
    index: Option<&'a Index>,
    /// `index` holds exactly the old tree's entries, so deleted subtrees may be listed from it.
    old_tree_matches: bool,
    /// Do not descend into directories `index` holds as sparse-directory rows.
    skip_sparse_directories: bool,
    /// When non-empty, do not descend into directories these pathspecs cannot match inside.
    pathspecs: &'a [String],
}

impl DiffTreesWalkHint<'_> {
    /// Whether the walk must not enter directory `dir` (stored form, no trailing slash).
    fn skips(&self, dir: &str) -> bool {
        if dir.is_empty() {
            return false;
        }
        if !self.pathspecs.is_empty()
            && !crate::pathspec::pathspec_entry_is_relevant(dir, true, self.pathspecs)
        {
            return true;
        }
        let Some(index) = self.index.filter(|_| self.skip_sparse_directories) else {
            return false;
        };
        let mut row = crate::repo_relpath::storage_bytes(dir).into_owned();
        row.push(b'/');
        index
            .find_entry_pos(&row, 0)
            .is_some_and(|pos| index.entries[pos].is_sparse_directory_placeholder())
    }
}

/// Count path-level differences between two trees without building a [`DiffEntry`] list.
///
/// Same semantics as [`diff_trees`], but avoids allocating diff records (issue #1064 commit hot path).
///
/// # Errors
///
/// Returns errors from object database reads.
pub fn diff_tree_path_change_count(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
) -> Result<usize> {
    if old_tree_oid == new_tree_oid {
        return Ok(0);
    }
    let old_entries = match old_tree_oid {
        Some(oid) => read_tree(odb, oid)?,
        None => Vec::new(),
    };
    let new_entries = match new_tree_oid {
        Some(oid) => read_tree(odb, oid)?,
        None => Vec::new(),
    };
    let mut count = 0usize;
    diff_tree_entries_count(odb, &old_entries, &new_entries, prefix, false, &mut count)?;
    Ok(count)
}

/// Diff two trees and pair deleted/added paths as renames when content is similar enough.
///
/// `rename_threshold` is Git's `-M` percentage (`0`–`100`, inclusive). Use
/// [`RENAME_DETECTION_DISABLED`](crate::diff::RENAME_DETECTION_DISABLED) to skip rename detection.
///
/// # Errors
///
/// Propagates errors from [`diff_trees`].
pub fn diff_trees_with_renames(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
    rename_threshold: u32,
) -> Result<Vec<DiffEntry>> {
    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    diff_trees_with_renames_and_policy(
        odb,
        old_tree_oid,
        new_tree_oid,
        prefix,
        rename_threshold,
        &config,
        &policy,
    )
}

/// Like [`diff_trees_with_renames`], but honors `index.threads` / embedder thread policy.
///
/// # Errors
///
/// Propagates errors from [`diff_trees`].
pub fn diff_trees_with_renames_and_policy(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
    rename_threshold: u32,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let mut entries = diff_trees(odb, old_tree_oid, new_tree_oid, prefix)?;
    if rename_threshold <= 100 {
        entries = detect_renames_with_policy(
            odb,
            None,
            entries,
            rename_threshold,
            config,
            thread_policy,
            None,
        )?;
    }
    Ok(entries)
}

/// Return the tree OID stored in a commit object.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when `commit_oid` is not a commit, and object read/parse
/// errors when it is missing or malformed.
pub fn tree_oid_of_commit(odb: &Odb, commit_oid: &ObjectId) -> Result<ObjectId> {
    odb.commit_tree(commit_oid).map_err(|err| match err {
        Error::UnexpectedObjectKind { .. } => {
            Error::InvalidRef(format!("invalid ref: {commit_oid} is not a commit"))
        }
        other => other,
    })
}

/// Like [`tree_oid_of_commit`], but uses the repository commit-graph cache when available.
///
/// # Errors
///
/// Returns object read/parse errors when `commit_oid` is missing or not a commit.
pub fn tree_oid_of_commit_in_repo(
    repo: &crate::repo::Repository,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    if let Some(chain) = repo.odb().commit_graph_chain() {
        if let Some(tree) = chain.graph_commit_tree(commit_oid) {
            return Ok(tree);
        }
    }
    tree_oid_of_commit(repo.odb(), commit_oid)
}

/// Diff the trees recorded at two commits (`base` is the old side, `tip` the new side).
///
/// # Errors
///
/// Propagates object read errors and tree diff failures.
pub fn diff_commits(odb: &Odb, base: &ObjectId, tip: &ObjectId) -> Result<Vec<DiffEntry>> {
    let base_tree = tree_oid_of_commit(odb, base)?;
    let tip_tree = tree_oid_of_commit(odb, tip)?;
    diff_trees(odb, Some(&base_tree), Some(&tip_tree), "")
}

/// Diff the trees at two commits and pair deleted/added paths as renames when similar enough.
///
/// `rename_threshold` is Git's `-M` percentage (`0`–`100`, inclusive). Use
/// [`RENAME_DETECTION_DISABLED`](crate::diff::RENAME_DETECTION_DISABLED) to skip rename detection.
///
/// # Errors
///
/// Propagates object read errors and tree diff failures.
pub fn diff_commits_with_renames(
    odb: &Odb,
    base: &ObjectId,
    tip: &ObjectId,
    rename_threshold: u32,
) -> Result<Vec<DiffEntry>> {
    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    diff_commits_with_renames_and_policy(odb, base, tip, rename_threshold, &config, &policy)
}

/// Like [`diff_commits_with_renames`], but honors `index.threads` / embedder thread policy.
///
/// # Errors
///
/// Propagates object read errors and tree diff failures.
pub fn diff_commits_with_renames_and_policy(
    odb: &Odb,
    base: &ObjectId,
    tip: &ObjectId,
    rename_threshold: u32,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let base_tree = tree_oid_of_commit(odb, base)?;
    let tip_tree = tree_oid_of_commit(odb, tip)?;
    diff_trees_with_renames_and_policy(
        odb,
        Some(&base_tree),
        Some(&tip_tree),
        "",
        rename_threshold,
        config,
        thread_policy,
    )
}

/// Like [`diff_trees`], but emit entries for tree objects themselves in addition
/// to their recursive contents (Git's `diff-tree -t` behavior).
///
/// # Errors
///
/// Returns errors from object database reads.
pub fn diff_trees_show_tree_entries(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
) -> Result<Vec<DiffEntry>> {
    diff_trees_opts(odb, old_tree_oid, new_tree_oid, prefix, true, None)
}

fn diff_subtrees_in_place(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
    show_trees: bool,
    index_hint: Option<&DiffTreesWalkHint<'_>>,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    if index_hint.is_some_and(|hint| hint.skips(prefix)) {
        return Ok(());
    }
    if !show_trees {
        if let (Some(a), Some(b)) = (old_tree_oid, new_tree_oid) {
            if a == b {
                return Ok(());
            }
        }
    }
    // The ODB already budgets cached trees. Keep only active recursion frames
    // here so completing siblings can release trees evicted from that cache.
    let old_tree = old_tree_oid
        .map(|oid| odb.cached_tree_for_walk(oid))
        .transpose()?;
    let new_tree = new_tree_oid
        .map(|oid| odb.cached_tree_for_walk(oid))
        .transpose()?;
    let oid_len = odb.hash_algo().len();
    diff_tree_entries_opts(
        odb,
        WalkEntries::new(old_tree.as_ref(), oid_len),
        WalkEntries::new(new_tree.as_ref(), oid_len),
        prefix,
        show_trees,
        index_hint,
        result,
    )
}

/// Entries of a tree from [`Odb::cached_tree_for_walk`] in stored order, borrowing names
/// from it.
enum WalkEntries<'a> {
    Parsed(std::slice::Iter<'a, TreeEntry>),
    Raw(TreeRefIter<'a>),
}

impl<'a> WalkEntries<'a> {
    /// Iterate `tree`, or nothing when the side is absent.
    fn new(tree: Option<&'a SessionTree>, oid_len: usize) -> Self {
        match tree {
            Some(SessionTree::Parsed(entries)) => Self::Parsed(entries.iter()),
            Some(SessionTree::Raw(data)) => Self::Raw(TreeRefIter::with_oid_len(data, oid_len)),
            None => Self::Parsed([].iter()),
        }
    }

    fn next_entry(&mut self) -> Result<Option<TreeEntryRef<'a>>> {
        match self {
            Self::Parsed(entries) => Ok(entries.next().map(|entry| TreeEntryRef {
                mode: entry.mode,
                name: &entry.name,
                oid: entry.oid,
            })),
            Self::Raw(entries) => entries.next().transpose(),
        }
    }
}

fn diff_trees_opts(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
    show_trees: bool,
    index_hint: Option<&DiffTreesWalkHint<'_>>,
) -> Result<Vec<DiffEntry>> {
    let mut result = Vec::new();
    diff_subtrees_in_place(
        odb,
        old_tree_oid,
        new_tree_oid,
        prefix,
        show_trees,
        index_hint,
        &mut result,
    )?;
    Ok(result)
}

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::rc::Rc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
thread_local! {
    static DIFF_TREE_READ_HOOK: RefCell<Option<Rc<AtomicUsize>>> = const { RefCell::new(None) };
}

/// Install a per-thread tree read counter for unit tests (cleared when `hook` is `None`).
#[cfg(test)]
pub(crate) fn set_diff_tree_read_hook_for_tests(hook: Option<Rc<AtomicUsize>>) {
    DIFF_TREE_READ_HOOK.with(|cell| *cell.borrow_mut() = hook);
}

/// Read and parse a tree object from the ODB (no session cache).
fn read_tree_from_odb(odb: &Odb, oid: &ObjectId) -> Result<Vec<TreeEntry>> {
    #[cfg(test)]
    if let Some(counter) = DIFF_TREE_READ_HOOK.with(|cell| cell.borrow().clone()) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    Ok((*odb.cached_tree_entries(oid)?).clone())
}

/// Read and parse a tree object from the ODB.
fn read_tree(odb: &Odb, oid: &ObjectId) -> Result<Vec<TreeEntry>> {
    read_tree_from_odb(odb, oid)
}

fn diff_tree_entries_count(
    odb: &Odb,
    old: &[TreeEntry],
    new: &[TreeEntry],
    prefix: &str,
    show_trees: bool,
    count: &mut usize,
) -> Result<()> {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() || ni < new.len() {
        match (old.get(oi), new.get(ni)) {
            (Some(o), Some(n)) => {
                let cmp = crate::objects::tree_entry_cmp(
                    &o.name,
                    is_tree_mode(o.mode),
                    &n.name,
                    is_tree_mode(n.mode),
                );
                match cmp {
                    std::cmp::Ordering::Less => {
                        emit_deleted_count(odb, o, prefix, show_trees, count)?;
                        oi += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        emit_added_count(odb, n, prefix, show_trees, count)?;
                        ni += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if o.oid != n.oid
                            || crate::index::canon_mode(o.mode) != crate::index::canon_mode(n.mode)
                        {
                            let path = crate::repo_relpath::format_path_stored(prefix, &o.name);
                            if is_tree_mode(o.mode) && is_tree_mode(n.mode) {
                                if show_trees {
                                    *count += 1;
                                }
                                *count += diff_tree_path_change_count(
                                    odb,
                                    Some(&o.oid),
                                    Some(&n.oid),
                                    &path,
                                )?;
                            } else if is_tree_mode(o.mode) != is_tree_mode(n.mode) {
                                emit_deleted_count(odb, o, prefix, show_trees, count)?;
                                emit_added_count(odb, n, prefix, show_trees, count)?;
                            } else {
                                *count += 1;
                            }
                        }
                        oi += 1;
                        ni += 1;
                    }
                }
            }
            (Some(o), None) => {
                emit_deleted_count(odb, o, prefix, show_trees, count)?;
                oi += 1;
            }
            (None, Some(n)) => {
                emit_added_count(odb, n, prefix, show_trees, count)?;
                ni += 1;
            }
            (None, None) => break,
        }
    }

    Ok(())
}

fn emit_deleted_count(
    odb: &Odb,
    entry: &TreeEntry,
    prefix: &str,
    show_trees: bool,
    count: &mut usize,
) -> Result<()> {
    let path = crate::repo_relpath::format_path_stored(prefix, &entry.name);
    if is_tree_mode(entry.mode) {
        if show_trees {
            *count += 1;
        }
        *count += diff_tree_path_change_count(odb, Some(&entry.oid), None, &path)?;
    } else {
        *count += 1;
    }
    Ok(())
}

fn emit_added_count(
    odb: &Odb,
    entry: &TreeEntry,
    prefix: &str,
    show_trees: bool,
    count: &mut usize,
) -> Result<()> {
    let path = crate::repo_relpath::format_path_stored(prefix, &entry.name);
    if is_tree_mode(entry.mode) {
        if show_trees {
            *count += 1;
        }
        *count += diff_tree_path_change_count(odb, None, Some(&entry.oid), &path)?;
    } else {
        *count += 1;
    }
    Ok(())
}

/// Compare two sorted streams of tree entries, recursing into subtrees.
fn diff_tree_entries_opts(
    odb: &Odb,
    mut old: WalkEntries<'_>,
    mut new: WalkEntries<'_>,
    prefix: &str,
    show_trees: bool,
    index_hint: Option<&DiffTreesWalkHint<'_>>,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    let mut old_entry = old.next_entry()?;
    let mut new_entry = new.next_entry()?;

    loop {
        match (&old_entry, &new_entry) {
            (Some(o), Some(n)) => {
                let cmp = crate::objects::tree_entry_cmp(
                    o.name,
                    is_tree_mode(o.mode),
                    n.name,
                    is_tree_mode(n.mode),
                );
                match cmp {
                    std::cmp::Ordering::Less => {
                        // Old entry not in new → deleted
                        emit_deleted_opts(odb, o, prefix, show_trees, index_hint, result)?;
                        old_entry = old.next_entry()?;
                    }
                    std::cmp::Ordering::Greater => {
                        // New entry not in old → added
                        emit_added_opts(odb, n, prefix, show_trees, index_hint, result)?;
                        new_entry = new.next_entry()?;
                    }
                    std::cmp::Ordering::Equal => {
                        // Both present — check for changes
                        if o.oid != n.oid
                            || crate::index::canon_mode(o.mode) != crate::index::canon_mode(n.mode)
                        {
                            let path = crate::repo_relpath::format_path_stored(prefix, o.name);
                            if is_tree_mode(o.mode) && is_tree_mode(n.mode) {
                                // Both are trees
                                if show_trees {
                                    result.push(DiffEntry {
                                        status: DiffStatus::Modified,
                                        old_path: Some(path.clone()),
                                        new_path: Some(path.clone()),
                                        old_mode: FileMode::new(crate::index::canon_mode(o.mode)),
                                        new_mode: FileMode::new(crate::index::canon_mode(n.mode)),
                                        old_oid: o.oid,
                                        new_oid: n.oid,
                                        score: None,
                                    });
                                }
                                diff_subtrees_in_place(
                                    odb,
                                    Some(&o.oid),
                                    Some(&n.oid),
                                    &path,
                                    show_trees,
                                    index_hint,
                                    result,
                                )?;
                            } else if is_tree_mode(o.mode) && !is_tree_mode(n.mode) {
                                // Tree → blob: delete tree contents, add blob
                                emit_deleted_opts(odb, o, prefix, show_trees, index_hint, result)?;
                                emit_added_opts(odb, n, prefix, show_trees, index_hint, result)?;
                            } else if !is_tree_mode(o.mode) && is_tree_mode(n.mode) {
                                // Blob → tree: delete blob, add tree contents
                                emit_deleted_opts(odb, o, prefix, show_trees, index_hint, result)?;
                                emit_added_opts(odb, n, prefix, show_trees, index_hint, result)?;
                            } else {
                                result.push(DiffEntry {
                                    status: DiffStatus::for_content_change(o.mode, n.mode),
                                    old_path: Some(path.clone()),
                                    new_path: Some(path),
                                    old_mode: FileMode::new(o.mode),
                                    new_mode: FileMode::new(n.mode),
                                    old_oid: o.oid,
                                    new_oid: n.oid,
                                    score: None,
                                });
                            }
                        }
                        old_entry = old.next_entry()?;
                        new_entry = new.next_entry()?;
                    }
                }
            }
            (Some(o), None) => {
                emit_deleted_opts(odb, o, prefix, show_trees, index_hint, result)?;
                old_entry = old.next_entry()?;
            }
            (None, Some(n)) => {
                emit_added_opts(odb, n, prefix, show_trees, index_hint, result)?;
                new_entry = new.next_entry()?;
            }
            (None, None) => break,
        }
    }

    Ok(())
}

fn emit_deleted_tree_from_index(index: &Index, dir_prefix: &str, result: &mut Vec<DiffEntry>) {
    let scan_prefix: Vec<u8> = if dir_prefix.is_empty() {
        Vec::new()
    } else {
        format!("{dir_prefix}/").into_bytes()
    };
    if index.entries_sorted {
        let start = if scan_prefix.is_empty() {
            0
        } else {
            index
                .entries
                .partition_point(|e| e.path.as_slice() < scan_prefix.as_slice())
        };
        for ie in index.entries[start..].iter() {
            if ie.stage() != 0 {
                continue;
            }
            if !scan_prefix.is_empty() && !ie.path.starts_with(&scan_prefix) {
                break;
            }
            if scan_prefix.is_empty() && ie.path.contains(&b'/') {
                continue;
            }
            let path = String::from_utf8_lossy(&ie.path).into_owned();
            result.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path),
                new_path: None,
                old_mode: FileMode::new(ie.mode),
                new_mode: FileMode::ABSENT,
                old_oid: ie.oid,
                new_oid: zero_oid(),
                score: None,
            });
        }
        return;
    }
    for ie in &index.entries {
        if ie.stage() != 0 {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&ie.path) else {
            continue;
        };
        let under = if dir_prefix.is_empty() {
            !path.contains('/')
        } else if path == dir_prefix {
            false
        } else {
            path.strip_prefix(dir_prefix)
                .is_some_and(|rest| rest.starts_with('/'))
        };
        if !under {
            continue;
        }
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path.to_owned()),
            new_path: None,
            old_mode: FileMode::new(ie.mode),
            new_mode: FileMode::ABSENT,
            old_oid: ie.oid,
            new_oid: zero_oid(),
            score: None,
        });
    }
}

fn emit_deleted_opts(
    odb: &Odb,
    entry: &TreeEntryRef<'_>,
    prefix: &str,
    show_trees: bool,
    index_hint: Option<&DiffTreesWalkHint<'_>>,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    let path = crate::repo_relpath::format_path_stored(prefix, entry.name);
    if is_tree_mode(entry.mode) {
        if show_trees {
            result.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path.clone()),
                new_path: None,
                old_mode: FileMode::new(entry.mode),
                new_mode: FileMode::ABSENT,
                old_oid: entry.oid,
                new_oid: zero_oid(),
                score: None,
            });
        }
        if let Some(index) = index_hint
            .filter(|hint| hint.old_tree_matches)
            .and_then(|hint| hint.index)
            .filter(|index| !index.has_sparse_directory_placeholders())
        {
            emit_deleted_tree_from_index(index, &path, result);
            return Ok(());
        }
        diff_subtrees_in_place(
            odb,
            Some(&entry.oid),
            None,
            &path,
            show_trees,
            index_hint,
            result,
        )?;
    } else {
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path.clone()),
            new_path: None,
            old_mode: FileMode::new(entry.mode),
            new_mode: FileMode::ABSENT,
            old_oid: entry.oid,
            new_oid: zero_oid(),
            score: None,
        });
    }
    Ok(())
}

fn emit_added_opts(
    odb: &Odb,
    entry: &TreeEntryRef<'_>,
    prefix: &str,
    show_trees: bool,
    index_hint: Option<&DiffTreesWalkHint<'_>>,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    let path = crate::repo_relpath::format_path_stored(prefix, entry.name);
    if is_tree_mode(entry.mode) {
        if show_trees {
            result.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path.clone()),
                old_mode: FileMode::ABSENT,
                new_mode: FileMode::new(entry.mode),
                old_oid: zero_oid(),
                new_oid: entry.oid,
                score: None,
            });
        }
        diff_subtrees_in_place(
            odb,
            None,
            Some(&entry.oid),
            &path,
            show_trees,
            index_hint,
            result,
        )?;
    } else {
        result.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(path),
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::new(entry.mode),
            old_oid: zero_oid(),
            new_oid: entry.oid,
            score: None,
        });
    }
    Ok(())
}

// ── Index-to-tree diff (staged changes) ─────────────────────────────

/// Compare the index against a tree (usually HEAD's tree).
///
/// This shows "staged" changes — what would be committed.
///
/// # Parameters
///
/// - `odb` — object database.
/// - `index` — the current index.
/// - `tree_oid` — the tree to compare against (e.g. HEAD's tree), or `None`
///   for comparison against an empty tree (initial commit).
///
/// # Errors
///
/// Returns errors from ODB reads.
///
/// When `ignore_submodules` is true, gitlink (`160000`) paths are omitted from the diff, matching
/// Git's `require_clean_work_tree(..., ignore_submodules=1)` used by `git rebase` / `git pull`.
pub fn diff_index_to_tree(
    odb: &Odb,
    index: &Index,
    tree_oid: Option<&ObjectId>,
    ignore_submodules: bool,
) -> Result<Vec<DiffEntry>> {
    match index_cache_tree_walk(index, tree_oid) {
        CacheTreeWalk::Fast {
            head_tree,
            cache_root,
        } => {
            diff_index_to_tree_with_cache_tree(odb, index, head_tree, cache_root, ignore_submodules)
        }
        CacheTreeWalk::Flatten => {
            diff_index_to_tree_flatten(odb, index, tree_oid, ignore_submodules)
        }
    }
}

/// Diff the index against a tree and pair deleted/added paths as renames when similar enough.
///
/// `rename_threshold` is Git's `-M` percentage (50 is the usual default); pass `100` to skip
/// rename detection.
///
/// # Errors
///
/// Propagates errors from [`diff_index_to_tree`].
pub fn diff_index_to_tree_with_renames(
    odb: &Odb,
    index: &Index,
    tree_oid: Option<&ObjectId>,
    ignore_submodules: bool,
    rename_threshold: u32,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let entries = diff_index_to_tree(odb, index, tree_oid, ignore_submodules)?;
    apply_rename_detection_to_diff_entries(
        odb,
        entries,
        None,
        rename_threshold,
        config,
        thread_policy,
    )
}

/// Apply Git `-M` rename detection to a flat diff (index↔tree, tree↔tree, or index↔worktree).
///
/// Pass `work_root` when added sides may be read from the working tree (index↔worktree diffs).
/// Use `rename_threshold` `100` to leave entries unchanged.
///
/// # Errors
///
/// Propagates object read failures while scoring renames.
pub fn apply_rename_detection_to_diff_entries(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    work_root: Option<&Path>,
    rename_threshold: u32,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    if entries.is_empty() || rename_threshold >= 100 {
        return Ok(entries);
    }
    if work_root.is_some() {
        let opts = DiffPostprocessOptions {
            rename_threshold,
            ..DiffPostprocessOptions::default()
        };
        postprocess_diff_entries(
            odb,
            entries,
            None,
            work_root,
            None,
            config,
            thread_policy,
            &opts,
        )
    } else {
        detect_renames_with_policy(
            odb,
            None,
            entries,
            rename_threshold,
            config,
            thread_policy,
            None,
        )
    }
}

/// Diff the index against the working tree and pair deleted/added paths as renames when similar.
///
/// Runs [`diff_index_to_worktree_with_options`], then [`apply_rename_detection_to_diff_entries`]
/// with `work_root` set to `work_tree`. Untracked worktree paths are not diff entries (see
/// [`crate::porcelain::status`]); they are never fed into rename detection here.
///
/// # Errors
///
/// Propagates errors from the index↔worktree diff or rename post-processing.
pub fn diff_index_to_worktree_with_renames(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    options: DiffIndexToWorktreeOptions,
    rename_threshold: u32,
) -> Result<(Vec<DiffEntry>, bool)> {
    let config = repo.config()?.as_ref().clone();
    let thread_policy = repo.thread_policy();
    let odb = repo.odb();
    let (entries, changed) = diff_index_to_worktree_with_options(odb, index, work_tree, options)?;
    let entries = apply_rename_detection_to_diff_entries(
        odb,
        entries,
        Some(work_tree),
        rename_threshold,
        &config,
        &thread_policy,
    )?;
    Ok((entries, changed))
}

enum CacheTreeWalk<'a> {
    Fast {
        head_tree: &'a ObjectId,
        cache_root: &'a CacheTreeNode,
    },
    Flatten,
}

/// Whether the index may use the cache-tree walker (possibly with an invalid root and valid
/// descendant nodes after partial invalidation).
fn index_cache_tree_walk<'a>(
    index: &'a Index,
    tree_oid: Option<&'a ObjectId>,
) -> CacheTreeWalk<'a> {
    let Some(head_tree) = tree_oid else {
        return CacheTreeWalk::Flatten;
    };
    let Some(cache_root) = index.cache_tree.as_ref() else {
        return CacheTreeWalk::Flatten;
    };
    if index.entries.iter().any(|e| e.stage() != 0) {
        return CacheTreeWalk::Flatten;
    }
    CacheTreeWalk::Fast {
        head_tree,
        cache_root,
    }
}

/// Flatten-tree index-vs-tree diff (used when the index has no cache-tree extension).
fn diff_index_to_tree_flatten(
    odb: &Odb,
    index: &Index,
    tree_oid: Option<&ObjectId>,
    ignore_submodules: bool,
) -> Result<Vec<DiffEntry>> {
    let sparse_index = index.has_sparse_directory_placeholders();
    // Flatten the tree into a sorted list of (path, mode, oid)
    let tree_entries = match tree_oid {
        Some(oid) if sparse_index => flatten_tree_with_index(odb, oid, "", Some(index))?,
        Some(oid) => flatten_tree(odb, oid, "")?,
        None => Vec::new(),
    };

    // Build maps keyed by path
    let mut tree_map: std::collections::BTreeMap<&str, &FlatEntry> =
        std::collections::BTreeMap::new();
    for entry in &tree_entries {
        tree_map.insert(&entry.path, entry);
    }

    let mut result = Vec::new();
    let mut stage0_paths = std::collections::BTreeSet::new();
    let mut unmerged_modes: std::collections::BTreeMap<String, (u8, u32)> =
        std::collections::BTreeMap::new();

    // Check index entries against tree
    for ie in &index.entries {
        let path = String::from_utf8_lossy(&ie.path).to_string();
        if ie.stage() == 0 && ie.intent_to_add() {
            // Intent-to-add entries are not "staged" for diff-index / status
            // (matches Git: `git diff --cached` is empty for `-N` paths).
            continue;
        }
        if ie.stage() == 0 && ie.is_sparse_directory_placeholder() {
            let dir = path.strip_suffix('/').unwrap_or(path.as_str());
            let index_flat = flatten_tree(odb, &ie.oid, dir)?;
            let head_flat = match tree_oid {
                None => Vec::new(),
                Some(head_root) => match subtree_oid_at_path(odb, head_root, dir)? {
                    None => Vec::new(),
                    Some(head_sub) if head_sub == ie.oid => {
                        continue;
                    }
                    Some(head_sub) => flatten_tree(odb, &head_sub, dir)?,
                },
            };
            diff_flattened_subtrees(
                &head_flat,
                &index_flat,
                ignore_submodules,
                &mut result,
                &mut stage0_paths,
                &mut tree_map,
            );
            continue;
        }
        if ie.stage() != 0 {
            let rank = match ie.stage() {
                2 => 0u8,
                3 => 1u8,
                1 => 2u8,
                _ => 3u8,
            };
            match unmerged_modes.get(&path) {
                Some((existing_rank, _)) if *existing_rank <= rank => {}
                _ => {
                    unmerged_modes.insert(path, (rank, ie.mode));
                }
            }
            continue;
        }
        if ignore_submodules && ie.mode == 0o160000 {
            let _ = tree_map.remove(path.as_str());
            stage0_paths.insert(path.clone());
            continue;
        }
        stage0_paths.insert(path.clone());
        match tree_map.remove(path.as_str()) {
            Some(te) => {
                // Present in both — check for differences
                if te.oid != ie.oid
                    || crate::index::canon_mode(te.mode) != crate::index::canon_mode(ie.mode)
                {
                    let tree_mode = crate::index::canon_mode(te.mode);
                    let index_mode = crate::index::canon_mode(ie.mode);
                    result.push(DiffEntry {
                        status: DiffStatus::for_content_change(tree_mode, index_mode),
                        old_path: Some(path.clone()),
                        new_path: Some(path),
                        old_mode: FileMode::new(tree_mode),
                        new_mode: FileMode::new(index_mode),
                        old_oid: te.oid,
                        new_oid: ie.oid,
                        score: None,
                    });
                }
            }
            None => {
                // In index but not tree → added
                result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path),
                    old_mode: FileMode::ABSENT,
                    new_mode: FileMode::new(ie.mode),
                    old_oid: zero_oid(),
                    new_oid: ie.oid,
                    score: None,
                });
            }
        }
    }

    diff_index_to_tree_finish_unmerged_and_deleted(
        ignore_submodules,
        &mut tree_map,
        &unmerged_modes,
        &stage0_paths,
        &mut result,
    );

    sort_entries_by_path(&mut result);
    Ok(result)
}

fn diff_index_to_tree_finish_unmerged_and_deleted(
    ignore_submodules: bool,
    tree_map: &mut std::collections::BTreeMap<&str, &FlatEntry>,
    unmerged_modes: &std::collections::BTreeMap<String, (u8, u32)>,
    stage0_paths: &std::collections::BTreeSet<String>,
    result: &mut Vec<DiffEntry>,
) {
    for (path, (_, mode)) in unmerged_modes {
        if stage0_paths.contains(path) {
            continue;
        }
        tree_map.remove(path.as_str());
        result.push(DiffEntry {
            status: DiffStatus::Unmerged,
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::new(*mode),
            old_oid: zero_oid(),
            new_oid: zero_oid(),
            score: None,
        });
    }

    // Remaining tree entries not in index → deleted
    for (path, te) in std::mem::take(tree_map) {
        if ignore_submodules && te.mode == 0o160000 {
            continue;
        }
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path.to_owned()),
            new_path: None,
            old_mode: FileMode::new(te.mode),
            new_mode: FileMode::ABSENT,
            old_oid: te.oid,
            new_oid: zero_oid(),
            score: None,
        });
    }
}

/// Cache-tree-aware index-vs-tree diff: walk HEAD tree and index together and skip
/// subtrees whose valid cache-tree node OID matches the tree entry (Git unpack-trees).
fn diff_index_to_tree_with_cache_tree(
    odb: &Odb,
    index: &Index,
    head_tree: &ObjectId,
    cache_root: &CacheTreeNode,
    ignore_submodules: bool,
) -> Result<Vec<DiffEntry>> {
    let mut result = Vec::new();
    let mut idx_cursor = 0usize;

    let mut walk = DiffIndexTreeWalk {
        odb,
        index,
        idx_cursor: &mut idx_cursor,
        ignore_submodules,
        result: &mut result,
        abort_to_flatten: false,
    };
    diff_index_tree_at(&mut walk, head_tree, Some(cache_root), b"")?;
    if walk.abort_to_flatten {
        return diff_index_to_tree_flatten(odb, index, Some(head_tree), ignore_submodules);
    }

    // Index entries not consumed by the tree walk (added paths).
    while idx_cursor < index.entries.len() {
        let ie = &index.entries[idx_cursor];
        idx_cursor += 1;
        let path = String::from_utf8_lossy(&ie.path).to_string();
        if ie.stage() == 0 && ie.intent_to_add() {
            continue;
        }
        if ie.stage() != 0 {
            continue;
        }
        if ignore_submodules && ie.mode == 0o160000 {
            continue;
        }
        result.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(path),
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::new(ie.mode),
            old_oid: zero_oid(),
            new_oid: ie.oid,
            score: None,
        });
    }

    sort_entries_by_path(&mut result);
    Ok(result)
}

enum IndexLevelKind<'a> {
    Blob {
        entry: &'a IndexEntry,
        name: &'a [u8],
    },
    Tree {
        name: &'a [u8],
    },
}

/// `path` relative to the directory `dir_prefix` (raw bytes, no trailing slash), or `None` when
/// `path` lies outside it. The directory's own path maps to an empty slice.
fn index_relative_in_dir<'a>(path: &'a [u8], dir_prefix: &[u8]) -> Option<&'a [u8]> {
    if dir_prefix.is_empty() {
        return Some(path);
    }
    let rest = path.strip_prefix(dir_prefix)?;
    match rest.split_first() {
        None => Some(rest),
        Some((b'/', tail)) => Some(tail),
        Some(_) => None,
    }
}

fn join_index_path(dir_prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if dir_prefix.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(dir_prefix.len() + 1 + name.len());
    path.extend_from_slice(dir_prefix);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

fn peek_index_level_entry<'a>(
    index: &'a Index,
    cursor: &mut usize,
    dir_prefix: &[u8],
    ignore_submodules: bool,
) -> Option<IndexLevelKind<'a>> {
    while let Some(ie) = index.entries.get(*cursor) {
        let rel = index_relative_in_dir(&ie.path, dir_prefix)?;
        if ie.stage() != 0 || ie.intent_to_add() || rel.is_empty() {
            *cursor += 1;
            continue;
        }
        if let Some(slash) = rel.iter().position(|&b| b == b'/') {
            return Some(IndexLevelKind::Tree {
                name: &rel[..slash],
            });
        }
        if ignore_submodules && ie.mode == 0o160000 {
            *cursor += 1;
            continue;
        }
        return Some(IndexLevelKind::Blob {
            entry: ie,
            name: rel,
        });
    }
    None
}

fn skip_index_subtree(index: &Index, cursor: &mut usize, dir_prefix: &[u8]) {
    while index
        .entries
        .get(*cursor)
        .is_some_and(|ie| index_relative_in_dir(&ie.path, dir_prefix).is_some())
    {
        *cursor += 1;
    }
}

/// Children of a cache-tree node up to which a linear name scan beats sorting.
const CACHE_CHILD_LINEAR_SCAN_MAX: usize = 16;

/// Name lookup over one cache-tree node's subtrees.
///
/// The index extension does not promise any child order, and a wide directory is looked up
/// once per tree entry, so nodes with many children are sorted once per visit instead of
/// being scanned for every entry. The first child with a matching name wins either way.
struct CacheChildren<'a> {
    children: &'a [CacheTreeNode],
    sorted: Vec<&'a CacheTreeNode>,
}

impl<'a> CacheChildren<'a> {
    fn new(node: Option<&'a CacheTreeNode>) -> Self {
        let children = node.map_or(&[][..], |n| n.children.as_slice());
        let sorted = if children.len() > CACHE_CHILD_LINEAR_SCAN_MAX {
            let mut sorted: Vec<&CacheTreeNode> = children.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            sorted
        } else {
            Vec::new()
        };
        Self { children, sorted }
    }

    fn get(&self, name: &[u8]) -> Option<&'a CacheTreeNode> {
        if self.sorted.is_empty() {
            return self.children.iter().find(|c| c.name == name);
        }
        let pos = self.sorted.partition_point(|c| c.name.as_slice() < name);
        self.sorted.get(pos).copied().filter(|c| c.name == name)
    }
}

struct DiffIndexTreeWalk<'a, 'b> {
    odb: &'a Odb,
    index: &'a Index,
    idx_cursor: &'b mut usize,
    ignore_submodules: bool,
    result: &'b mut Vec<DiffEntry>,
    /// Set when a tree object has duplicate entry names; the caller falls back to flatten.
    abort_to_flatten: bool,
}

fn tree_entries_have_duplicate_names(entries: &[TreeEntry]) -> bool {
    let mut seen = std::collections::BTreeSet::<&[u8]>::new();
    for entry in entries {
        if !seen.insert(entry.name.as_slice()) {
            return true;
        }
    }
    false
}

/// Walk the tree `tree_oid` against the index entries under `dir_prefix` (raw path bytes).
fn diff_index_tree_at(
    walk: &mut DiffIndexTreeWalk<'_, '_>,
    tree_oid: &ObjectId,
    cache_node: Option<&CacheTreeNode>,
    dir_prefix: &[u8],
) -> Result<()> {
    if !dir_prefix.is_empty() {
        let mut path = dir_prefix.to_vec();
        path.push(b'/');
        if let Some(entry) = walk
            .index
            .entries
            .get(*walk.idx_cursor)
            .filter(|entry| entry.path == path && entry.is_sparse_directory_placeholder())
        {
            if entry.oid == *tree_oid {
                *walk.idx_cursor += 1;
                return Ok(());
            }
            let dir = String::from_utf8_lossy(dir_prefix);
            let head_flat = flatten_tree(walk.odb, tree_oid, dir.as_ref())?;
            let index_flat = flatten_tree(walk.odb, &entry.oid, dir.as_ref())?;
            let mut stage0_paths = std::collections::BTreeSet::new();
            let mut tree_map = std::collections::BTreeMap::new();
            diff_flattened_subtrees(
                &head_flat,
                &index_flat,
                walk.ignore_submodules,
                walk.result,
                &mut stage0_paths,
                &mut tree_map,
            );
            *walk.idx_cursor += 1;
            return Ok(());
        }
    }
    let tree_entries = read_tree(walk.odb, tree_oid)?;
    if tree_entries_have_duplicate_names(&tree_entries) {
        walk.abort_to_flatten = true;
        return Ok(());
    }
    let cache_children = CacheChildren::new(cache_node);
    let mut ti = 0usize;

    loop {
        let te = tree_entries.get(ti);
        let ie_kind = peek_index_level_entry(
            walk.index,
            walk.idx_cursor,
            dir_prefix,
            walk.ignore_submodules,
        );

        match (te, ie_kind) {
            (Some(t_entry), Some(ie)) => {
                let t_name = &t_entry.name;
                let (i_name, i_is_tree) = match &ie {
                    IndexLevelKind::Blob { name, .. } => (*name, false),
                    IndexLevelKind::Tree { name } => (*name, true),
                };
                let t_is_tree = is_tree_mode(t_entry.mode);
                let cmp = crate::objects::tree_entry_cmp(t_name, t_is_tree, i_name, i_is_tree);
                match cmp {
                    std::cmp::Ordering::Less => {
                        emit_index_tree_deleted(
                            walk.odb,
                            t_entry,
                            dir_prefix,
                            walk.ignore_submodules,
                            walk.result,
                        )?;
                        ti += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        emit_index_tree_added(walk, dir_prefix)?;
                    }
                    std::cmp::Ordering::Equal => {
                        if t_is_tree && i_is_tree {
                            let full_path = join_index_path(dir_prefix, t_name);
                            if diff_index_tree_skip_sparse_placeholder(walk, &full_path, t_entry)? {
                                ti += 1;
                                continue;
                            }
                            let child_cache = cache_children.get(t_name);
                            if child_cache
                                .is_some_and(|c| c.is_valid() && c.oid == Some(t_entry.oid))
                            {
                                skip_index_subtree(walk.index, walk.idx_cursor, &full_path);
                                ti += 1;
                                continue;
                            }
                            diff_index_tree_at(walk, &t_entry.oid, child_cache, &full_path)?;
                        } else if t_is_tree != i_is_tree {
                            emit_index_tree_deleted(
                                walk.odb,
                                t_entry,
                                dir_prefix,
                                walk.ignore_submodules,
                                walk.result,
                            )?;
                            emit_index_tree_added(walk, dir_prefix)?;
                        } else {
                            let IndexLevelKind::Blob { entry: ie, .. } = ie else {
                                ti += 1;
                                continue;
                            };
                            *walk.idx_cursor += 1;
                            if t_entry.oid != ie.oid
                                || crate::index::canon_mode(t_entry.mode)
                                    != crate::index::canon_mode(ie.mode)
                            {
                                let path = String::from_utf8_lossy(&ie.path).into_owned();
                                let tree_mode = crate::index::canon_mode(t_entry.mode);
                                let index_mode = crate::index::canon_mode(ie.mode);
                                walk.result.push(DiffEntry {
                                    status: DiffStatus::for_content_change(tree_mode, index_mode),
                                    old_path: Some(path.clone()),
                                    new_path: Some(path),
                                    old_mode: FileMode::new(tree_mode),
                                    new_mode: FileMode::new(index_mode),
                                    old_oid: t_entry.oid,
                                    new_oid: ie.oid,
                                    score: None,
                                });
                            }
                        }
                        ti += 1;
                    }
                }
            }
            (Some(t_entry), None) => {
                emit_index_tree_deleted(
                    walk.odb,
                    t_entry,
                    dir_prefix,
                    walk.ignore_submodules,
                    walk.result,
                )?;
                ti += 1;
            }
            (None, Some(_)) => {
                emit_index_tree_added(walk, dir_prefix)?;
            }
            (None, None) => break,
        }
    }

    Ok(())
}

fn diff_index_tree_skip_sparse_placeholder(
    walk: &mut DiffIndexTreeWalk<'_, '_>,
    full_path: &[u8],
    t_entry: &TreeEntry,
) -> Result<bool> {
    let Some(entry) = walk.index.entries.get(*walk.idx_cursor) else {
        return Ok(false);
    };
    if !entry.is_sparse_directory_placeholder() {
        return Ok(false);
    }
    let mut dir = full_path.to_vec();
    if !dir.ends_with(b"/") {
        dir.push(b'/');
    }
    if entry.path != dir || entry.oid != t_entry.oid {
        return Ok(false);
    }
    *walk.idx_cursor += 1;
    Ok(true)
}

fn emit_index_tree_deleted(
    odb: &Odb,
    entry: &TreeEntry,
    dir_prefix: &[u8],
    ignore_submodules: bool,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    if ignore_submodules && entry.mode == 0o160000 {
        return Ok(());
    }
    let path = String::from_utf8_lossy(&join_index_path(dir_prefix, &entry.name)).into_owned();
    if is_tree_mode(entry.mode) {
        for fe in flatten_tree(odb, &entry.oid, &path)? {
            if ignore_submodules && fe.mode == 0o160000 {
                continue;
            }
            result.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(fe.path.clone()),
                new_path: None,
                old_mode: FileMode::new(fe.mode),
                new_mode: FileMode::ABSENT,
                old_oid: fe.oid,
                new_oid: zero_oid(),
                score: None,
            });
        }
    } else {
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path),
            new_path: None,
            old_mode: FileMode::new(entry.mode),
            new_mode: FileMode::ABSENT,
            old_oid: entry.oid,
            new_oid: zero_oid(),
            score: None,
        });
    }
    Ok(())
}

fn emit_index_tree_added(walk: &mut DiffIndexTreeWalk<'_, '_>, dir_prefix: &[u8]) -> Result<()> {
    let ie_kind = peek_index_level_entry(
        walk.index,
        walk.idx_cursor,
        dir_prefix,
        walk.ignore_submodules,
    )
    .ok_or_else(|| Error::IndexError("missing index entry during tree walk".to_owned()))?;
    match ie_kind {
        IndexLevelKind::Blob { entry: ie, .. } => {
            let path = String::from_utf8_lossy(&ie.path).into_owned();
            *walk.idx_cursor += 1;
            walk.result.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path),
                old_mode: FileMode::ABSENT,
                new_mode: FileMode::new(ie.mode),
                old_oid: zero_oid(),
                new_oid: ie.oid,
                score: None,
            });
        }
        IndexLevelKind::Tree { name } => {
            let full_path = join_index_path(dir_prefix, name);
            let mut path_slash = full_path.clone();
            path_slash.push(b'/');
            if let Some(entry) =
                walk.index.entries.get(*walk.idx_cursor).filter(|entry| {
                    entry.path == path_slash && entry.is_sparse_directory_placeholder()
                })
            {
                let dir = String::from_utf8_lossy(if dir_prefix.is_empty() {
                    name
                } else {
                    &full_path
                });
                emit_flattened_subtree_as_added(
                    walk.odb,
                    &entry.oid,
                    dir.as_ref(),
                    walk.ignore_submodules,
                    walk.result,
                )?;
                *walk.idx_cursor += 1;
                return Ok(());
            }
            let start = *walk.idx_cursor;
            skip_index_subtree(walk.index, walk.idx_cursor, &full_path);
            for ie in &walk.index.entries[start..*walk.idx_cursor] {
                if ie.stage() != 0 || ie.intent_to_add() {
                    continue;
                }
                if walk.ignore_submodules && ie.mode == 0o160000 {
                    continue;
                }
                let path = String::from_utf8_lossy(&ie.path).into_owned();
                walk.result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path),
                    old_mode: FileMode::ABSENT,
                    new_mode: FileMode::new(ie.mode),
                    old_oid: zero_oid(),
                    new_oid: ie.oid,
                    score: None,
                });
            }
        }
    }
    Ok(())
}

/// Paths and rows to reconcile an index with a target tree during hard reset.
#[derive(Default)]
pub(crate) struct HardResetIndexPlan {
    pub remove_paths: Vec<Vec<u8>>,
    pub updates: Vec<(Vec<u8>, u32, ObjectId)>,
    pub insert_entries: Vec<IndexEntry>,
}

/// Plan index changes to match `target_tree` using a cache-tree-aware lockstep walk (raw paths).
///
/// Never builds a flattened tree map; when the cache-tree extension is absent, walks the full tree
/// against the index instead of calling [`diff_index_to_tree_flatten`].
pub(crate) fn plan_hard_reset_index_align(
    odb: &Odb,
    index: &Index,
    target_tree: &ObjectId,
) -> Result<HardResetIndexPlan> {
    let mut plan = HardResetIndexPlan::default();
    let mut idx_cursor = 0usize;
    let cache_root = index.cache_tree.as_ref();
    let use_cache = cache_root.is_some()
        && index
            .entries
            .iter()
            .all(|entry| entry.stage() == 0 && !entry.intent_to_add());
    let cache_node = if use_cache { cache_root } else { None };
    let retry = {
        let mut walk = HardResetAlignWalk {
            odb,
            index,
            idx_cursor: &mut idx_cursor,
            ignore_submodules: false,
            plan: &mut plan,
            retry_without_cache: false,
        };
        hard_reset_align_index_tree_at(&mut walk, target_tree, cache_node, b"")?;
        walk.retry_without_cache
    };
    if retry {
        idx_cursor = 0;
        plan = HardResetIndexPlan::default();
        let mut walk = HardResetAlignWalk {
            odb,
            index,
            idx_cursor: &mut idx_cursor,
            ignore_submodules: false,
            plan: &mut plan,
            retry_without_cache: false,
        };
        hard_reset_align_index_tree_at(&mut walk, target_tree, None, b"")?;
    }
    while idx_cursor < index.entries.len() {
        let ie = &index.entries[idx_cursor];
        idx_cursor += 1;
        if ie.stage() != 0 || ie.intent_to_add() {
            continue;
        }
        plan.remove_paths.push(ie.path.clone());
    }
    Ok(plan)
}

/// Apply a [`HardResetIndexPlan`] and invalidate cache-tree nodes for touched paths.
pub(crate) fn apply_hard_reset_index_plan(index: &mut Index, plan: HardResetIndexPlan) {
    for path in &plan.remove_paths {
        index.remove_path_all_stages(path);
        index.invalidate_cache_tree_for_path(path);
    }
    for (path, mode, oid) in plan.updates {
        if let Some(pos) = index.find_entry_pos(&path, 0) {
            index.entries[pos].mode = mode;
            index.entries[pos].oid = oid;
            index.entries[pos].size = 0;
            index.invalidate_cache_tree_for_path(&path);
        }
    }
    for entry in plan.insert_entries {
        let path = entry.path.clone();
        index.add_or_replace(entry);
        index.invalidate_cache_tree_for_path(&path);
    }
    index.sort();
}

/// Rebuild the cache-tree extension after hard-reset index alignment, reusing valid seed
/// subtrees from the lockstep tree/index walk.
pub(crate) fn rebuild_hard_reset_cache_tree(
    odb: &Odb,
    index: &mut Index,
    target_tree: &ObjectId,
) -> Result<()> {
    let stage0_count = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == 0 && !entry.intent_to_add())
        .count();
    let seed_root = index.cache_tree.as_ref();
    let mut idx_cursor = 0usize;
    let root = rebuild_hard_reset_cache_tree_at(
        odb,
        index,
        &mut idx_cursor,
        target_tree,
        seed_root,
        b"",
        Vec::new(),
    )?;
    if usize::try_from(root.entry_count).ok() == Some(stage0_count) {
        index.set_cache_tree(root);
        Ok(())
    } else {
        Err(Error::IndexError(
            "hard reset cache-tree entry count does not match index".into(),
        ))
    }
}

fn rebuild_hard_reset_cache_tree_at(
    odb: &Odb,
    index: &Index,
    idx_cursor: &mut usize,
    tree_oid: &ObjectId,
    seed_node: Option<&CacheTreeNode>,
    dir_prefix: &[u8],
    name: Vec<u8>,
) -> Result<CacheTreeNode> {
    if !dir_prefix.is_empty() {
        let mut path = dir_prefix.to_vec();
        path.push(b'/');
        if let Some(entry) = index.entries.get(*idx_cursor).filter(|entry| {
            entry.path == path && entry.is_sparse_directory_placeholder() && entry.oid == *tree_oid
        }) {
            let _ = entry;
            *idx_cursor += 1;
            return Ok(CacheTreeNode::valid(name, 1, *tree_oid, Vec::new()));
        }
    }
    let tree_entries = read_tree(odb, tree_oid)?;
    let cache_children = CacheChildren::new(seed_node);
    let mut entry_count: i32 = 0;
    let mut children: Vec<CacheTreeNode> = Vec::new();
    let mut ti = 0usize;

    loop {
        let te = tree_entries.get(ti);
        let ie_kind = peek_index_level_entry(index, idx_cursor, dir_prefix, false);
        match (te, ie_kind) {
            (Some(t_entry), Some(ie)) => {
                let t_name = &t_entry.name;
                let (i_name, i_is_tree) = match &ie {
                    IndexLevelKind::Blob { name, .. } => (*name, false),
                    IndexLevelKind::Tree { name } => (*name, true),
                };
                let t_is_tree = is_tree_mode(t_entry.mode);
                let cmp = crate::objects::tree_entry_cmp(t_name, t_is_tree, i_name, i_is_tree);
                if cmp != std::cmp::Ordering::Equal {
                    return Err(Error::IndexError(
                        "hard reset cache-tree rebuild: index out of sync with target tree".into(),
                    ));
                }
                if t_is_tree && i_is_tree {
                    let full_path = join_index_path(dir_prefix, t_name);
                    if hard_reset_skip_sparse_cache_rebuild(index, idx_cursor, &full_path, t_entry)?
                    {
                        entry_count = entry_count.saturating_add(1);
                        ti += 1;
                        continue;
                    }
                    let child = if let Some(child_cache) = cache_children
                        .get(t_name)
                        .filter(|node| node.is_valid() && node.oid == Some(t_entry.oid))
                    {
                        skip_index_subtree(index, idx_cursor, &full_path);
                        let mut reused = (*child_cache).clone();
                        reused.name = t_name.clone();
                        reused
                    } else {
                        rebuild_hard_reset_cache_tree_at(
                            odb,
                            index,
                            idx_cursor,
                            &t_entry.oid,
                            cache_children.get(t_name),
                            &full_path,
                            t_name.clone(),
                        )?
                    };
                    entry_count = entry_count.saturating_add(child.entry_count.max(0));
                    children.push(child);
                } else if t_is_tree != i_is_tree {
                    return Err(Error::IndexError(
                        "hard reset cache-tree rebuild: tree/index type mismatch".into(),
                    ));
                } else {
                    *idx_cursor += 1;
                    entry_count = entry_count.saturating_add(1);
                }
                ti += 1;
            }
            (None, None) => break,
            _ => {
                return Err(Error::IndexError(
                    "hard reset cache-tree rebuild: index out of sync with target tree".into(),
                ));
            }
        }
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(CacheTreeNode::valid(name, entry_count, *tree_oid, children))
}

fn hard_reset_skip_sparse_cache_rebuild(
    index: &Index,
    idx_cursor: &mut usize,
    full_path: &[u8],
    t_entry: &TreeEntry,
) -> Result<bool> {
    let Some(entry) = index.entries.get(*idx_cursor) else {
        return Ok(false);
    };
    if !entry.is_sparse_directory_placeholder() {
        return Ok(false);
    }
    let mut dir = full_path.to_vec();
    if !dir.ends_with(b"/") {
        dir.push(b'/');
    }
    if entry.path != dir || entry.oid != t_entry.oid {
        return Ok(false);
    }
    *idx_cursor += 1;
    Ok(true)
}

struct HardResetAlignWalk<'a, 'b> {
    odb: &'a Odb,
    index: &'a Index,
    idx_cursor: &'b mut usize,
    ignore_submodules: bool,
    plan: &'b mut HardResetIndexPlan,
    retry_without_cache: bool,
}

fn hard_reset_align_index_tree_at(
    walk: &mut HardResetAlignWalk<'_, '_>,
    tree_oid: &ObjectId,
    cache_node: Option<&CacheTreeNode>,
    dir_prefix: &[u8],
) -> Result<()> {
    let tree_entries = read_tree(walk.odb, tree_oid)?;
    if tree_entries_have_duplicate_names(&tree_entries) {
        walk.retry_without_cache = true;
        return Ok(());
    }
    let cache_children = CacheChildren::new(cache_node);
    let mut ti = 0usize;

    loop {
        let te = tree_entries.get(ti);
        let ie_kind = peek_index_level_entry(
            walk.index,
            walk.idx_cursor,
            dir_prefix,
            walk.ignore_submodules,
        );

        match (te, ie_kind) {
            (Some(t_entry), Some(ie)) => {
                let t_name = &t_entry.name;
                let (i_name, i_is_tree) = match &ie {
                    IndexLevelKind::Blob { name, .. } => (*name, false),
                    IndexLevelKind::Tree { name } => (*name, true),
                };
                let t_is_tree = is_tree_mode(t_entry.mode);
                let cmp = crate::objects::tree_entry_cmp(t_name, t_is_tree, i_name, i_is_tree);
                match cmp {
                    std::cmp::Ordering::Less => {
                        hard_reset_plan_tree_only(walk, t_entry, dir_prefix)?;
                        ti += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        hard_reset_plan_index_only(walk, dir_prefix)?;
                    }
                    std::cmp::Ordering::Equal => {
                        if t_is_tree && i_is_tree {
                            let full_path = join_index_path(dir_prefix, t_name);
                            if hard_reset_skip_sparse_placeholder(walk, &full_path, t_entry)? {
                                ti += 1;
                                continue;
                            }
                            let child_cache = cache_children.get(t_name);
                            if child_cache
                                .is_some_and(|c| c.is_valid() && c.oid == Some(t_entry.oid))
                            {
                                skip_index_subtree(walk.index, walk.idx_cursor, &full_path);
                                ti += 1;
                                continue;
                            }
                            hard_reset_align_index_tree_at(
                                walk,
                                &t_entry.oid,
                                child_cache,
                                &full_path,
                            )?;
                        } else if t_is_tree != i_is_tree {
                            hard_reset_plan_tree_only(walk, t_entry, dir_prefix)?;
                            hard_reset_plan_index_only(walk, dir_prefix)?;
                        } else {
                            let IndexLevelKind::Blob { entry: ie, .. } = ie else {
                                ti += 1;
                                continue;
                            };
                            *walk.idx_cursor += 1;
                            if t_entry.oid != ie.oid
                                || crate::index::canon_mode(t_entry.mode)
                                    != crate::index::canon_mode(ie.mode)
                            {
                                walk.plan.updates.push((
                                    ie.path.clone(),
                                    crate::index::canon_mode(t_entry.mode),
                                    t_entry.oid,
                                ));
                            }
                        }
                        ti += 1;
                    }
                }
            }
            (Some(t_entry), None) => {
                hard_reset_plan_tree_only(walk, t_entry, dir_prefix)?;
                ti += 1;
            }
            (None, Some(_)) => {
                hard_reset_plan_index_only(walk, dir_prefix)?;
            }
            (None, None) => break,
        }
    }
    Ok(())
}

fn hard_reset_skip_sparse_placeholder(
    walk: &mut HardResetAlignWalk<'_, '_>,
    full_path: &[u8],
    t_entry: &TreeEntry,
) -> Result<bool> {
    let Some(ie) = walk.index.entries.get(*walk.idx_cursor) else {
        return Ok(false);
    };
    if !ie.is_sparse_directory_placeholder() {
        return Ok(false);
    }
    let mut dir = full_path.to_vec();
    if !dir.ends_with(b"/") {
        dir.push(b'/');
    }
    if ie.path != dir || ie.oid != t_entry.oid {
        return Ok(false);
    }
    *walk.idx_cursor += 1;
    Ok(true)
}

fn hard_reset_plan_index_only(
    walk: &mut HardResetAlignWalk<'_, '_>,
    dir_prefix: &[u8],
) -> Result<()> {
    let ie_kind = peek_index_level_entry(
        walk.index,
        walk.idx_cursor,
        dir_prefix,
        walk.ignore_submodules,
    )
    .ok_or_else(|| Error::IndexError("missing index entry during hard reset align".into()))?;
    match ie_kind {
        IndexLevelKind::Blob { entry: ie, .. } => {
            walk.plan.remove_paths.push(ie.path.clone());
            *walk.idx_cursor += 1;
        }
        IndexLevelKind::Tree { name } => {
            let full_path = join_index_path(dir_prefix, name);
            let start = *walk.idx_cursor;
            skip_index_subtree(walk.index, walk.idx_cursor, &full_path);
            for ie in &walk.index.entries[start..*walk.idx_cursor] {
                if ie.stage() != 0 || ie.intent_to_add() {
                    continue;
                }
                walk.plan.remove_paths.push(ie.path.clone());
            }
        }
    }
    Ok(())
}

fn hard_reset_plan_tree_only(
    walk: &mut HardResetAlignWalk<'_, '_>,
    entry: &TreeEntry,
    dir_prefix: &[u8],
) -> Result<()> {
    if walk.ignore_submodules && entry.mode == 0o160000 {
        return Ok(());
    }
    let path = join_index_path(dir_prefix, &entry.name);
    if is_tree_mode(entry.mode) {
        hard_reset_append_entries_from_tree(
            walk.odb,
            &entry.oid,
            &path,
            &mut walk.plan.insert_entries,
        )?;
    } else {
        walk.plan
            .insert_entries
            .push(hard_reset_index_tree_row(path, entry.mode, entry.oid));
    }
    Ok(())
}

fn hard_reset_index_tree_row(path: Vec<u8>, mode: u32, oid: ObjectId) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: crate::index::canon_mode(mode),
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path,
        base_index_pos: 0,
    }
}

fn hard_reset_append_entries_from_tree(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &[u8],
    out: &mut Vec<IndexEntry>,
) -> Result<()> {
    for te in read_tree(odb, tree_oid)? {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&te.name);
        if is_tree_mode(te.mode) {
            hard_reset_append_entries_from_tree(odb, &te.oid, &path, out)?;
        } else {
            out.push(hard_reset_index_tree_row(path, te.mode, te.oid));
        }
    }
    Ok(())
}

// ── Index-to-worktree diff (unstaged changes) ───────────────────────

/// Compare the index against the working tree.
///
/// This shows "unstaged" changes — modifications not yet staged.
///
/// Entries with [`IndexEntry::assume_unchanged`] or [`IndexEntry::skip_worktree`] are treated as
/// matching the work tree without examining the filesystem (Git `CE_VALID` / skip-worktree).
///
/// # Parameters
///
/// - `odb` — object database (for hashing worktree files).
/// - `index` — the current index.
/// - `work_tree` — path to the working tree root.
/// - `ignore_submodule_untracked` — when true, gitlink entries are not dirty solely from untracked
///   files inside the submodule (matches `git status -uno`).
/// - `simplify_gitlinks` — when true, nested gitlink entries only compare the submodule checkout
///   HEAD to the recorded OID (ignore dirty work trees inside nested submodules). Used when
///   computing `submodule_porcelain_flags` so untracked files under a nested submodule do not set
///   the parent submodule's `modified` bit (Git `DIRTY_SUBMODULE_MODIFIED`; t7506).
///
/// # Errors
///
/// Returns errors from I/O or hashing.
pub fn diff_index_to_worktree(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    ignore_submodule_untracked: bool,
    simplify_gitlinks: bool,
) -> Result<Vec<DiffEntry>> {
    diff_index_to_worktree_with_options(
        odb,
        index,
        work_tree,
        DiffIndexToWorktreeOptions {
            ignore_submodule_untracked,
            simplify_gitlinks,
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .map(|(entries, _)| entries)
}

/// Additional inputs for [`diff_index_to_worktree_with_options`].
#[derive(Clone)]
pub struct DiffIndexToWorktreeOptions {
    /// Optional index mtime pair `(sec, nsec)` sampled when the index was read.
    ///
    /// When provided, entries with matching stat data are still considered dirty candidates if
    /// their recorded mtime is "racy" (at or after this timestamp), matching Git's
    /// `is_racy_timestamp` behavior.
    pub index_mtime: Option<(u32, u32)>,
    /// When true, gitlink entries are not dirty solely from untracked files inside the submodule.
    pub ignore_submodule_untracked: bool,
    /// When true, nested gitlink entries only compare the submodule checkout HEAD to the recorded OID.
    pub simplify_gitlinks: bool,
    /// When true, a populated gitlink checkout whose `.git` indirection cannot resolve to a HEAD
    /// is returned as an error instead of a normal modified gitlink.
    pub error_on_broken_gitlinks: bool,
    /// Repository git directory used to load config (separate git dir / linked worktrees).
    ///
    /// When unset, defaults to `work_tree/.git`, which is wrong for gitfile layouts.
    pub repository_git_dir: Option<std::path::PathBuf>,
    /// When true, adopt refreshed stat data for unchanged blobs during the same directory scan
    /// as the worktree diff (one lstat per tracked path).
    pub refresh_index_stat_in_pass: bool,
    /// When set, used instead of loading config from [`Self::repository_git_dir`].
    pub config: Option<std::sync::Arc<crate::config::ConfigSet>>,
    /// Cap parallel stat workers (`Some(1)` forces serial), also applied to hashing the
    /// racily clean entries the stat refresh verifies. When `None`, honors
    /// `core.preloadIndex` and [`crate::worktree_scan::PARALLEL_STAT_MIN_ENTRIES`].
    pub stat_parallel_threads: Option<usize>,
    /// CLI / embedder thread override applied to stat preload and hashing in this diff.
    pub thread_policy: Option<crate::parallel::ThreadPolicy>,
    /// Shared per-operation attribute/conversion context (see [`crate::worktree_rules::WorktreeRules`]).
    pub worktree_rules:
        Option<std::sync::Arc<std::sync::Mutex<crate::worktree_rules::WorktreeRules>>>,
    /// Status-only shortcuts (default `false`).
    ///
    /// When true, a tracked file whose size differs from its index entry is reported as
    /// modified without reading it, with a null worktree `new_oid`, and intent-to-add
    /// entries are not hashed. Content diffs must leave this off so a content change always
    /// carries a null worktree `new_oid` (`old_oid != new_oid`).
    pub for_status: bool,
    /// When true (default), query `core.fsmonitor` before scanning the worktree.
    pub refresh_fsmonitor: bool,
    /// Injectable subprocess runner for fsmonitor hooks; defaults to the system runner.
    pub command_runner: Option<std::sync::Arc<dyn crate::command_runner::CommandRunner>>,
    /// When set, used as the hook protocol v1 query boundary; otherwise [`crate::environment::wall_clock_unix_timestamp_nanos`] is sampled once per diff.
    pub fsmonitor_query_boundary: Option<crate::fsmonitor::FsmonitorQueryBoundary>,
    /// When set, only index paths in this set are compared against the work tree.
    ///
    /// Used by branch switch to verify dirty paths only where the tree-to-tree diff would
    /// update the worktree, avoiding a full tracked scan on large indexes. Callers must
    /// refuse the operation when the index has unmerged entries; this filter does not
    /// substitute for a full unmerged scan.
    pub restrict_to_paths: Option<std::sync::Arc<std::collections::BTreeSet<String>>>,
    /// When true with staging materialization, compute blob OIDs without writing loose objects.
    pub hash_blobs_without_write: bool,
    /// When set, long loops abort with [`crate::error::Error::Cancelled`] once the flag is raised
    /// (see [`crate::progress::cancel_watch_arc`] and [`crate::progress::poll_cancel_watch`]).
    pub cancel_watch: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// When set, work-tree access failures during status emit [`crate::diagnostics::Warning`]
    /// values here instead of aborting the diff.
    pub diagnostics: Option<crate::diagnostics::DiagnosticsHandle>,
}

impl Default for DiffIndexToWorktreeOptions {
    fn default() -> Self {
        Self {
            index_mtime: None,
            ignore_submodule_untracked: false,
            simplify_gitlinks: false,
            error_on_broken_gitlinks: false,
            repository_git_dir: None,
            refresh_index_stat_in_pass: false,
            config: None,
            stat_parallel_threads: None,
            thread_policy: None,
            worktree_rules: None,
            for_status: false,
            refresh_fsmonitor: true,
            command_runner: None,
            fsmonitor_query_boundary: None,
            restrict_to_paths: None,
            hash_blobs_without_write: false,
            cancel_watch: None,
            diagnostics: None,
        }
    }
}

#[inline]
fn poll_diff_cancel_watch(
    watch: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> crate::error::Result<()> {
    if let Some(flag) = watch {
        crate::progress::poll_watch(flag)?;
    }
    Ok(())
}

#[inline]
fn poll_diff_cancel(
    options: &DiffIndexToWorktreeOptions,
    owner_poll: Option<&std::sync::Arc<crate::progress::StackCancelPoll>>,
) -> crate::error::Result<()> {
    if let Some(poll) = owner_poll {
        poll.poll_owner()?;
    } else {
        poll_diff_cancel_watch(options.cancel_watch.as_ref())?;
    }
    Ok(())
}

/// One tracked file the parallel pass must hash, queued once per stat-dirty index entry.
///
/// A stale index can queue every entry, so the job names its entry by position instead of
/// owning copies of it, and the workers rebuild paths from the shared index.
struct StagingHashJob {
    entry_index: usize,
    /// Worktree mode after [`crate::index::WorktreeModePolicy`] against the entry's mode.
    worktree_mode: u32,
    /// `None` when the path has default attributes, which is the common case.
    file_attrs: Option<Box<crate::crlf::FileAttrs>>,
}

impl StagingHashJob {
    fn new(entry_index: usize, worktree_mode: u32, file_attrs: crate::crlf::FileAttrs) -> Self {
        let file_attrs =
            (file_attrs != crate::crlf::FileAttrs::default()).then(|| Box::new(file_attrs));
        Self {
            entry_index,
            worktree_mode,
            file_attrs,
        }
    }
}

/// Where the parallel pass finds the index entries and files its jobs name.
struct StagingHashSource<'a> {
    index: &'a Index,
    work_tree: &'a Path,
    precompose_unicode: bool,
    ignorecase: bool,
}

#[allow(clippy::too_many_arguments)]
fn parallel_process_blob_worktree_jobs(
    odb: &Odb,
    source: &StagingHashSource<'_>,
    jobs: &[StagingHashJob],
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    materialize_dirty_blobs: bool,
    hash_blobs_without_write: bool,
    for_status: bool,
    diagnostics: Option<&crate::diagnostics::DiagnosticsHandle>,
    conv: &crate::crlf::ConversionConfig,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
    _icase_collisions: Option<&crate::path_icase::IcaseCheckoutCollisionMap>,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    use crate::parallel::{try_par_map, ParallelHashError, ParallelOp};

    if jobs.is_empty() {
        return Ok(());
    }
    let mut par_opts = thread_policy.resolve(ParallelOp::IndexRefresh, config);
    par_opts.min_items = 0;
    par_opts.min_total_bytes = None;
    let est_bytes = jobs.len().saturating_mul(8192);
    let store_dirty_blobs = (materialize_dirty_blobs && !hash_blobs_without_write)
        .then(|| odb.bulk_stage_write_options());
    let stream_from = if store_dirty_blobs.is_some() {
        odb.big_file_threshold()?
    } else {
        hash_stream_threshold(odb)?
    };
    let default_attrs = crate::crlf::FileAttrs::default();

    // Each job yields only the worktree side's oid when the entry changed; most jobs on a
    // stale index find clean files, so diff entries are built afterwards for the few changes.
    let new_oids = try_par_map::<StagingHashJob, Option<ObjectId>, Error, _>(
        jobs,
        par_opts,
        est_bytes,
        |job| {
            let entry = &source.index.entries[job.entry_index];
            let rel_path = crate::repo_relpath::storage_string(&entry.path);
            let modified = |new_oid| Ok(Some(new_oid));
            let file_path = index_entry_worktree_abs_bytes(
                source.work_tree,
                &entry.path,
                source.precompose_unicode,
                source.ignorecase,
            );
            let content = match read_regular_worktree_blob(
                odb,
                &file_path,
                conv,
                job.file_attrs.as_deref().unwrap_or(&default_attrs),
                &rel_path,
                Some(entry),
                filter_process,
                stream_from,
            ) {
                Ok(content) => content,
                Err(err)
                    if status_skips_inaccessible_worktree_error(
                        &err,
                        for_status,
                        materialize_dirty_blobs,
                    ) =>
                {
                    warn_and_skip_status_inaccessible(diagnostics, rel_path.as_str(), &err);
                    return Ok(None);
                }
                Err(err) if unreadable_worktree_blob_is_modified(&err, materialize_dirty_blobs) => {
                    return modified(zero_oid());
                }
                Err(err) => return Err(err),
            };
            let eff_oid = content.object_id(odb, store_dirty_blobs)?;
            if eff_oid != entry.oid || job.worktree_mode != entry.mode {
                let new_oid = if eff_oid != entry.oid && !materialize_dirty_blobs {
                    zero_oid()
                } else {
                    eff_oid
                };
                modified(new_oid)
            } else {
                Ok(None)
            }
        },
    )
    .map_err(|e| match e {
        ParallelHashError::Task(err) => err,
    })?;

    let mut changed: Vec<(usize, u32, ObjectId)> = jobs
        .iter()
        .zip(new_oids)
        .filter_map(|(job, new_oid)| new_oid.map(|oid| (job.entry_index, job.worktree_mode, oid)))
        .collect();
    changed.sort_by_key(|(idx, _, _)| *idx);
    result.extend(
        changed
            .into_iter()
            .map(|(entry_index, worktree_mode, new_oid)| {
                let entry = &source.index.entries[entry_index];
                let rel_path = crate::repo_relpath::storage_string(&entry.path);
                worktree_content_change_entry(entry, &rel_path, worktree_mode, new_oid)
            }),
    );
    Ok(())
}

/// Index-vs-worktree entry for a tracked path whose content or mode differs from `ie`.
fn worktree_content_change_entry(
    ie: &IndexEntry,
    path: &str,
    worktree_mode: u32,
    new_oid: ObjectId,
) -> DiffEntry {
    DiffEntry {
        status: DiffStatus::for_content_change(ie.mode, worktree_mode),
        old_path: Some(path.to_owned()),
        new_path: Some(path.to_owned()),
        old_mode: FileMode::new(ie.mode),
        new_mode: FileMode::new(worktree_mode),
        old_oid: ie.oid,
        new_oid,
        score: None,
    }
}

/// Whether a failed read of a tracked file's content reports the path as modified (with an
/// unknown worktree oid) instead of failing the whole scan.
///
/// A file that cannot be opened (no permission, held exclusively by another process) cannot
/// be proven to match the index, so it counts as changed, as in Git. Callers that need the
/// bytes themselves (`materialize_dirty_blobs`, i.e. staging) still get the error.
fn unreadable_worktree_blob_is_modified(err: &Error, materialize_dirty_blobs: bool) -> bool {
    !materialize_dirty_blobs
        && matches!(
            err,
            Error::WorktreePathIo {
                operation: crate::worktree::WorktreePathOperation::Read,
                ..
            }
        )
}

fn warn_worktree_access_failed(
    diagnostics: Option<&crate::diagnostics::DiagnosticsHandle>,
    path: &str,
    err: &std::io::Error,
) {
    if let Some(handle) = diagnostics {
        crate::diagnostics::warn(
            handle,
            crate::diagnostics::Warning::WorktreeAccessFailed {
                path: path.to_owned(),
                detail: err.to_string(),
            },
        );
    }
}

fn blob_disk_lookup_io_result(
    diagnostics: Option<&crate::diagnostics::DiagnosticsHandle>,
    path: &str,
    err: std::io::Error,
    for_status: bool,
    materialize_dirty_blobs: bool,
) -> Result<()> {
    if for_status && !materialize_dirty_blobs {
        warn_worktree_access_failed(diagnostics, path, &err);
        return Ok(());
    }
    Err(Error::Io(err))
}

fn io_err_is_permission_denied(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
}

fn status_skips_inaccessible_worktree_error(
    err: &Error,
    for_status: bool,
    materialize_dirty_blobs: bool,
) -> bool {
    if !for_status || materialize_dirty_blobs {
        return false;
    }
    match err {
        Error::Io(e) => io_err_is_permission_denied(e),
        Error::WorktreePathIo {
            operation: crate::worktree::WorktreePathOperation::Metadata,
            source,
            ..
        } => io_err_is_permission_denied(source),
        _ => false,
    }
}

fn warn_and_skip_status_inaccessible(
    diagnostics: Option<&crate::diagnostics::DiagnosticsHandle>,
    path: &str,
    err: &Error,
) -> bool {
    let detail = match err {
        Error::Io(e) => e.to_string(),
        Error::WorktreePathIo { source, .. } => source.to_string(),
        _ => return false,
    };
    if let Some(handle) = diagnostics {
        crate::diagnostics::warn(
            handle,
            crate::diagnostics::Warning::WorktreeAccessFailed {
                path: path.to_owned(),
                detail,
            },
        );
    }
    true
}

fn symlink_metadata_worktree_diff(
    file_path: &Path,
    path_str: &str,
    diagnostics: Option<&crate::diagnostics::DiagnosticsHandle>,
    for_status: bool,
    materialize_dirty_blobs: bool,
) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(file_path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
        {
            Ok(None)
        }
        Err(e) => {
            if for_status && !materialize_dirty_blobs && io_err_is_permission_denied(&e) {
                warn_worktree_access_failed(diagnostics, path_str, &e);
                Ok(None)
            } else {
                Err(Error::Io(e))
            }
        }
    }
}

/// Whether status may report `ie` modified because the worktree size (already munged to the
/// index's 32-bit form) differs from the cached one, without reading the file (Git
/// `compare_stat_data`).
///
/// Only sound when clean conversion is the identity: a line-ending rule, filter, encoding or
/// `ident` can turn a file of another size into exactly the indexed blob. The
/// `conversion_is_identity` check runs only once the sizes differ, so clean entries never
/// resolve attributes here.
fn status_size_change_proves_modified(
    ie: &IndexEntry,
    worktree_size: u32,
    for_status: bool,
    conversion_is_identity: impl FnOnce() -> bool,
) -> bool {
    for_status && !ie.stat_cache_unset() && worktree_size != ie.size && conversion_is_identity()
}

/// Report `ie` modified when [`status_size_change_proves_modified`] holds; returns whether it
/// did. The worktree side uses the null OID because the file was not hashed.
#[allow(clippy::too_many_arguments)]
fn try_push_status_modified_on_size_mismatch(
    odb: &Odb,
    ie: &IndexEntry,
    path_str_ref: &str,
    worktree_size: u32,
    worktree_mode: u32,
    raw_worktree_len: u64,
    for_status: bool,
    conversion_is_identity: impl FnOnce() -> bool,
    result: &mut Vec<DiffEntry>,
) -> Result<bool> {
    let identity = conversion_is_identity();
    let size_differs = worktree_size != ie.size;
    if !size_differs || !identity {
        return Ok(false);
    }
    let threshold = odb.big_file_threshold()?;
    let big_file = raw_worktree_len > threshold;
    let proves_modified = if big_file {
        true
    } else {
        status_size_change_proves_modified(ie, worktree_size, for_status, || identity)
    };
    if !proves_modified {
        return Ok(false);
    }
    result.push(worktree_content_change_entry(
        ie,
        path_str_ref,
        worktree_mode,
        zero_oid(),
    ));
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn push_index_blob_worktree_diff(
    odb: &Odb,
    ie: &IndexEntry,
    path_str_ref: &str,
    file_path: &Path,
    meta: fs::Metadata,
    index_mtime: Option<(u32, u32)>,
    mode_policy: crate::index::WorktreeModePolicy,
    stat_policy: crate::index::StatMatchPolicy,
    file_attrs_for: &dyn Fn(&str) -> crate::crlf::FileAttrs,
    conv: &crate::crlf::ConversionConfig,
    materialize_dirty_blobs: bool,
    hash_blobs_without_write: bool,
    for_status: bool,
    diagnostics: Option<&crate::diagnostics::DiagnosticsHandle>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
    icase_collisions: Option<&crate::path_icase::IcaseCheckoutCollisionMap>,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    if meta.is_dir() {
        if file_path.join(".git").exists() {
            let head = read_submodule_head_oid(file_path).unwrap_or_else(zero_oid);
            let path_owned = path_str_ref.to_owned();
            result.push(DiffEntry {
                status: DiffStatus::TypeChanged,
                old_path: Some(path_owned.clone()),
                new_path: Some(path_owned),
                old_mode: FileMode::new(ie.mode),
                new_mode: FileMode::new(0o160000),
                old_oid: ie.oid,
                new_oid: head,
                score: None,
            });
            return Ok(());
        }
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path_str_ref.to_owned()),
            new_path: None,
            old_mode: FileMode::new(ie.mode),
            new_mode: FileMode::ABSENT,
            old_oid: ie.oid,
            new_oid: zero_oid(),
            score: None,
        });
        return Ok(());
    }

    let worktree_mode =
        mode_policy.index_mode_for_worktree(mode_from_metadata(&meta), Some(ie.mode));
    let stat_same = stat_matches_with_policy(ie, &meta, stat_policy);
    if stat_same && worktree_mode != ie.mode {
        result.push(worktree_content_change_entry(
            ie,
            path_str_ref,
            worktree_mode,
            ie.oid,
        ));
        return Ok(());
    }

    if stat_same
        && index_entry_skips_worktree_content_hash_after_stat_match(ie, index_mtime, stat_policy)
    {
        return Ok(());
    }

    let file_attrs = file_attrs_for(path_str_ref);
    if !icase_collisions.is_some_and(|map| map.diff_requires_worktree_hash(ie.path.as_slice()))
        && try_push_status_modified_on_size_mismatch(
            odb,
            ie,
            path_str_ref,
            crate::index::munge_index_size(meta.len()),
            worktree_mode,
            meta.len(),
            for_status,
            || crate::crlf::git_conversion_is_identity(conv, &file_attrs),
            result,
        )?
    {
        return Ok(());
    }

    let store = (materialize_dirty_blobs && !hash_blobs_without_write)
        .then(|| odb.bulk_stage_write_options());
    let stream_from = if store.is_some() {
        odb.big_file_threshold()?
    } else {
        hash_stream_threshold(odb)?
    };
    let content = match worktree_blob_content_from(
        odb,
        file_path,
        &meta,
        conv,
        &file_attrs,
        path_str_ref,
        Some(ie),
        filter_process,
        stream_from,
    ) {
        Ok(content) => content,
        Err(err)
            if status_skips_inaccessible_worktree_error(
                &err,
                for_status,
                materialize_dirty_blobs,
            ) =>
        {
            warn_and_skip_status_inaccessible(diagnostics, path_str_ref, &err);
            return Ok(());
        }
        Err(err) if unreadable_worktree_blob_is_modified(&err, materialize_dirty_blobs) => {
            result.push(worktree_content_change_entry(
                ie,
                path_str_ref,
                worktree_mode,
                zero_oid(),
            ));
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    let eff_oid = content.object_id(odb, store)?;

    if eff_oid != ie.oid || worktree_mode != ie.mode {
        // `git diff-files --raw` uses the null OID for the worktree side when content differs.
        // Staging (`diff_index_to_worktree_for_staging`) materializes blobs and needs the real OID.
        let new_oid = if eff_oid != ie.oid && !materialize_dirty_blobs {
            zero_oid()
        } else {
            eff_oid
        };
        result.push(worktree_content_change_entry(
            ie,
            path_str_ref,
            worktree_mode,
            new_oid,
        ));
    }
    Ok(())
}

/// Compare tracked index entries with the worktree, using link-target bytes for symlinks.
///
/// Symlink targets bypass regular-file filters and line-ending conversion. Parallel and
/// serial scans use the same file-type classification, including directory replacements
/// and regular-file placeholders checked out with `core.symlinks=false`.
///
/// # Errors
///
/// Returns errors from I/O or hashing.
pub fn diff_index_to_worktree_with_options(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    options: DiffIndexToWorktreeOptions,
) -> Result<(Vec<DiffEntry>, bool)> {
    let default_git_dir = work_tree.join(".git");
    let git_dir = options
        .repository_git_dir
        .clone()
        .unwrap_or(default_git_dir);
    diff_index_to_worktree_inner(odb, index, work_tree, &git_dir, options, false, None)
        .map(|(entries, changed, _)| (entries, changed))
}

/// Like [`diff_index_to_worktree_with_options`], with an owner-thread progress poll hook for
/// long stat scans (used by [`crate::porcelain::status::status`]).
pub(crate) fn diff_index_to_worktree_with_owner_poll(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    options: DiffIndexToWorktreeOptions,
    owner_poll: Option<std::sync::Arc<crate::progress::StackCancelPoll>>,
) -> Result<(Vec<DiffEntry>, bool)> {
    let default_git_dir = work_tree.join(".git");
    let git_dir = options
        .repository_git_dir
        .clone()
        .unwrap_or(default_git_dir);
    diff_index_to_worktree_inner(odb, index, work_tree, &git_dir, options, false, owner_poll)
        .map(|(entries, changed, _)| (entries, changed))
}

/// Like [`diff_index_to_worktree_with_options`], but loads config from `repository_git_dir`
/// (the main repository for linked worktrees) and persists each hashed worktree blob into `odb`
/// so staging can update the index without re-reading file contents.
/// Symlinks store their target bytes. With [`DiffIndexToWorktreeOptions::hash_blobs_without_write`],
/// returns the same blob IDs without writing objects, for staging previews.
pub fn diff_index_to_worktree_for_staging(
    odb: &Odb,
    repository_git_dir: &Path,
    index: &mut Index,
    work_tree: &Path,
    options: DiffIndexToWorktreeOptions,
) -> Result<(
    Vec<DiffEntry>,
    bool,
    std::collections::BTreeMap<String, crate::index::IndexStatFields>,
)> {
    diff_index_to_worktree_inner(
        odb,
        index,
        work_tree,
        repository_git_dir,
        options,
        true,
        None,
    )
}

/// First pass of [`diff_index_to_worktree_inner`]: gitlinks and intent-to-add paths only.
#[allow(clippy::too_many_arguments)]
fn worktree_diff_pass1_index_entry(
    odb: &Odb,
    work_tree: &Path,
    ie: &IndexEntry,
    path_str_ref: &str,
    simplify_gitlinks: bool,
    ignore_submodule_untracked: bool,
    error_on_broken_gitlinks: bool,
    materialize_dirty_blobs: bool,
    filter_fp: Option<&crate::filter_process::FilterProcessState>,
    conv: &crate::crlf::ConversionConfig,
    file_attrs_for: &dyn Fn(&str) -> crate::crlf::FileAttrs,
    precompose_unicode: bool,
    ignorecase: bool,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    if ie.skip_worktree() || ie.assume_unchanged() {
        return Ok(());
    }
    let is_intent_to_add = ie.intent_to_add();

    if ie.mode == 0o160000 {
        let sub_dir =
            index_entry_worktree_abs_bytes(work_tree, &ie.path, precompose_unicode, ignorecase);
        match fs::symlink_metadata(&sub_dir) {
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
            {
                if !simplify_gitlinks {
                    let path_owned = path_str_ref.to_owned();
                    result.push(DiffEntry {
                        status: DiffStatus::Deleted,
                        old_path: Some(path_owned.clone()),
                        new_path: Some(path_owned),
                        old_mode: FileMode::new(ie.mode),
                        new_mode: FileMode::ABSENT,
                        old_oid: ie.oid,
                        new_oid: zero_oid(),
                        score: None,
                    });
                }
                return Ok(());
            }
            Err(e) => return Err(Error::Io(e)),
            Ok(meta) if !meta.is_dir() => {
                let worktree_mode = mode_from_metadata(&meta);
                let file_attrs = file_attrs_for(path_str_ref);
                let worktree_oid = worktree_file_oid(
                    odb,
                    &sub_dir,
                    &meta,
                    conv,
                    &file_attrs,
                    path_str_ref,
                    None,
                    materialize_dirty_blobs,
                    filter_fp,
                )?;
                let path_owned = path_str_ref.to_owned();
                result.push(DiffEntry {
                    status: DiffStatus::TypeChanged,
                    old_path: Some(path_owned.clone()),
                    new_path: Some(path_owned),
                    old_mode: FileMode::new(ie.mode),
                    new_mode: FileMode::new(worktree_mode),
                    old_oid: ie.oid,
                    new_oid: worktree_oid,
                    score: None,
                });
                return Ok(());
            }
            Ok(_) => {}
        }
        let sub_head_oid = read_submodule_head_oid(&sub_dir);
        let ref_matches = if let Some(oid) = sub_head_oid {
            oid == ie.oid
        } else {
            if error_on_broken_gitlinks && submodule_embedded_git_dir(&sub_dir).is_some() {
                return Err(Error::Config(
                    format!("could not read submodule HEAD for '{path_str_ref}'").into(),
                ));
            }
            uninitialized_gitlink_worktree_matches_index(&sub_dir)
        };
        if simplify_gitlinks {
            if !ref_matches {
                let path_owned = path_str_ref.to_owned();
                let new_oid = sub_head_oid.unwrap_or_else(zero_oid);
                result.push(DiffEntry {
                    status: DiffStatus::Modified,
                    old_path: Some(path_owned.clone()),
                    new_path: Some(path_owned),
                    old_mode: FileMode::new(ie.mode),
                    new_mode: FileMode::new(ie.mode),
                    old_oid: ie.oid,
                    new_oid,
                    score: None,
                });
            }
            return Ok(());
        }
        if sub_head_oid.is_some() && submodule_head_object_broken(&sub_dir) {
            return Err(Error::Config(
                format!("'git status --porcelain=2' failed in submodule {path_str_ref}").into(),
            ));
        }
        let mut flags = submodule_porcelain_flags(work_tree, path_str_ref, ie.oid);
        if ignore_submodule_untracked {
            flags.untracked = false;
        }
        let inner_dirty = flags.modified || flags.untracked;
        if !ref_matches || inner_dirty {
            let path_owned = path_str_ref.to_owned();
            let new_oid = if !ref_matches {
                sub_head_oid.unwrap_or_else(zero_oid)
            } else {
                zero_oid()
            };
            result.push(DiffEntry {
                status: DiffStatus::Modified,
                old_path: Some(path_owned.clone()),
                new_path: Some(path_owned),
                old_mode: FileMode::new(ie.mode),
                new_mode: FileMode::new(ie.mode),
                old_oid: ie.oid,
                new_oid,
                score: None,
            });
        }
        return Ok(());
    }

    let file_path =
        index_entry_worktree_abs_bytes(work_tree, &ie.path, precompose_unicode, ignorecase);
    let path_match = crate::repo_relpath::as_utf8_for_matching(path_str_ref);

    if is_intent_to_add {
        match fs::symlink_metadata(&file_path) {
            Ok(meta) => {
                let file_attrs = file_attrs_for(path_match.as_ref());
                let worktree_oid = worktree_file_oid(
                    odb,
                    &file_path,
                    &meta,
                    conv,
                    &file_attrs,
                    path_match.as_ref(),
                    None,
                    materialize_dirty_blobs,
                    filter_fp,
                )?;
                let worktree_mode = mode_from_metadata(&meta);
                result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path_str_ref.to_owned()),
                    old_mode: FileMode::ABSENT,
                    new_mode: FileMode::new(worktree_mode),
                    old_oid: zero_oid(),
                    new_oid: worktree_oid,
                    score: None,
                });
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
            {
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path_str_ref.to_owned()),
                    new_path: None,
                    old_mode: FileMode::new(ie.mode),
                    new_mode: FileMode::ABSENT,
                    old_oid: ie.oid,
                    new_oid: zero_oid(),
                    score: None,
                });
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(())
}

#[inline]
fn index_entry_needs_worktree_diff_pass1(ie: &IndexEntry) -> bool {
    ie.mode == crate::index::MODE_GITLINK || ie.intent_to_add()
}

fn diff_index_to_worktree_inner(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    repository_git_dir: &Path,
    options: DiffIndexToWorktreeOptions,
    materialize_dirty_blobs: bool,
    owner_poll: Option<std::sync::Arc<crate::progress::StackCancelPoll>>,
) -> Result<(
    Vec<DiffEntry>,
    bool,
    std::collections::BTreeMap<String, crate::index::IndexStatFields>,
)> {
    use crate::config::ConfigSet;
    use crate::crlf;
    use std::collections::BTreeMap;

    let mut staging_stat_cache = BTreeMap::new();
    poll_diff_cancel(&options, owner_poll.as_ref())?;
    let index_mtime = index_mtime_for_diff(index, options.index_mtime);

    let ignore_submodule_untracked = options.ignore_submodule_untracked;
    let simplify_gitlinks = options.simplify_gitlinks;

    let rules_lock = options
        .worktree_rules
        .as_ref()
        .map(|shared| {
            shared
                .lock()
                .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))
        })
        .transpose()?;
    let config = if let Some(rules) = rules_lock.as_ref() {
        rules.config().clone()
    } else if let Some(c) = options.config.as_ref() {
        c.as_ref().clone()
    } else {
        ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(repository_git_dir),
            true,
        )
        .unwrap_or_else(|_| ConfigSet::new())
    };
    let conv = rules_lock
        .as_ref()
        .map(|r| r.conversion().clone())
        .unwrap_or_else(|| crlf::ConversionConfig::from_config(&config));
    let legacy_attrs = if rules_lock.is_none() {
        Some(crlf::load_gitattributes_parsed(work_tree))
    } else {
        None
    };
    let file_attrs_for = |path: &str| -> crlf::FileAttrs {
        if let Some(rules) = rules_lock.as_ref() {
            rules.file_attrs(path, false)
        } else if let Some(parsed) = legacy_attrs.as_ref() {
            crlf::file_attrs_from_gitattributes(&parsed.rules, &parsed.macros, path, false, &config)
        } else {
            crlf::FileAttrs::default()
        }
    };
    let filter_fp = rules_lock.as_ref().map(|r| r.filter_process());
    let precompose_unicode = config
        .get_bool("core.precomposeunicode")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let mode_policy = crate::index::WorktreeModePolicy::from_config(&config);
    let stat_policy = crate::index::StatMatchPolicy::from_config(Some(&config));
    let ignorecase = config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let icase_collisions = ignorecase
        .then(|| crate::path_icase::IcaseCheckoutCollisionMap::from_index(work_tree, index, true));
    let hash_blobs_without_write = options.hash_blobs_without_write;

    let mut result = Vec::new();
    let refresh_index_stat_in_pass = options.refresh_index_stat_in_pass;
    let mut index_stat_refresh_changed = false;
    let mut honor_fsmonitor_valid = false;
    if options.refresh_fsmonitor {
        let default_runner = crate::command_runner::system_command_runner();
        let runner = options
            .command_runner
            .as_deref()
            .unwrap_or(default_runner.as_ref());
        let boundary = options.fsmonitor_query_boundary.unwrap_or_else(|| {
            crate::fsmonitor::FsmonitorQueryBoundary(
                crate::environment::wall_clock_unix_timestamp_nanos(),
            )
        });
        let refresh_opts = crate::fsmonitor::FsmonitorRefreshOptions::with_query_boundary(boundary);
        let refresh = crate::fsmonitor::refresh_index(
            index,
            repository_git_dir,
            work_tree,
            &config,
            Some(runner),
            &refresh_opts,
        )?;
        if refresh.index_changed {
            index_stat_refresh_changed = true;
        }
        honor_fsmonitor_valid = refresh.honor_valid_bits;
    }
    let use_fsmonitor_stat_skip = honor_fsmonitor_valid;
    let mut unmerged_base: std::collections::BTreeMap<String, (u8, IndexEntry)> =
        std::collections::BTreeMap::new();

    let error_on_broken_gitlinks = options.error_on_broken_gitlinks;
    if options.restrict_to_paths.is_none() {
        for ie in &index.entries {
            poll_diff_cancel(&options, owner_poll.as_ref())?;
            if ie.stage() != 0 {
                let path = String::from_utf8_lossy(&ie.path).to_string();
                let rank = match ie.stage() {
                    2 => 0u8,
                    3 => 1u8,
                    1 => 2u8,
                    _ => 3u8,
                };
                match unmerged_base.get(&path) {
                    Some((existing_rank, _)) if *existing_rank <= rank => {}
                    _ => {
                        unmerged_base.insert(path, (rank, ie.clone()));
                    }
                }
            }
        }
    }

    if let Some(limit) = options.restrict_to_paths.as_ref() {
        for path in limit.iter() {
            let Some(ie) = index.get(path.as_bytes(), 0) else {
                continue;
            };
            if !index_entry_needs_worktree_diff_pass1(ie) {
                continue;
            }
            worktree_diff_pass1_index_entry(
                odb,
                work_tree,
                ie,
                path.as_str(),
                simplify_gitlinks,
                ignore_submodule_untracked,
                error_on_broken_gitlinks,
                materialize_dirty_blobs,
                filter_fp,
                &conv,
                &file_attrs_for,
                precompose_unicode,
                ignorecase,
                &mut result,
            )?;
        }
    } else {
        for ie in &index.entries {
            poll_diff_cancel(&options, owner_poll.as_ref())?;
            if ie.stage() != 0 {
                continue;
            }
            if !index_entry_needs_worktree_diff_pass1(ie) {
                continue;
            }
            let path_stored = crate::repo_relpath::storage_string(&ie.path);
            worktree_diff_pass1_index_entry(
                odb,
                work_tree,
                ie,
                &path_stored,
                simplify_gitlinks,
                ignore_submodule_untracked,
                error_on_broken_gitlinks,
                materialize_dirty_blobs,
                filter_fp,
                &conv,
                &file_attrs_for,
                precompose_unicode,
                ignorecase,
                &mut result,
            )?;
        }
    }

    use crate::worktree_scan::{
        collect_blob_lookup_batches_with_literal_paths, group_blob_entries_by_dir,
        stat_scan_thread_count, visit_blob_lookup_batches, BlobDiskLookup, BlobScanSparseMode,
        WorktreeBlobScanOptions, PARALLEL_STAT_MIN_ENTRIES,
    };
    let preload_index = config
        .get_bool("core.preloadIndex")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let blob_scan = WorktreeBlobScanOptions {
        preload_index,
        stat_parallel_threads: options.stat_parallel_threads,
    };
    let blob_dirs = if let Some(limit) = options.restrict_to_paths.as_ref() {
        crate::worktree_scan::group_blob_entries_by_dir_for_paths(index, limit)
    } else {
        group_blob_entries_by_dir(index, honor_fsmonitor_valid)
    };
    let work_tree_owned = work_tree.to_path_buf();
    let dir_abs = move |dir: &[u8]| {
        if dir.is_empty() {
            work_tree_owned.clone()
        } else {
            index_entry_worktree_abs_bytes(&work_tree_owned, dir, precompose_unicode, ignorecase)
        }
    };
    let work_tree_for_files = work_tree.to_path_buf();
    let file_abs = move |rel: &[u8]| {
        index_entry_worktree_abs_bytes(&work_tree_for_files, rel, precompose_unicode, ignorecase)
    };
    let blob_count = blob_dirs.len();
    let thread_policy = options.thread_policy.unwrap_or_default();
    let hash_threads = stat_scan_thread_count(&thread_policy, &config, blob_scan, blob_count);
    let parallel_blob_work = blob_count >= PARALLEL_STAT_MIN_ENTRIES && hash_threads > 1;
    let mut staging_hash_jobs: Vec<StagingHashJob> = Vec::new();
    // Entries the stat refresh just hashed and found matching their recorded blob.
    let mut refresh_content_verified = vec![false; index.entries.len()];
    // Parallel preload must lstat ancestor directories without following symlinks. When a
    // tracked directory is replaced by a symlink, indexed paths under it are deletions even if
    // the link target still holds a file at the same relative path with an unchanged stat.
    // This is independent of `core.symlinks` (checkout support); detection must stay enabled.
    let sparse = if parallel_blob_work {
        BlobScanSparseMode::StatusDiff {
            stat_policy,
            index_mtime,
            mode_policy,
            for_status: options.for_status,
            refresh_index_stat_in_pass,
            parallel_blob_work,
            honor_fsmonitor_stat_skip: use_fsmonitor_stat_skip,
        }
    } else {
        BlobScanSparseMode::Dense
    };
    let blob_stat_batches = collect_blob_lookup_batches_with_literal_paths(
        index,
        &blob_dirs,
        blob_scan,
        &thread_policy,
        &config,
        dir_abs,
        file_abs,
        !precompose_unicode && !ignorecase,
        || {
            let mut dir_symlinks = SymlinkDirCache::default();
            move |rel_path: &[u8]| {
                dir_symlinks.has_symlink_in_path_bytes(
                    work_tree,
                    rel_path,
                    precompose_unicode,
                    ignorecase,
                )
            }
        },
        sparse,
        options.cancel_watch.clone(),
        owner_poll.clone(),
    )?;
    // Size-changed entries ask whether their clean conversion is the identity in both the stat
    // refresh and the visit below; resolve each one's attributes once.
    let identity_by_entry = std::cell::RefCell::new(std::collections::HashMap::new());
    let conversion_is_identity = |entry_index: usize, path: &str| -> bool {
        *identity_by_entry
            .borrow_mut()
            .entry(entry_index)
            .or_insert_with(|| crlf::git_conversion_is_identity(&conv, &file_attrs_for(path)))
    };
    let refresh_checks = if refresh_index_stat_in_pass {
        let par_opts =
            refresh_hash_parallel_options(&thread_policy, &config, options.stat_parallel_threads);
        refresh_content_checks(
            odb,
            index,
            &blob_stat_batches,
            index_mtime,
            stat_policy,
            &|entry_index, ie, stat| {
                !(use_fsmonitor_stat_skip && crate::fsmonitor::entry_skips_worktree_stat(ie, index))
                    && worktree_diff_refreshes_entry(
                        ie,
                        stat,
                        index_mtime,
                        stat_policy,
                        options.for_status,
                        || {
                            conversion_is_identity(
                                entry_index,
                                &crate::repo_relpath::storage_string(&ie.path),
                            )
                        },
                    )
            },
            &WorktreeHashContext {
                work_tree,
                precompose_unicode,
                ignorecase,
                conv: &conv,
                file_attrs_for: &file_attrs_for,
                filter_process: filter_fp,
            },
            par_opts,
            options.cancel_watch.as_ref(),
        )?
    } else {
        RefreshChecks::default()
    };
    poll_diff_cancel(&options, owner_poll.as_ref())?;
    visit_blob_lookup_batches(blob_stat_batches, |entry_index, lookup| {
        poll_diff_cancel(&options, owner_poll.as_ref())?;
        let path_owned = crate::repo_relpath::storage_string(&index.entries[entry_index].path);
        let path_str_ref = path_owned.as_str();
        if use_fsmonitor_stat_skip {
            let ie_skip = &index.entries[entry_index];
            if crate::fsmonitor::entry_skips_worktree_stat(ie_skip, index) {
                return Ok(());
            }
        }
        match lookup {
            BlobDiskLookup::Missing => {
                if icase_collisions.as_ref().is_some_and(|map| {
                    map.preserve_missing_collision_loser(&index.entries[entry_index].path)
                }) {
                    return Ok(());
                }
                let ie = &index.entries[entry_index];
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path_str_ref.to_owned()),
                    new_path: None,
                    old_mode: FileMode::new(ie.mode),
                    new_mode: FileMode::ABSENT,
                    old_oid: ie.oid,
                    new_oid: zero_oid(),
                    score: None,
                });
            }
            BlobDiskLookup::Io(e) => {
                blob_disk_lookup_io_result(
                    options.diagnostics.as_ref(),
                    path_str_ref,
                    e,
                    options.for_status,
                    materialize_dirty_blobs,
                )?;
            }
            BlobDiskLookup::Present(stat) => {
                if materialize_dirty_blobs {
                    staging_stat_cache.insert(path_str_ref.to_owned(), stat.fields);
                }
                if refresh_index_stat_in_pass
                    && worktree_diff_refreshes_entry(
                        &index.entries[entry_index],
                        &stat,
                        index_mtime,
                        stat_policy,
                        options.for_status,
                        || conversion_is_identity(entry_index, path_str_ref),
                    )
                {
                    let content_matches = RefreshContent::matches(
                        refresh_checks.get(entry_index),
                        &index.entries[entry_index].oid,
                    );
                    let refresh = refresh_index_blob_stat_from_scan(
                        &mut index.entries[entry_index],
                        &stat,
                        index_mtime,
                        stat_policy,
                        content_matches,
                    );
                    if refresh.index_changed {
                        index_stat_refresh_changed = true;
                    }
                    if refresh.content_verified {
                        refresh_content_verified[entry_index] = true;
                        let path = index.entries[entry_index].path.clone();
                        let oid = index.entries[entry_index].oid;
                        index.mark_content_verified(&path, oid);
                    }
                    if index.fsmonitor_last_update.is_some()
                        && stat_matches_blob_stat(&index.entries[entry_index], &stat, stat_policy)
                        && !refresh.index_changed
                    {
                        index.entries[entry_index].set_fsmonitor_valid(true);
                    }
                }
                let ie = &index.entries[entry_index];
                let worktree_mode =
                    mode_policy.index_mode_for_worktree(stat.git_mode, Some(ie.mode));
                // The stat refresh already hashed this file and found other content, so do not
                // read it again. Staging hashes again because it needs the blob stored.
                if !materialize_dirty_blobs
                    && !stat.is_dir
                    && stat.git_mode != crate::index::MODE_SYMLINK
                    && RefreshContent::differs(refresh_checks.get(entry_index), &ie.oid)
                {
                    result.push(worktree_content_change_entry(
                        ie,
                        path_str_ref,
                        worktree_mode,
                        zero_oid(),
                    ));
                    return Ok(());
                }
                // Streaming jobs read regular files. Links and directories need the shared
                // classifier; use the physical type so core.symlinks=false placeholders
                // keep the regular-file fast path even with an effective symlink mode.
                if parallel_blob_work && !stat.is_dir && stat.git_mode != crate::index::MODE_SYMLINK
                {
                    let stat_same = stat_matches_blob_stat(ie, &stat, stat_policy);
                    if stat_same
                        && worktree_mode == ie.mode
                        && index_entry_skips_worktree_content_hash_after_stat_match(
                            ie,
                            index_mtime,
                            stat_policy,
                        )
                    {
                        return Ok(());
                    }
                    if stat_same && worktree_mode != ie.mode {
                        result.push(worktree_content_change_entry(
                            ie,
                            path_str_ref,
                            worktree_mode,
                            ie.oid,
                        ));
                    } else if !icase_collisions.as_ref().is_some_and(|map| {
                        map.diff_requires_worktree_hash(&index.entries[entry_index].path)
                    }) && try_push_status_modified_on_size_mismatch(
                        odb,
                        ie,
                        path_str_ref,
                        stat.worktree_size(),
                        worktree_mode,
                        stat.raw_size,
                        options.for_status,
                        || conversion_is_identity(entry_index, path_str_ref),
                        &mut result,
                    )? {
                    } else if !refresh_content_verified[entry_index] {
                        staging_hash_jobs.push(StagingHashJob::new(
                            entry_index,
                            worktree_mode,
                            file_attrs_for(path_str_ref),
                        ));
                    }
                } else {
                    // The scan's lstat already proves an unchanged, non-racy entry clean, and the
                    // stat refresh above may have just verified a racy entry by content; only
                    // re-stat paths that `push_index_blob_worktree_diff` has to inspect.
                    let same_file_type = (worktree_mode == crate::index::MODE_SYMLINK)
                        == (ie.mode == crate::index::MODE_SYMLINK);
                    if !stat.is_dir
                        && worktree_mode == ie.mode
                        && ((refresh_content_verified[entry_index] && same_file_type)
                            || (stat_matches_blob_stat(ie, &stat, stat_policy)
                                && index_entry_skips_worktree_content_hash_after_stat_match(
                                    ie,
                                    index_mtime,
                                    stat_policy,
                                )))
                    {
                        return Ok(());
                    }
                    let file_path = index_entry_worktree_abs_bytes(
                        work_tree,
                        &index.entries[entry_index].path,
                        precompose_unicode,
                        ignorecase,
                    );
                    let Some(meta) = symlink_metadata_worktree_diff(
                        &file_path,
                        path_str_ref,
                        options.diagnostics.as_ref(),
                        options.for_status,
                        materialize_dirty_blobs,
                    )?
                    else {
                        return Ok(());
                    };
                    push_index_blob_worktree_diff(
                        odb,
                        ie,
                        path_str_ref,
                        &file_path,
                        meta,
                        index_mtime,
                        mode_policy,
                        stat_policy,
                        &file_attrs_for,
                        &conv,
                        materialize_dirty_blobs,
                        hash_blobs_without_write,
                        options.for_status,
                        options.diagnostics.as_ref(),
                        filter_fp,
                        icase_collisions.as_ref(),
                        &mut result,
                    )?;
                }
            }
        }
        Ok(())
    })?;
    if parallel_blob_work && !staging_hash_jobs.is_empty() {
        parallel_process_blob_worktree_jobs(
            odb,
            &StagingHashSource {
                index,
                work_tree,
                precompose_unicode,
                ignorecase,
            },
            &staging_hash_jobs,
            &thread_policy,
            &config,
            materialize_dirty_blobs,
            hash_blobs_without_write,
            options.for_status,
            options.diagnostics.as_ref(),
            &conv,
            filter_fp,
            icase_collisions.as_ref(),
            &mut result,
        )?;
    }

    for (path, (_, base_entry)) in unmerged_base {
        let file_path = index_entry_worktree_abs(work_tree, &path, precompose_unicode, ignorecase);
        let wt_meta = match fs::symlink_metadata(&file_path) {
            Ok(meta) => Some(meta),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
            {
                None
            }
            Err(e) => return Err(Error::Io(e)),
        };

        let new_mode = wt_meta.as_ref().map_or_else(
            || FileMode::ABSENT,
            |meta| FileMode::new(mode_from_metadata(meta)),
        );
        result.push(DiffEntry {
            status: DiffStatus::Unmerged,
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            old_mode: FileMode::ABSENT,
            new_mode,
            old_oid: zero_oid(),
            new_oid: zero_oid(),
            score: None,
        });

        if !options.for_status {
            if let Some(meta) = wt_meta {
                let file_attrs = file_attrs_for(&path);
                let wt_oid = worktree_file_oid(
                    odb,
                    &file_path,
                    &meta,
                    &conv,
                    &file_attrs,
                    &path,
                    Some(&base_entry),
                    materialize_dirty_blobs,
                    filter_fp,
                )?;
                let wt_mode = mode_from_metadata(&meta);
                if wt_oid != base_entry.oid || wt_mode != base_entry.mode {
                    result.push(DiffEntry {
                        status: DiffStatus::for_content_change(base_entry.mode, wt_mode),
                        old_path: Some(path.clone()),
                        new_path: Some(path),
                        old_mode: FileMode::new(base_entry.mode),
                        new_mode: FileMode::new(wt_mode),
                        old_oid: base_entry.oid,
                        new_oid: wt_oid,
                        score: None,
                    });
                }
            }
        }
    }

    Ok((result, index_stat_refresh_changed, staging_stat_cache))
}

/// Per-worker cache answering "is any ancestor directory of this index path a symlink?".
///
/// It remembers the last chain of ancestors verified to be real directories, plus the first
/// ancestor found not to be one. Index paths arrive in sorted order, so consecutive queries
/// share most of their leading directories and only the differing components are probed.
/// Each scan worker owns its own cache: there is no locking and no per-directory allocation.
#[derive(Default)]
pub(crate) struct SymlinkDirCache {
    /// Leading directory (raw index bytes, no trailing `/`) whose every component is a real
    /// directory on disk.
    real_dirs: Vec<u8>,
    /// First ancestor found not to be a real directory, and whether it is a symlink. Every
    /// path under it shares that answer.
    blocked: Option<(Vec<u8>, bool)>,
    /// Single-walk check for directories several components below `real_dirs`.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    chain: Option<crate::worktree_scan::RealDirChain>,
    /// Filesystem queries made so far.
    #[cfg(test)]
    fs_queries: usize,
}

/// Whether `path` is `dir` itself or lies under it (`dir` has no trailing `/`).
fn path_within_dir(path: &[u8], dir: &[u8]) -> bool {
    path.starts_with(dir) && path.get(dir.len()).is_none_or(|&b| b == b'/')
}

/// Length of the longest leading run of whole components shared by two directory paths.
fn common_dir_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let same = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let ends_component = |p: &[u8]| p.get(same).is_none_or(|&c| c == b'/');
    if ends_component(a) && ends_component(b) {
        return same;
    }
    a[..same].iter().rposition(|&c| c == b'/').unwrap_or(0)
}

pub(crate) fn index_entry_worktree_abs(
    work_tree: &Path,
    index_relpath: &str,
    precompose_unicode: bool,
    ignorecase: bool,
) -> PathBuf {
    index_entry_worktree_abs_bytes(
        work_tree,
        index_relpath.as_bytes(),
        precompose_unicode,
        ignorecase,
    )
}

pub(crate) fn index_entry_worktree_abs_bytes(
    work_tree: &Path,
    index_relpath: &[u8],
    precompose_unicode: bool,
    ignorecase: bool,
) -> PathBuf {
    if precompose_unicode || ignorecase {
        if let Ok(s) = std::str::from_utf8(index_relpath) {
            if precompose_unicode {
                return crate::unicode_normalization::resolve_worktree_path_for_staging(
                    work_tree, s, true,
                )
                .abs;
            }
            return crate::path_icase::worktree_path_for_index_entry(work_tree, s, ignorecase);
        }
    }
    crate::index::worktree_path_from_index_rel(work_tree, index_relpath)
}

/// Effective index-file mtime for racy-git checks.
///
/// Uses an explicit override when provided, otherwise [`Index::source_mtime`] from the last
/// on-disk read.
#[must_use]
pub fn index_mtime_for_diff(index: &Index, explicit: Option<(u32, u32)>) -> Option<(u32, u32)> {
    explicit.or(index.source_mtime)
}

impl SymlinkDirCache {
    /// Whether any parent directory of `rel_path` (raw index path bytes) is a symlink.
    ///
    /// Assumes the worktree does not change during the scan this cache serves. A missing
    /// ancestor, or one that is a file, is not a symlink and hides everything below it.
    pub(crate) fn has_symlink_in_path_bytes(
        &mut self,
        work_tree: &Path,
        rel_path: &[u8],
        precompose_unicode: bool,
        ignorecase: bool,
    ) -> bool {
        use crate::worktree_scan::{ancestor_kind, AncestorKind};

        let Some(dir_len) = rel_path.iter().rposition(|&b| b == b'/') else {
            return false;
        };
        let dir = &rel_path[..dir_len];
        if let Some((blocked, is_symlink)) = &self.blocked {
            if path_within_dir(dir, blocked) {
                return *is_symlink;
            }
        }
        let verified = common_dir_prefix_len(dir, &self.real_dirs);
        self.real_dirs.truncate(verified);
        #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
        if !precompose_unicode
            && !ignorecase
            && dir[verified..]
                .split(|&b| b == b'/')
                .filter(|component| !component.is_empty())
                .nth(1)
                .is_some()
        {
            let chain = match &mut self.chain {
                Some(chain) if chain.work_tree() == work_tree => chain,
                slot => slot.insert(crate::worktree_scan::RealDirChain::open(work_tree)),
            };
            #[cfg(test)]
            {
                self.fs_queries += 1;
            }
            if chain.confirms(dir) {
                self.real_dirs.clear();
                self.real_dirs.extend_from_slice(dir);
                return false;
            }
        }
        let mut start = verified;
        while start < dir.len() {
            if dir[start] == b'/' {
                start += 1;
                continue;
            }
            let end = dir[start..]
                .iter()
                .position(|&b| b == b'/')
                .map_or(dir.len(), |i| start + i);
            let prefix = &dir[..end];
            let abs =
                index_entry_worktree_abs_bytes(work_tree, prefix, precompose_unicode, ignorecase);
            #[cfg(test)]
            {
                self.fs_queries += 1;
            }
            match ancestor_kind(&abs) {
                AncestorKind::Directory => {
                    let known = self.real_dirs.len();
                    self.real_dirs.extend_from_slice(&prefix[known..]);
                }
                AncestorKind::Symlink => {
                    self.blocked = Some((prefix.to_vec(), true));
                    return true;
                }
                AncestorKind::Other => {
                    self.blocked = Some((prefix.to_vec(), false));
                    return false;
                }
            }
            start = end;
        }
        false
    }
}

/// Whether an index entry's cached mtime is at or after the on-disk index file's mtime.
///
/// Git labels such entries racy: stat data can look clean even though the worktree changed
/// within the same second the index was written. Callers pass the index file mtime sampled when
/// the index was read (before any rewrite). When `index_mtime` is `None`, returns `false`.
#[must_use]
pub fn entry_is_racy(ie: &IndexEntry, index_mtime: Option<(u32, u32)>) -> bool {
    entry_is_racy_with_policy(ie, index_mtime, crate::index::StatMatchPolicy::default())
}

/// Like [`entry_is_racy`], honoring `core.usenanosec` for sub-second racy detection.
///
/// Sub-second racy detection applies only when stat equality also compares the fractions
/// (`core.usenanosec` with `core.checkstat=default`); otherwise a same-second, same-size edit
/// would be neither racy nor a stat mismatch.
#[must_use]
pub fn entry_is_racy_with_policy(
    ie: &IndexEntry,
    index_mtime: Option<(u32, u32)>,
    policy: crate::index::StatMatchPolicy,
) -> bool {
    let Some((index_mtime_sec, index_mtime_nsec)) = index_mtime else {
        return false;
    };
    if index_mtime_sec == 0 {
        return false;
    }
    if policy.use_nanosec && policy.check_stat {
        index_mtime_sec < ie.mtime_sec
            || (index_mtime_sec == ie.mtime_sec && index_mtime_nsec <= ie.mtime_nsec)
    } else {
        index_mtime_sec <= ie.mtime_sec
    }
}

/// Clear racily-clean cache lines on entries whose blob no longer matches the worktree.
///
/// Before Git writes the index, it walks stage-0 file and symlink entries and sets the cached
/// size to zero when stat data still matches the worktree but content does not match the
/// recorded OID and the entry timestamp is racy relative to `index_mtime`. That forces the
/// next diff or status pass to re-hash the path instead of trusting stat alone.
///
/// Skip-worktree, assume-unchanged, and intent-to-add entries are left unchanged, and so are
/// entries this process just hashed from the worktree to their current OID (Git skips its
/// `CE_UPTODATE` entries the same way). When `index_mtime` is `None` (no prior on-disk index),
/// nothing is smudged.
///
/// Content equality uses `git_dir` config and worktree attributes with [`hash_worktree_file`]
/// so the repository hash algorithm and clean (CRLF/filter) conversion match index OIDs.
///
/// Returns `true` when at least one entry was smudged.
pub fn smudge_racily_clean_entries(
    odb: &Odb,
    git_dir: &Path,
    index: &mut Index,
    work_tree: &Path,
    index_mtime: Option<(u32, u32)>,
    config: Option<&ConfigSet>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> bool {
    use crate::crlf;
    use crate::index::{MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};

    let Some(index_mtime) = index_mtime else {
        return false;
    };

    let config = config.cloned().unwrap_or_else(|| {
        ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(git_dir),
            true,
        )
        .unwrap_or_default()
    });
    let conv = crlf::ConversionConfig::from_config(&config);
    let attrs = crlf::load_gitattributes(work_tree);
    let verified = std::mem::take(&mut index.content_verified);

    let mut changed = false;
    for ie in &mut index.entries {
        let marked_verified = ie.take_content_verified();
        if ie.stage() != 0 || ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
            continue;
        }
        if ie.mode != MODE_REGULAR && ie.mode != MODE_EXECUTABLE && ie.mode != MODE_SYMLINK {
            continue;
        }
        if ie.size == 0 {
            continue;
        }
        if !entry_is_racy(ie, Some(index_mtime)) {
            continue;
        }
        if marked_verified || verified.get(&ie.path) == Some(&ie.oid) {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&ie.path) else {
            continue;
        };
        let abs = work_tree.join(path);
        let Ok(meta) = fs::symlink_metadata(&abs) else {
            continue;
        };
        if !stat_matches(ie, &meta) {
            continue;
        }
        let file_attrs = crlf::get_file_attrs(&attrs, path, false, &config);
        let content_matches = hash_worktree_file(
            odb,
            &abs,
            &meta,
            &conv,
            &file_attrs,
            path,
            Some(ie),
            filter_process,
        )
        .map(|oid| oid == ie.oid)
        .unwrap_or(false);
        if content_matches {
            continue;
        }
        ie.size = 0;
        changed = true;
    }
    changed
}
/// Quick stat check: does the index entry's cached stat data match the file?
/// Returns true when the file at `ie`'s path differs from the index entry (mode or blob).
///
/// Used by commands such as `git mv` to detect "dirty" paths under sparse checkout.
/// Symlinks and submodules are compared in a Git-compatible way.
///
/// `ignore_submodule_untracked` mirrors [`diff_index_to_worktree`]'s same flag for gitlinks.
pub fn worktree_differs_from_index_entry(
    odb: &Odb,
    repository_git_dir: &Path,
    work_tree: &Path,
    ie: &IndexEntry,
    ignore_submodule_untracked: bool,
    config: Option<&ConfigSet>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<bool> {
    WorktreeEntryComparer::new(repository_git_dir, work_tree, config).differs(
        odb,
        ie,
        ignore_submodule_untracked,
        filter_process,
    )
}

/// Repeated [`worktree_differs_from_index_entry`] checks against one worktree.
///
/// Loads config-derived settings and the root `.gitattributes` once instead of per entry.
pub(crate) struct WorktreeEntryComparer<'a> {
    work_tree: &'a Path,
    config: ConfigSet,
    mode_policy: crate::index::WorktreeModePolicy,
    precompose_unicode: bool,
    ignorecase: bool,
    conv: std::cell::OnceCell<crate::crlf::ConversionConfig>,
    attrs: std::cell::OnceCell<Vec<crate::crlf::AttrRule>>,
}

impl<'a> WorktreeEntryComparer<'a> {
    /// Capture settings for `work_tree`; `config` falls back to the repository config files.
    pub(crate) fn new(
        repository_git_dir: &Path,
        work_tree: &'a Path,
        config: Option<&ConfigSet>,
    ) -> Self {
        let config = config.cloned().unwrap_or_else(|| {
            ConfigSet::load(
                &crate::environment::Environment::empty(),
                Some(repository_git_dir),
                true,
            )
            .unwrap_or_else(|_| ConfigSet::new())
        });
        let flag =
            |key: &str, default: bool| config.get_bool(key).and_then(|r| r.ok()).unwrap_or(default);
        Self {
            work_tree,
            mode_policy: crate::index::WorktreeModePolicy::from_config(&config),
            precompose_unicode: flag("core.precomposeunicode", false),
            ignorecase: flag("core.ignorecase", false),
            config,
            conv: std::cell::OnceCell::new(),
            attrs: std::cell::OnceCell::new(),
        }
    }

    /// Same contract as [`worktree_differs_from_index_entry`].
    pub(crate) fn differs(
        &self,
        odb: &Odb,
        ie: &IndexEntry,
        ignore_submodule_untracked: bool,
        filter_process: Option<&crate::filter_process::FilterProcessState>,
    ) -> Result<bool> {
        use crate::crlf;

        let work_tree = self.work_tree;
        let path_stored = crate::repo_relpath::storage_string(&ie.path);
        let path_str_ref = crate::repo_relpath::as_utf8_for_matching(&path_stored);
        let file_path = index_entry_worktree_abs_bytes(
            work_tree,
            &ie.path,
            self.precompose_unicode,
            self.ignorecase,
        );

        if ie.mode == 0o160000 {
            let sub_head_oid = read_submodule_head(&file_path);
            let ref_matches = match sub_head_oid {
                Some(oid) => oid == ie.oid,
                None => uninitialized_gitlink_worktree_matches_index(&file_path),
            };
            let mut flags = submodule_porcelain_flags(work_tree, path_str_ref.as_ref(), ie.oid);
            if ignore_submodule_untracked {
                flags.untracked = false;
            }
            return Ok(!ref_matches || flags.modified || flags.untracked);
        }

        let meta = match fs::symlink_metadata(&file_path) {
            Ok(m) => m,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
            {
                return Ok(true);
            }
            Err(e) => return Err(Error::Io(e)),
        };

        if meta.is_dir() {
            return Ok(true);
        }

        if !self
            .mode_policy
            .worktree_mode_matches(mode_from_metadata(&meta), ie.mode)
        {
            return Ok(true);
        }

        let conv = self
            .conv
            .get_or_init(|| crlf::ConversionConfig::from_config(&self.config));
        let attrs = self
            .attrs
            .get_or_init(|| crlf::load_gitattributes(work_tree));
        let file_attrs = crlf::get_file_attrs(attrs, path_str_ref.as_ref(), false, &self.config);
        let worktree_oid = hash_worktree_file(
            odb,
            &file_path,
            &meta,
            conv,
            &file_attrs,
            path_str_ref.as_ref(),
            Some(ie),
            filter_process,
        )?;

        let mut eff_oid = worktree_oid;
        if eff_oid != ie.oid {
            if let Ok(raw) = fs::read(&file_path) {
                let raw_oid = odb.hash(ObjectKind::Blob, &raw)?;
                if raw_oid == ie.oid {
                    eff_oid = ie.oid;
                }
            }
        }

        Ok(eff_oid != ie.oid)
    }
}

/// Inputs for [`classify_worktree_entry_for_add`].
pub struct WorktreeAddRefreshParams<'a> {
    /// Object database used to verify blob OIDs when stat data is inconclusive.
    pub odb: &'a Odb,
    /// Index entry being compared.
    pub ie: &'a IndexEntry,
    /// `lstat` metadata for the worktree path.
    pub meta: &'a fs::Metadata,
    /// Absolute worktree path to the file.
    pub abs_path: &'a Path,
    /// Repository-relative path string.
    pub rel_path: &'a str,
    /// CRLF / filter conversion settings.
    pub conv: &'a crate::crlf::ConversionConfig,
    /// Per-path attribute state.
    pub file_attrs: &'a crate::crlf::FileAttrs,
    /// On-disk index mtime when the index was loaded; `None` skips racy checks.
    pub index_mtime: Option<(u32, u32)>,
    /// Mode that would be written, after [`crate::index::WorktreeModePolicy`] against `ie.mode`
    /// and any `--chmod` hint.
    pub staged_mode: u32,
    /// Repository-scoped filter-process registry (when staging through a [`crate::repo::Repository`]).
    pub filter_process: Option<&'a crate::filter_process::FilterProcessState>,
}

/// Outcome of comparing a tracked index entry to its worktree path for staging refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeAddRefresh {
    /// Index already reflects the worktree; caller may skip staging.
    UpToDate,
    /// Executable bit differs while blob content is unchanged.
    ModeOnly {
        /// Mode to store in the index.
        mode: u32,
    },
    /// Blob OID and mode are unchanged; only cached stat fields should be refreshed.
    StatOnly,
    /// Content, type, or mode/blob mismatch requires a full restage (read, convert, hash).
    NeedsRestage,
}

/// Whether stat-only fast paths may skip re-hashing during staging.
#[must_use]
pub(crate) fn index_stat_cache_trustworthy(index_mtime: Option<(u32, u32)>) -> bool {
    match index_mtime {
        None => false,
        Some((0, _)) => false,
        Some(_) => true,
    }
}

/// When true, after worktree metadata has been read and matches the index entry, blob content
/// hashing may be skipped (Git `diff-files` / `update-index --refresh` style).
///
/// Does not permit skipping worktree observation; fsmonitor-valid entries use a separate path.
/// Independent of `core.filemode`: callers compare the executable bit separately.
///
/// A zero cached size is trusted only for the empty blob; any other blob with size zero was
/// smudged by a racy index write and must be re-hashed (Git `ce_match_stat_basic`).
#[must_use]
pub(crate) fn index_entry_skips_worktree_content_hash_after_stat_match(
    ie: &IndexEntry,
    index_mtime: Option<(u32, u32)>,
    stat_policy: crate::index::StatMatchPolicy,
) -> bool {
    if ie.stat_cache_unset() || (ie.size == 0 && !ie.oid.is_empty_blob()) {
        return false;
    }
    if !index_stat_cache_trustworthy(index_mtime) {
        return false;
    }
    !entry_is_racy_with_policy(ie, index_mtime, stat_policy)
}

/// Decide whether staging must re-read and re-hash a tracked path.
///
/// Mirrors the fast paths in [`diff_index_to_worktree`] and Git's `refresh_index` /
/// `add_file_to_index`: trust matching stat when the entry is not racy and
/// `index_stat_cache_trustworthy` allows it; re-hash when stat matches but the entry is racy
/// or when stat differs; apply CRLF-aware hashing like staging.
///
/// Returns [`WorktreeAddRefresh::UpToDate`] when the index entry already matches the worktree and
/// no restaging is required; other variants describe mode/stat-only updates or a full restage.
///
/// # Errors
///
/// Returns [`Error::Io`] when the worktree file cannot be read for content verification.
pub fn classify_worktree_entry_for_add(
    params: &WorktreeAddRefreshParams<'_>,
) -> Result<WorktreeAddRefresh> {
    use crate::index::{MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};

    let ie = params.ie;
    let meta = params.meta;
    let staged_mode = params.staged_mode;

    if ie.intent_to_add() {
        return Ok(WorktreeAddRefresh::NeedsRestage);
    }

    let racy = entry_is_racy(ie, params.index_mtime);
    let stat_same = stat_matches(ie, meta);
    let trust_stat = index_stat_cache_trustworthy(params.index_mtime);

    if ie.mode == MODE_SYMLINK {
        // With `core.symlinks=false` the policy keeps a plain file holding the link target as
        // a symlink entry, so it is compared by content like a real link.
        if !meta.file_type().is_symlink() && staged_mode != MODE_SYMLINK {
            return Ok(WorktreeAddRefresh::NeedsRestage);
        }
        if stat_same && !racy && trust_stat && staged_mode == ie.mode {
            return Ok(WorktreeAddRefresh::UpToDate);
        }
        if stat_same && !racy && trust_stat && staged_mode != ie.mode {
            return Ok(WorktreeAddRefresh::ModeOnly { mode: staged_mode });
        }
        return worktree_effective_oid_matches_index(params);
    }

    if ie.mode != MODE_REGULAR && ie.mode != MODE_EXECUTABLE {
        return Ok(WorktreeAddRefresh::NeedsRestage);
    }

    if meta.file_type().is_symlink() || meta.is_dir() {
        return Ok(WorktreeAddRefresh::NeedsRestage);
    }

    if stat_same && !racy && trust_stat {
        if staged_mode == ie.mode {
            return Ok(WorktreeAddRefresh::UpToDate);
        }
        return Ok(WorktreeAddRefresh::ModeOnly { mode: staged_mode });
    }

    worktree_effective_oid_matches_index(params)
}

fn worktree_effective_oid_matches_index(
    params: &WorktreeAddRefreshParams<'_>,
) -> Result<WorktreeAddRefresh> {
    let WorktreeAddRefreshParams {
        odb,
        ie,
        meta,
        abs_path,
        rel_path,
        conv,
        file_attrs,
        staged_mode,
        ..
    } = params;
    let worktree_oid = hash_worktree_file(
        odb,
        abs_path,
        meta,
        conv,
        file_attrs,
        rel_path,
        Some(ie),
        params.filter_process,
    )?;
    let mut eff_oid = worktree_oid;
    if eff_oid != ie.oid {
        if let Ok(raw) = fs::read(abs_path) {
            let raw_oid = odb.hash(ObjectKind::Blob, &raw)?;
            if raw_oid == ie.oid {
                eff_oid = ie.oid;
            }
        }
    }
    if eff_oid != ie.oid {
        return Ok(WorktreeAddRefresh::NeedsRestage);
    }
    if *staged_mode != ie.mode {
        return Ok(WorktreeAddRefresh::ModeOnly { mode: *staged_mode });
    }
    if stat_matches(ie, meta) {
        Ok(WorktreeAddRefresh::UpToDate)
    } else {
        Ok(WorktreeAddRefresh::StatOnly)
    }
}

/// Whether worktree metadata matches cached index stat data (Git `ie_match_stat` / `match_stat_data`).
#[must_use]
pub fn stat_matches(ie: &IndexEntry, meta: &fs::Metadata) -> bool {
    stat_matches_with_policy(ie, meta, crate::index::StatMatchPolicy::default())
}

/// Whether compact worktree stat from a directory scan matches the index row.
pub(crate) fn stat_matches_blob_stat(
    ie: &IndexEntry,
    stat: &crate::worktree_scan::BlobDiskStat,
    policy: crate::index::StatMatchPolicy,
) -> bool {
    stat.fields.matches_index_entry(ie, policy)
}

/// Like [`stat_matches`], honoring `core.trustctime`, `core.checkstat`, and `core.usenanosec`.
#[must_use]
pub fn stat_matches_with_policy(
    ie: &IndexEntry,
    meta: &fs::Metadata,
    policy: crate::index::StatMatchPolicy,
) -> bool {
    if crate::index::munge_index_size(meta.len()) != ie.size {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mtime() as u32 != ie.mtime_sec {
            return false;
        }
        if policy.use_nanosec && policy.check_stat && meta.mtime_nsec() as u32 != ie.mtime_nsec {
            return false;
        }
        if policy.trust_ctime && policy.check_stat {
            if meta.ctime() as u32 != ie.ctime_sec {
                return false;
            }
            if policy.use_nanosec && meta.ctime_nsec() as u32 != ie.ctime_nsec {
                return false;
            }
        }
        if policy.check_stat && meta.ino() as u32 != ie.ino {
            return false;
        }
    }
    #[cfg(not(unix))]
    {
        let ((ctime_sec, ctime_nsec), (mtime_sec, mtime_nsec)) = crate::index::metadata_times(meta);
        if mtime_sec != ie.mtime_sec {
            return false;
        }
        if policy.use_nanosec && policy.check_stat && mtime_nsec != ie.mtime_nsec {
            return false;
        }
        if policy.trust_ctime && policy.check_stat {
            if ctime_sec != ie.ctime_sec {
                return false;
            }
            if policy.use_nanosec && ctime_nsec != ie.ctime_nsec {
                return false;
            }
        }
    }
    true
}

/// Refresh cached stat data for stage-0 file/symlink entries whose worktree content still matches
/// the recorded OID but whose on-disk stat went stale.
///
/// This mirrors Git's `refresh_index` / `refresh_cache_ent`: an entry is only marked clean (stat
/// adopted from the worktree) after its content is re-verified against the index OID. A genuinely
/// modified entry keeps its stale stat so `diff-files` / `status` continue to report it. Operations
/// that rewrite the worktree (`status`, `reset --mixed`, `stash`) call this before writing the
/// index so a subsequent `git diff-files` sees refreshed entries as clean.
///
/// Gitlinks, sparse (`skip_worktree`), `assume_unchanged` and intent-to-add entries are skipped.
/// The blob comparison is a raw-content hash, so a CRLF-smudged match is conservatively missed
/// (the entry simply stays stat-dirty and is re-hashed next time — never the reverse).
///
/// `index_mtime` is the on-disk index file's `(mtime_sec, mtime_nsec)` (see
/// `entry_is_racy` / Git `is_racy_timestamp`); pass `None` when unknown — racy detection is
/// then skipped, which is conservative for tree-built indexes whose zeroed stat never matches.
///
/// `stat_parallel_threads` overrides parallel stat workers (`Some(1)` forces serial); when `None`,
/// honors `core.preloadIndex` and [`crate::worktree_scan::PARALLEL_STAT_MIN_ENTRIES`].
///
/// Returns `Ok(true)` when at least one entry was refreshed or invalidated, so callers can write
/// the index opportunistically (Git only persists a refresh that changed something).
pub fn refresh_index_stat_content_verified(
    odb: &Odb,
    git_dir: &Path,
    index: &mut Index,
    work_tree: &Path,
    index_mtime: Option<(u32, u32)>,
    config: Option<&ConfigSet>,
    _stat_parallel_threads: Option<usize>,
) -> Result<bool> {
    let _ = _stat_parallel_threads;
    refresh_index_stat_content_verified_with_rules(
        odb,
        git_dir,
        index,
        work_tree,
        index_mtime,
        config,
        _stat_parallel_threads,
        None,
    )
}

/// Like [`refresh_index_stat_content_verified`], resolving attributes and conversion through a
/// shared [`crate::worktree_rules::WorktreeRules`] when one is supplied (its config snapshot then
/// takes precedence over `config`).
///
/// # Errors
///
/// Propagates I/O errors from the directory-grouped worktree scan.
#[allow(clippy::too_many_arguments)]
pub fn refresh_index_stat_content_verified_with_rules(
    odb: &Odb,
    git_dir: &Path,
    index: &mut Index,
    work_tree: &Path,
    index_mtime: Option<(u32, u32)>,
    config: Option<&ConfigSet>,
    stat_parallel_threads: Option<usize>,
    worktree_rules: Option<&std::sync::Arc<std::sync::Mutex<crate::worktree_rules::WorktreeRules>>>,
) -> Result<bool> {
    use crate::config::ConfigSet;
    use crate::crlf;
    use crate::worktree_scan::{
        collect_blob_lookup_batches, group_blob_entries_by_dir, visit_blob_lookup_batches,
        BlobDiskLookup, BlobScanSparseMode, WorktreeBlobScanOptions,
    };

    let index_mtime = index_mtime_for_diff(index, index_mtime);
    let rules_lock = worktree_rules
        .map(|shared| {
            shared
                .lock()
                .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))
        })
        .transpose()?;
    let config = rules_lock
        .as_ref()
        .map(|r| r.config().clone())
        .unwrap_or_else(|| {
            config.cloned().unwrap_or_else(|| {
                ConfigSet::load(
                    &crate::environment::Environment::empty(),
                    Some(git_dir),
                    true,
                )
                .unwrap_or_default()
            })
        });
    let preload_index = config
        .get_bool("core.preloadIndex")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let blob_scan = WorktreeBlobScanOptions {
        preload_index,
        stat_parallel_threads,
    };
    let conv = rules_lock
        .as_ref()
        .map(|r| r.conversion().clone())
        .unwrap_or_else(|| crlf::ConversionConfig::from_config(&config));
    let legacy_attrs = if rules_lock.is_none() {
        Some(crlf::load_gitattributes_parsed(work_tree))
    } else {
        None
    };
    let file_attrs_for = |path: &str| -> crlf::FileAttrs {
        if let Some(rules) = rules_lock.as_ref() {
            rules.file_attrs(path, false)
        } else if let Some(parsed) = legacy_attrs.as_ref() {
            crlf::file_attrs_from_gitattributes(&parsed.rules, &parsed.macros, path, false, &config)
        } else {
            crlf::FileAttrs::default()
        }
    };
    let precompose_unicode = config
        .get_bool("core.precomposeunicode")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let ignorecase = config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let stat_policy = crate::index::StatMatchPolicy::from_config(Some(&config));

    let blob_dirs = group_blob_entries_by_dir(index, false);
    let work_tree_owned = work_tree.to_path_buf();
    let dir_abs = move |dir: &[u8]| {
        if dir.is_empty() {
            work_tree_owned.clone()
        } else {
            index_entry_worktree_abs_bytes(&work_tree_owned, dir, precompose_unicode, ignorecase)
        }
    };
    let work_tree_for_files = work_tree.to_path_buf();
    let file_abs = move |rel: &[u8]| {
        index_entry_worktree_abs_bytes(&work_tree_for_files, rel, precompose_unicode, ignorecase)
    };
    let mut changed = false;
    let filter_fp = rules_lock.as_ref().map(|r| r.filter_process());
    let thread_policy = crate::parallel::ThreadPolicy::default();
    // Symlink ancestor detection must run even when core.symlinks is false.
    let refresh_batches = collect_blob_lookup_batches(
        index,
        &blob_dirs,
        blob_scan,
        &thread_policy,
        &config,
        dir_abs,
        file_abs,
        || {
            let mut dir_symlinks = SymlinkDirCache::default();
            move |rel_path: &[u8]| {
                dir_symlinks.has_symlink_in_path_bytes(
                    work_tree,
                    rel_path,
                    precompose_unicode,
                    ignorecase,
                )
            }
        },
        BlobScanSparseMode::StatRefresh {
            stat_policy,
            index_mtime,
        },
    )?;
    let par_opts = refresh_hash_parallel_options(&thread_policy, &config, stat_parallel_threads);
    let refresh_checks = refresh_content_checks(
        odb,
        index,
        &refresh_batches,
        index_mtime,
        stat_policy,
        &|_, _, _| true,
        &WorktreeHashContext {
            work_tree,
            precompose_unicode,
            ignorecase,
            conv: &conv,
            file_attrs_for: &file_attrs_for,
            filter_process: filter_fp,
        },
        par_opts,
        None,
    )?;
    visit_blob_lookup_batches(refresh_batches, |entry_index, lookup| {
        let BlobDiskLookup::Present(stat) = lookup else {
            return Ok(());
        };
        let content_matches = RefreshContent::matches(
            refresh_checks.get(entry_index),
            &index.entries[entry_index].oid,
        );
        let refresh = refresh_index_blob_stat_from_scan(
            &mut index.entries[entry_index],
            &stat,
            index_mtime,
            stat_policy,
            content_matches,
        );
        if refresh.index_changed {
            changed = true;
        }
        if refresh.content_verified {
            let path = index.entries[entry_index].path.clone();
            let oid = index.entries[entry_index].oid;
            index.mark_content_verified(&path, oid);
        }
        Ok(())
    })?;
    Ok(changed)
}

/// Result of refreshing one index entry's cached stat during a worktree scan.
struct BlobStatRefreshOutcome {
    /// Whether the in-memory index entry was updated (stat fields or invalidation).
    index_changed: bool,
    /// Blob content was read and matches the recorded OID (skip a second hash in the same pass).
    content_verified: bool,
}

/// Whether the stat refresh may adopt worktree stat data for `ie` at all.
fn refresh_index_entry_eligible(ie: &IndexEntry) -> bool {
    use crate::index::{MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};
    ie.stage() == 0
        && !ie.skip_worktree()
        && !ie.assume_unchanged()
        && !ie.intent_to_add()
        && matches!(ie.mode, MODE_REGULAR | MODE_EXECUTABLE | MODE_SYMLINK)
}

/// Whether refreshing `ie` from the scanned `stat` must first prove by content that the
/// worktree still holds `ie.oid`: the stat data changed, or it matches but is racy.
fn refresh_needs_content_check(
    ie: &IndexEntry,
    stat: &crate::worktree_scan::BlobDiskStat,
    index_mtime: Option<(u32, u32)>,
    stat_policy: crate::index::StatMatchPolicy,
) -> bool {
    refresh_index_entry_eligible(ie)
        && !stat.is_dir
        && (!stat_matches_blob_stat(ie, stat, stat_policy)
            || entry_is_racy_with_policy(ie, index_mtime, stat_policy))
}

/// Whether a worktree diff with `refresh_index_stat_in_pass` refreshes `ie` from `stat`.
///
/// Status leaves out entries whose stat data already decides the answer: clean and not racy,
/// or a changed size that [`status_size_change_proves_modified`] accepts (reported as modified
/// without reading the file). A size change under a non-identity clean conversion is refreshed:
/// when the cleaned content still matches, the entry adopts the new stat, so later scans trust it.
pub(crate) fn worktree_diff_refreshes_entry(
    ie: &IndexEntry,
    stat: &crate::worktree_scan::BlobDiskStat,
    index_mtime: Option<(u32, u32)>,
    stat_policy: crate::index::StatMatchPolicy,
    for_status: bool,
    conversion_is_identity: impl FnOnce() -> bool,
) -> bool {
    if !for_status {
        return true;
    }
    if stat_matches_blob_stat(ie, stat, stat_policy) {
        return entry_is_racy_with_policy(ie, index_mtime, stat_policy);
    }
    !status_size_change_proves_modified(
        ie,
        stat.worktree_size(),
        for_status,
        conversion_is_identity,
    )
}

/// Pool options for the stat refresh's content checks.
///
/// `thread_policy` and config pick the worker count; `stat_parallel_threads` caps it the same
/// way it caps the stat scan (`Some(0)` or `Some(1)` keeps hashing on the calling thread).
/// There is no byte threshold: opening each file, not hashing its bytes, dominates for the
/// small files a checkout leaves racy.
fn refresh_hash_parallel_options(
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &ConfigSet,
    stat_parallel_threads: Option<usize>,
) -> crate::parallel::ParallelOptions {
    let policy = crate::parallel::ThreadPolicy {
        min_items_override: thread_policy.min_items_override.or(Some(0)),
        min_total_bytes_override: thread_policy.min_total_bytes_override.or(Some(0)),
        ..*thread_policy
    };
    let mut par_opts = policy.resolve(crate::parallel::ParallelOp::IndexRefresh, config);
    if let Some(cap) = stat_parallel_threads {
        par_opts.threads = par_opts
            .threads
            .min(std::num::NonZeroUsize::new(cap).unwrap_or(std::num::NonZeroUsize::MIN));
    }
    par_opts.min_items = 0;
    par_opts.min_total_bytes = None;
    par_opts
}

/// Worktree layout and attribute context for hashing index entries during a scan.
struct WorktreeHashContext<'a> {
    work_tree: &'a Path,
    precompose_unicode: bool,
    ignorecase: bool,
    conv: &'a crate::crlf::ConversionConfig,
    file_attrs_for: &'a dyn Fn(&str) -> crate::crlf::FileAttrs,
    filter_process: Option<&'a crate::filter_process::FilterProcessState>,
}

/// Run every content check the stat refresh needs before the serial visit of `batches`.
///
/// Racily clean entries are the common case right after a checkout writes the index: every
/// file then shares the index's timestamp second. Hashing them here, in parallel when
/// `par_opts` and the total size allow it, keeps that first scan from reading files one by one.
///
/// Returns a check for each scanned entry that `include` (given the entry index) admits and
/// [`refresh_needs_content_check`] selects. Read and filter failures are
/// [`RefreshContent::Failed`], so those entries stay unverified.
#[allow(clippy::too_many_arguments)]
fn refresh_content_checks(
    odb: &Odb,
    index: &Index,
    batches: &[Vec<(usize, crate::worktree_scan::BlobDiskLookup)>],
    index_mtime: Option<(u32, u32)>,
    stat_policy: crate::index::StatMatchPolicy,
    include: &dyn Fn(usize, &IndexEntry, &crate::worktree_scan::BlobDiskStat) -> bool,
    ctx: &WorktreeHashContext<'_>,
    par_opts: crate::parallel::ParallelOptions,
    cancel_watch: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<RefreshChecks> {
    use crate::progress::{poll_watch, CANCEL_POLL_STRIDE};
    use crate::worktree_scan::BlobDiskLookup;

    /// Attributes are resolved on the calling thread: the shared rules are not `Sync`.
    struct ContentCheck {
        entry_index: usize,
        worktree_mode: u32,
        file_attrs: crate::crlf::FileAttrs,
    }

    let mut jobs = Vec::new();
    let mut total_bytes = 0usize;
    let mut cancel_stride = 0usize;
    for (entry_index, lookup) in batches.iter().flatten() {
        cancel_stride = cancel_stride.wrapping_add(1);
        if cancel_stride.is_multiple_of(CANCEL_POLL_STRIDE) {
            if let Some(w) = cancel_watch {
                poll_watch(w)?;
            }
        }
        let BlobDiskLookup::Present(stat) = lookup else {
            continue;
        };
        let ie = &index.entries[*entry_index];
        if !refresh_needs_content_check(ie, stat, index_mtime, stat_policy)
            || !include(*entry_index, ie, stat)
        {
            continue;
        }
        total_bytes = total_bytes.saturating_add(ie.size as usize);
        jobs.push(ContentCheck {
            entry_index: *entry_index,
            worktree_mode: stat.git_mode,
            file_attrs: (ctx.file_attrs_for)(&crate::repo_relpath::storage_string(&ie.path)),
        });
    }
    if jobs.is_empty() {
        return Ok(RefreshChecks::default());
    }
    let mut checks = vec![None; index.entries.len()];
    let (work_tree, precompose_unicode, ignorecase) =
        (ctx.work_tree, ctx.precompose_unicode, ctx.ignorecase);
    let (conv, filter_process) = (ctx.conv, ctx.filter_process);
    let entries = index.entries.as_slice();
    let watch_for_hash = cancel_watch.cloned();
    let hash_one = |job: &ContentCheck| -> Result<RefreshContent> {
        if job.entry_index.is_multiple_of(CANCEL_POLL_STRIDE) {
            if let Some(w) = watch_for_hash.as_ref() {
                poll_watch(w)?;
            }
        }
        let ie = &entries[job.entry_index];
        let file_path =
            index_entry_worktree_abs_bytes(work_tree, &ie.path, precompose_unicode, ignorecase);
        Ok(RefreshContent::from(hash_worktree_file_with_mode(
            odb,
            &file_path,
            job.worktree_mode,
            conv,
            &job.file_attrs,
            &crate::repo_relpath::storage_string(&ie.path),
            Some(ie),
            filter_process,
        )))
    };
    let matches = if par_opts.threads.get() > 1 && jobs.len() > 1 {
        crate::parallel::try_par_map_force(&jobs, par_opts, hash_one).map_err(|e| match e {
            crate::parallel::ParallelHashError::Task(err) => err,
        })?
    } else {
        let mut out = Vec::with_capacity(jobs.len());
        let mut serial_stride = 0usize;
        for job in &jobs {
            serial_stride = serial_stride.wrapping_add(1);
            if serial_stride.is_multiple_of(CANCEL_POLL_STRIDE) {
                if let Some(w) = cancel_watch {
                    poll_watch(w)?;
                }
            }
            out.push(hash_one(job)?);
        }
        out
    };
    for (job, content) in jobs.iter().zip(matches) {
        checks[job.entry_index] = Some(content);
    }
    Ok(RefreshChecks(checks))
}

/// Results of [`refresh_content_checks`], by index entry position.
///
/// Empty when no entry needed a check (a clean, non-racy work tree), so the common case
/// allocates nothing.
#[derive(Default)]
struct RefreshChecks(Vec<Option<RefreshContent>>);

impl RefreshChecks {
    /// The check that ran for the entry at `entry_index`, if any.
    fn get(&self, entry_index: usize) -> Option<RefreshContent> {
        self.0.get(entry_index).copied().flatten()
    }
}

/// What the stat refresh learned by hashing one entry's worktree file.
#[derive(Clone, Copy)]
enum RefreshContent {
    /// The file could not be read or clean-converted.
    Failed,
    /// The cleaned file hashes to this blob id.
    Hashed(ObjectId),
}

impl From<Result<ObjectId>> for RefreshContent {
    fn from(hashed: Result<ObjectId>) -> Self {
        hashed.map_or(Self::Failed, Self::Hashed)
    }
}

impl RefreshContent {
    /// Whether `check` proved the worktree still holds `oid`.
    fn matches(check: Option<Self>, oid: &ObjectId) -> bool {
        matches!(check, Some(Self::Hashed(hashed)) if hashed == *oid)
    }

    /// Whether `check` proved the worktree holds other content than `oid`.
    fn differs(check: Option<Self>, oid: &ObjectId) -> bool {
        matches!(check, Some(Self::Hashed(hashed)) if hashed != *oid)
    }
}

/// Adopt the scanned `stat` for `ie` where that cannot hide a worktree change.
///
/// `content_matches` is the [`refresh_content_checks`] result for this entry; it is only
/// consulted when [`refresh_needs_content_check`] holds. A racy entry whose content no longer
/// matches has its stat cache invalidated so later scans re-hash it.
fn refresh_index_blob_stat_from_scan(
    ie: &mut IndexEntry,
    stat: &crate::worktree_scan::BlobDiskStat,
    index_mtime: Option<(u32, u32)>,
    stat_policy: crate::index::StatMatchPolicy,
    content_matches: bool,
) -> BlobStatRefreshOutcome {
    const UNCHANGED: BlobStatRefreshOutcome = BlobStatRefreshOutcome {
        index_changed: false,
        content_verified: false,
    };
    if !refresh_index_entry_eligible(ie) || stat.is_dir {
        return UNCHANGED;
    }
    let cached = crate::index::IndexStatFields::of_entry(ie);
    let stat_same = cached.equivalent_to(&stat.fields, stat_policy);
    let stat_matches = stat_matches_blob_stat(ie, stat, stat_policy);
    if !stat_matches || entry_is_racy_with_policy(ie, index_mtime, stat_policy) {
        if !content_matches {
            if stat_matches {
                invalidate_index_stat_cache(ie);
                return BlobStatRefreshOutcome {
                    index_changed: true,
                    content_verified: false,
                };
            }
            return UNCHANGED;
        }
        if !stat_same {
            stat.fields.store_in(ie);
        }
        return BlobStatRefreshOutcome {
            index_changed: !stat_same,
            content_verified: true,
        };
    }
    if stat_same {
        return BlobStatRefreshOutcome {
            index_changed: false,
            content_verified: true,
        };
    }
    stat.fields.store_in(ie);
    BlobStatRefreshOutcome {
        index_changed: true,
        content_verified: false,
    }
}

/// Whether path-only checkout can skip writing a blob because the worktree is up to date.
///
/// Matching stat data is trusted unless the entry is racy relative to the on-disk index mtime;
/// then the worktree content (after clean/filter conversion) must hash to `expected_oid`
/// ([`hash_worktree_file`]). `config` is an optional preloaded configuration.
///
/// Returns `Ok(true)` when the caller should skip rewriting the path.
///
/// # Errors
///
/// Propagates I/O or hashing failures from [`hash_worktree_file`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn path_checkout_skip_blob_write_when_up_to_date_with_config(
    odb: &Odb,
    git_dir: &Path,
    work_tree: &Path,
    index: &Index,
    entry: &IndexEntry,
    expected_oid: &ObjectId,
    expected_mode: u32,
    rel_path: &str,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
    preloaded_config: Option<&crate::config::ConfigSet>,
) -> Result<bool> {
    use crate::config::ConfigSet;
    use crate::crlf;
    use crate::index::{MODE_EXECUTABLE, MODE_REGULAR};

    if entry.oid != *expected_oid || entry.mode != expected_mode {
        return Ok(false);
    }
    if expected_mode != MODE_REGULAR && expected_mode != MODE_EXECUTABLE {
        return Ok(false);
    }
    let abs_path = crate::repo_relpath::worktree_abs(work_tree, rel_path);
    let meta = match fs::symlink_metadata(&abs_path) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    if !meta.is_file() || !stat_matches(entry, &meta) {
        return Ok(false);
    }
    let owned_config;
    let config = match preloaded_config {
        Some(cfg) => cfg,
        None => {
            owned_config = ConfigSet::load(
                &crate::environment::Environment::empty(),
                Some(git_dir),
                true,
            )
            .unwrap_or_default();
            &owned_config
        }
    };
    if !crate::index::WorktreeModePolicy::from_config(config)
        .worktree_mode_matches(mode_from_metadata(&meta), expected_mode)
    {
        return Ok(false);
    }
    let index_mtime = index_mtime_for_diff(index, None);
    if !entry_is_racy(entry, index_mtime) {
        return Ok(true);
    }
    let conv = crlf::ConversionConfig::from_config(config);
    let attrs = crlf::load_gitattributes(work_tree);
    let file_attrs = crlf::get_file_attrs(&attrs, rel_path, false, config);
    let wt_oid = hash_worktree_file(
        odb,
        &abs_path,
        &meta,
        &conv,
        &file_attrs,
        rel_path,
        Some(entry),
        filter_process,
    )?;
    Ok(wt_oid == *expected_oid)
}

/// Clear cached stat fields so the next diff/status pass re-reads the work tree.
fn invalidate_index_stat_cache(ie: &mut IndexEntry) {
    ie.ctime_sec = 0;
    ie.ctime_nsec = 0;
    ie.mtime_sec = 0;
    ie.mtime_nsec = 0;
    ie.dev = 0;
    ie.ino = 0;
    ie.size = 0;
}

/// Read worktree content for diffing, honoring Git tree modes.
///
/// For [`MODE_SYMLINK`] entries backed by a symlink on disk this returns the link target path
/// bytes (via [`read_link`](std::fs::read_link)), not the contents of the followed file. A
/// symlink entry checked out as a plain file (`core.symlinks=false`) already holds the target,
/// so it is read like other modes, with a plain [`read`](std::fs::read) of the path.
///
/// # Errors
///
/// Returns [`std::io::Error`] when the path cannot be read.
pub fn read_worktree_bytes_for_mode(path: &Path, mode: u32) -> std::io::Result<Vec<u8>> {
    if mode == MODE_SYMLINK && fs::symlink_metadata(path)?.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(symlink_target_bytes(&target));
    }
    fs::read(path)
}

/// Read worktree file bytes in Git (repository) form for line diffs against index or tree blobs.
///
/// Applies the same `.gitattributes`, filter, and line-ending rules as
/// [`diff_index_to_worktree`]. Pass the stage-0 [`IndexEntry`] when available so
/// `autocrlf` index-guard normalization matches status and raw diff collection.
///
/// # Errors
///
/// Returns I/O errors from the worktree path or [`Error`] from filter conversion.
pub fn read_worktree_side_bytes_for_diff(
    odb: &Odb,
    work_tree: &Path,
    rel_path: &str,
    mode: u32,
    config: &crate::config::ConfigSet,
    index_entry: Option<&IndexEntry>,
    worktree_rules: Option<&crate::worktree_rules::WorktreeRules>,
) -> Result<Vec<u8>> {
    Ok(load_worktree_side_for_diff(
        odb,
        work_tree,
        rel_path,
        mode,
        config,
        index_entry,
        worktree_rules,
        false,
    )?
    .0
    .data)
}

/// Load worktree bytes for diffing, optionally skipping reads above `core.bigFileThreshold`.
///
/// Returns empty [`super::structured::SideBytes`] with `binary = true` when the file is not read.
/// When `force_read_content` is true, large files are read and binary payload bytes are retained
/// (for `GIT binary patch` / `git apply`).
///
/// # Errors
///
/// Returns I/O errors from the worktree path or [`Error`] from filter conversion.
#[allow(clippy::too_many_arguments)]
pub fn load_worktree_side_for_diff(
    odb: &Odb,
    work_tree: &Path,
    rel_path: &str,
    mode: u32,
    config: &crate::config::ConfigSet,
    index_entry: Option<&IndexEntry>,
    worktree_rules: Option<&crate::worktree_rules::WorktreeRules>,
    force_read_content: bool,
) -> Result<(super::structured::SideBytes, bool)> {
    use super::tree_content::{side_bytes_after_blob_read, treat_side_as_binary_without_reading};
    let file_path = crate::repo_relpath::worktree_abs(work_tree, rel_path);
    let rel_match = crate::repo_relpath::as_utf8_for_matching(rel_path);
    let meta = fs::symlink_metadata(&file_path).map_err(|source| {
        crate::worktree::map_worktree_io(
            rel_match.as_ref(),
            crate::worktree::WorktreePathOperation::Metadata,
            source,
        )
    })?;
    if mode == MODE_SYMLINK && meta.file_type().is_symlink() {
        let target = fs::read_link(&file_path).map_err(|source| {
            crate::worktree::map_worktree_io(
                rel_match.as_ref(),
                crate::worktree::WorktreePathOperation::Read,
                source,
            )
        })?;
        return Ok((
            super::structured::SideBytes::from_bytes(symlink_target_bytes(&target)),
            false,
        ));
    }
    if !force_read_content
        && treat_side_as_binary_without_reading(odb, meta.len(), Some(rel_match.as_ref()), false)?
    {
        return Ok((super::structured::SideBytes::from_bytes(Vec::new()), true));
    }
    let data = if let Some(rules) = worktree_rules {
        worktree_blob_bytes(
            odb,
            &file_path,
            &meta,
            rules.conversion(),
            &rules.file_attrs(rel_match.as_ref(), meta.is_dir()),
            rel_match.as_ref(),
            index_entry,
            Some(rules.filter_process()),
        )?
    } else {
        let conv = crate::crlf::ConversionConfig::from_config(config);
        let attrs = crate::crlf::load_gitattributes(work_tree);
        let file_attrs =
            crate::crlf::get_file_attrs(&attrs, rel_match.as_ref(), meta.is_dir(), config);
        worktree_blob_bytes(
            odb,
            &file_path,
            &meta,
            &conv,
            &file_attrs,
            rel_match.as_ref(),
            index_entry,
            None,
        )?
    };
    Ok(side_bytes_after_blob_read(data, force_read_content))
}

/// Symlink target as the byte string Git hashes for the blob OID.
///
/// On Unix the raw `OsStr` bytes are used verbatim. On Windows `OsStr` is WTF-8
/// and has no stable byte view, so the lossy UTF-8 form is used (symlink targets
/// in Git trees are UTF-8 in practice).
pub(crate) fn symlink_target_bytes(target: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        target.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        target.as_os_str().to_string_lossy().as_bytes().to_vec()
    }
}

/// A worktree file's blob content: normalized bytes, or an open file that is hashed or
/// stored as a stream.
///
/// Regular files of at least the caller's size threshold whose bytes can be stored without clean
/// conversion ([`crate::crlf::worktree_allows_raw_byte_stream`], including binary files under
/// `core.autocrlf`) are streamed so memory use does not depend on the file size.
pub(crate) enum WorktreeBlobContent {
    /// Content after the clean conversion (filters, encoding, line endings).
    Bytes(Vec<u8>),
    /// Unconverted big file, positioned at its start.
    Stream {
        /// Open handle on the worktree file.
        file: fs::File,
        /// File size when it was opened.
        size: u64,
    },
}

impl WorktreeBlobContent {
    /// Object id of the content, without storing it.
    pub(crate) fn hash(self, odb: &Odb) -> Result<ObjectId> {
        match self {
            Self::Bytes(data) => odb.hash(ObjectKind::Blob, &data),
            Self::Stream { mut file, size } => odb.hash_stream(ObjectKind::Blob, size, &mut file),
        }
    }

    /// Store the content as a blob and return its id.
    pub(crate) fn store(self, odb: &Odb, options: crate::odb::WriteOptions) -> Result<ObjectId> {
        match self {
            Self::Bytes(data) => odb.write_with_options(ObjectKind::Blob, &data, options),
            Self::Stream { mut file, size } => {
                odb.write_stream(ObjectKind::Blob, size, &mut file, options)
            }
        }
    }

    /// [`Self::store`] with `options` when given, otherwise [`Self::hash`].
    fn object_id(self, odb: &Odb, store: Option<crate::odb::WriteOptions>) -> Result<ObjectId> {
        match store {
            Some(options) => self.store(odb, options),
            None => self.hash(odb),
        }
    }
}

/// Read and clean-convert an open regular worktree file of `size` bytes.
#[allow(clippy::too_many_arguments)]
fn read_regular_worktree_bytes(
    odb: &Odb,
    mut file: fs::File,
    size: u64,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<Vec<u8>> {
    let needs_index_blob =
        index_entry.is_some_and(|_| crate::crlf::clean_uses_autocrlf_index_guard(file_attrs, conv));
    let prior_blob: Option<Vec<u8>> = if needs_index_blob {
        index_entry
            .filter(|e| e.oid != zero_oid())
            .and_then(|e| odb.read(&e.oid).ok().map(|o| o.data))
    } else {
        None
    };
    let mut raw = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
    std::io::Read::read_to_end(&mut file, &mut raw).map_err(|source| {
        crate::worktree::map_worktree_io(
            rel_path,
            crate::worktree::WorktreePathOperation::Read,
            source,
        )
    })?;
    #[cfg(test)]
    odb.hot_path_metrics().record_blob_content_read();
    let opts = crate::crlf::ConvertToGitOpts {
        index_blob: prior_blob.as_deref(),
        renormalize: false,
        check_safecrlf: false,
        filter_process,
        command_runner: None,
    };
    Ok(crate::crlf::convert_to_git_owned(
        raw, rel_path, conv, file_attrs, opts,
    )?)
}

fn open_with_size(path: &Path, rel_path: &str) -> Result<(fs::File, u64)> {
    let file = fs::File::open(path).map_err(|source| {
        crate::worktree::map_worktree_io(
            rel_path,
            crate::worktree::WorktreePathOperation::Read,
            source,
        )
    })?;
    let size = file
        .metadata()
        .map_err(|source| {
            crate::worktree::map_worktree_io(
                rel_path,
                crate::worktree::WorktreePathOperation::Metadata,
                source,
            )
        })?
        .len();
    Ok((file, size))
}

/// Open a regular worktree file and clean-convert it, or hand it out as a stream when it is
/// at least `stream_from` bytes and its clean conversion is the identity.
#[allow(clippy::too_many_arguments)]
fn read_regular_worktree_blob(
    odb: &Odb,
    path: &Path,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
    stream_from: u64,
) -> Result<WorktreeBlobContent> {
    let (mut file, size) = open_with_size(path, rel_path)?;
    if size >= stream_from
        && crate::crlf::worktree_allows_raw_byte_stream(
            conv,
            file_attrs,
            index_entry,
            &mut file,
            size,
        )?
    {
        #[cfg(test)]
        odb.hot_path_metrics().record_blob_content_read();
        return Ok(WorktreeBlobContent::Stream { file, size });
    }
    read_regular_worktree_bytes(
        odb,
        file,
        size,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        filter_process,
    )
    .map(WorktreeBlobContent::Bytes)
}

/// Blob content for a worktree path: symlink target, empty for a directory, or the
/// clean-converted (or streamed, see [`WorktreeBlobContent`]) file content.
#[allow(clippy::too_many_arguments)]
pub(crate) fn worktree_blob_content(
    odb: &Odb,
    path: &Path,
    meta: &fs::Metadata,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<WorktreeBlobContent> {
    worktree_blob_content_from(
        odb,
        path,
        meta,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        filter_process,
        hash_stream_threshold(odb)?,
    )
}

/// [`worktree_blob_content`], streaming unconverted regular files from `stream_from` bytes.
#[allow(clippy::too_many_arguments)]
fn worktree_blob_content_from(
    odb: &Odb,
    path: &Path,
    meta: &fs::Metadata,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
    stream_from: u64,
) -> Result<WorktreeBlobContent> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|source| {
            crate::worktree::map_worktree_io(
                rel_path,
                crate::worktree::WorktreePathOperation::Read,
                source,
            )
        })?;
        return Ok(WorktreeBlobContent::Bytes(symlink_target_bytes(&target)));
    }
    if meta.is_dir() {
        return Ok(WorktreeBlobContent::Bytes(Vec::new()));
    }
    read_regular_worktree_blob(
        odb,
        path,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        filter_process,
        stream_from,
    )
}

/// Like [`worktree_blob_content`], always returning the bytes (for callers that need the
/// content itself, such as line diffs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn worktree_blob_bytes(
    odb: &Odb,
    path: &Path,
    meta: &fs::Metadata,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<Vec<u8>> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|source| {
            crate::worktree::map_worktree_io(
                rel_path,
                crate::worktree::WorktreePathOperation::Read,
                source,
            )
        })?;
        return Ok(symlink_target_bytes(&target));
    }
    if meta.is_dir() {
        return Ok(Vec::new());
    }
    let (file, size) = open_with_size(path, rel_path)?;
    read_regular_worktree_bytes(
        odb,
        file,
        size,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        filter_process,
    )
}

#[allow(clippy::too_many_arguments)]
fn worktree_file_oid(
    odb: &Odb,
    path: &Path,
    meta: &fs::Metadata,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    materialize: bool,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<ObjectId> {
    let stream_from = if materialize {
        odb.big_file_threshold()?
    } else {
        hash_stream_threshold(odb)?
    };
    worktree_blob_content_from(
        odb,
        path,
        meta,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        filter_process,
        stream_from,
    )?
    .object_id(odb, materialize.then(|| odb.bulk_stage_write_options()))
}

/// Hash normalized worktree bytes for a path (does not write to the object database).
///
/// Files that need no clean conversion are hashed as a stream, without reading them into
/// memory, from 1 MiB (or `core.bigFileThreshold` when that is smaller).
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be read, and [`Error::Filter`] when a
/// `filter.<driver>.required` clean filter fails or cannot be started; content is never
/// stored or hashed unfiltered in that case.
#[allow(clippy::too_many_arguments)]
pub fn hash_worktree_file(
    odb: &Odb,
    path: &Path,
    meta: &fs::Metadata,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<ObjectId> {
    worktree_file_oid(
        odb,
        path,
        meta,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        false,
        filter_process,
    )
}

/// Work-tree files at least this large are hashed as a stream (when their content goes into
/// Git unchanged) even below `core.bigFileThreshold`, which only governs how blobs are stored.
const STREAMED_HASH_MIN_SIZE: u64 = 1024 * 1024;

/// Size from which an unconverted work-tree file is hashed as a stream rather than read whole.
fn hash_stream_threshold(odb: &Odb) -> Result<u64> {
    Ok(odb.big_file_threshold()?.min(STREAMED_HASH_MIN_SIZE))
}

/// Like [`hash_worktree_file`], for callers that already know the worktree file type from a
/// directory scan (`worktree_mode` is the Git mode derived from that stat), so no metadata
/// probe is needed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hash_worktree_file_with_mode(
    odb: &Odb,
    path: &Path,
    worktree_mode: u32,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<ObjectId> {
    if worktree_mode == MODE_SYMLINK {
        let data = read_worktree_bytes_for_mode(path, worktree_mode)?;
        return odb.hash(ObjectKind::Blob, &data);
    }
    read_regular_worktree_blob(
        odb,
        path,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        filter_process,
        hash_stream_threshold(odb)?,
    )?
    .hash(odb)
}

/// Read, normalize (CRLF/attributes), and store a worktree blob once.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be read, and [`Error::Filter`] when a
/// `filter.<driver>.required` clean filter fails or cannot be started; content is never
/// stored or hashed unfiltered in that case.
#[allow(clippy::too_many_arguments)]
pub fn materialize_worktree_blob(
    odb: &Odb,
    path: &Path,
    meta: &fs::Metadata,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> Result<ObjectId> {
    worktree_file_oid(
        odb,
        path,
        meta,
        conv,
        file_attrs,
        rel_path,
        index_entry,
        true,
        filter_process,
    )
}

/// Derive a Git file mode from filesystem metadata.
pub fn mode_from_metadata(meta: &fs::Metadata) -> u32 {
    if meta.file_type().is_symlink() {
        0o120000
    } else {
        #[cfg(unix)]
        {
            if meta.mode() & 0o111 != 0 {
                return 0o100755;
            }
        }
        0o100644
    }
}

/// Compare a tree against the working tree.
///
/// Shows changes from `tree_oid` to the current working directory state.
/// Files tracked in the index but not in the tree are shown as Added.
/// Files in the tree but missing from the working tree are shown as Deleted.
///
/// # Parameters
///
/// - `odb` — object database.
/// - `tree_oid` — the tree to compare against (`None` for empty tree).
/// - `work_tree` — path to the working tree root.
/// - `index` — current index (used to discover new tracked files not in tree).
///
/// # Errors
///
/// Returns errors from ODB reads or I/O.
pub fn diff_tree_to_worktree(
    odb: &Odb,
    tree_oid: Option<&ObjectId>,
    work_tree: &Path,
    index: &Index,
) -> Result<Vec<DiffEntry>> {
    diff_tree_to_worktree_with_git_dir(
        odb,
        tree_oid,
        work_tree,
        &work_tree.join(".git"),
        index,
        None,
    )
}

/// Like [`diff_tree_to_worktree`], but loads config from `repository_git_dir` (linked worktrees / gitfiles).
///
/// Pass `config` when the caller already holds a repository config snapshot.
pub fn diff_tree_to_worktree_with_git_dir(
    odb: &Odb,
    tree_oid: Option<&ObjectId>,
    work_tree: &Path,
    repository_git_dir: &Path,
    index: &Index,
    config: Option<&ConfigSet>,
) -> Result<Vec<DiffEntry>> {
    diff_tree_to_worktree_with_git_dir_and_rules(
        odb,
        tree_oid,
        work_tree,
        repository_git_dir,
        index,
        config,
        None,
    )
}

/// Like [`diff_tree_to_worktree_with_git_dir`], with optional shared [`crate::worktree_rules::WorktreeRules`].
pub fn diff_tree_to_worktree_with_git_dir_and_rules(
    odb: &Odb,
    tree_oid: Option<&ObjectId>,
    work_tree: &Path,
    repository_git_dir: &Path,
    index: &Index,
    config: Option<&ConfigSet>,
    worktree_rules: Option<&crate::worktree_rules::WorktreeRules>,
) -> Result<Vec<DiffEntry>> {
    use crate::config::ConfigSet;
    use crate::crlf;

    if worktree_rules.is_none() {
        if let Some(tree_oid) = tree_oid {
            if index.cache_tree_root.as_ref() == Some(tree_oid)
                && crate::write_tree::cache_tree_fully_valid(odb, index.cache_tree.as_ref())
            {
                let mut idx = index.clone();
                let options = DiffIndexToWorktreeOptions {
                    repository_git_dir: Some(repository_git_dir.to_path_buf()),
                    refresh_index_stat_in_pass: false,
                    // Tree↔worktree raw lines use a null worktree OID when content differs; the
                    // status fast path (`for_status`) uses the same null OID for size-only drift
                    // without hashing (see `status_size_change_proves_modified`).
                    for_status: false,
                    config: config.cloned().map(std::sync::Arc::new),
                    ..DiffIndexToWorktreeOptions::default()
                };
                let (entries, _, _) = diff_index_to_worktree_inner(
                    odb,
                    &mut idx,
                    work_tree,
                    repository_git_dir,
                    options,
                    false,
                    None,
                )?;
                return Ok(entries);
            }
        }
    }

    let config = worktree_rules
        .map(|r| r.config().clone())
        .unwrap_or_else(|| {
            config.cloned().unwrap_or_else(|| {
                ConfigSet::load(
                    &crate::environment::Environment::empty(),
                    Some(repository_git_dir),
                    true,
                )
                .unwrap_or_else(|_| ConfigSet::new())
            })
        });
    let conv = worktree_rules
        .map(|r| r.conversion().clone())
        .unwrap_or_else(|| crlf::ConversionConfig::from_config(&config));
    let legacy_attrs = if worktree_rules.is_none() {
        Some(crlf::load_gitattributes_parsed(work_tree))
    } else {
        None
    };
    let file_attrs_for = |path: &str| -> crlf::FileAttrs {
        if let Some(rules) = worktree_rules {
            rules.file_attrs(path, false)
        } else if let Some(parsed) = legacy_attrs.as_ref() {
            crlf::file_attrs_from_gitattributes(&parsed.rules, &parsed.macros, path, false, &config)
        } else {
            crlf::FileAttrs::default()
        }
    };
    let filter_fp = worktree_rules.map(|r| r.filter_process());

    let sparse_index = index.has_sparse_directory_placeholders();
    // Flatten the tree into a BTreeMap keyed by path
    let tree_flat = match tree_oid {
        Some(oid) if sparse_index => flatten_tree_with_index(odb, oid, "", Some(index))?,
        Some(oid) => flatten_tree(odb, oid, "")?,
        None => Vec::new(),
    };
    let tree_map: std::collections::BTreeMap<String, &FlatEntry> =
        tree_flat.iter().map(|e| (e.path.clone(), e)).collect();

    // Build index lookup: path → &IndexEntry (stage 0 only)
    let mut index_entries: std::collections::BTreeMap<&[u8], &IndexEntry> =
        std::collections::BTreeMap::new();
    let mut index_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stage0_paths: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    for ie in &index.entries {
        if ie.stage() != 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&ie.path).to_string();
        index_entries.insert(&ie.path, ie);
        index_paths.insert(path);
        stage0_paths.insert(ie.path.clone());
    }

    // Paths with only unmerged stages (1–3) and no stage 0 — `git diff <rev>` must still list them
    // so combined `diff --cc` conflict hunks can be emitted (`t4108-apply-threeway`).
    let mut unmerged_only_paths: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for ie in &index.entries {
        if !(1..=3).contains(&ie.stage()) {
            continue;
        }
        if stage0_paths.contains(&ie.path) {
            continue;
        }
        unmerged_only_paths.insert(String::from_utf8_lossy(&ie.path).into_owned());
    }

    // Union of tree paths + index paths
    let mut all_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    all_paths.extend(tree_map.keys().cloned());
    all_paths.extend(index_paths.iter().cloned());
    all_paths.extend(unmerged_only_paths.iter().cloned());

    let index_mtime = index_mtime_for_diff(index, None);
    let stat_policy = crate::index::StatMatchPolicy::from_config(Some(&config));
    let mode_policy = crate::index::WorktreeModePolicy::from_config(&config);
    let mut result = Vec::new();

    for path in &all_paths {
        if index_entries
            .get(path.as_bytes())
            .is_some_and(|ie| ie.skip_worktree())
        {
            // Sparse checkout: `git diff <rev>` does not report tree↔worktree drift for
            // skip-worktree paths (they are outside the sparse cone). Matches t7012 stash flow.
            continue;
        }
        if index_entries
            .get(path.as_bytes())
            .is_some_and(|ie| ie.assume_unchanged())
        {
            continue;
        }

        let tree_entry = tree_map.get(path.as_str());

        // Gitlink entries (submodules) — compare HEAD commit, not file content.
        let is_gitlink = tree_entry.is_some_and(|te| te.mode == 0o160000)
            || index_entries
                .get(path.as_bytes())
                .is_some_and(|ie| ie.mode == 0o160000);
        if is_gitlink {
            let sub_dir = work_tree.join(path);
            let index_gitlink_oid = index_entries
                .get(path.as_bytes())
                .filter(|ie| ie.mode == 0o160000)
                .map(|ie| ie.oid);
            match (tree_entry, index_gitlink_oid) {
                (Some(te), _) => {
                    let sub_head = read_submodule_head_oid(&sub_dir);
                    // A gitlink whose worktree directory no longer exists is a deleted submodule:
                    // Git's `diff-lib.c` reports it as a deletion (status `D`, new mode 000000) and
                    // `--submodule` renders the `(submodule deleted)` summary (t4041 #46).
                    if sub_head.is_none() && !sub_dir.exists() {
                        result.push(DiffEntry {
                            status: DiffStatus::Deleted,
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: FileMode::new(te.mode),
                            new_mode: FileMode::ABSENT,
                            old_oid: te.oid,
                            new_oid: zero_oid(),
                            score: None,
                        });
                        continue;
                    }
                    let ref_matches = if let Some(head) = sub_head {
                        head == te.oid
                    } else {
                        uninitialized_gitlink_worktree_matches_index(&sub_dir)
                    };
                    let index_matches_tree = index_gitlink_oid.is_some_and(|oid| oid == te.oid);
                    // Default tree↔worktree diff matches Git: untracked-only paths inside a
                    // checked-out submodule do not make the gitlink differ (see `git diff <tree>`).
                    let sub_flags = submodule_porcelain_flags(work_tree, path, te.oid);
                    let dirty_while_aligned =
                        index_matches_tree && ref_matches && sub_flags.modified;
                    if !ref_matches || dirty_while_aligned {
                        // Raw `git diff <tree>` lines use a null OID on the worktree side when the
                        // checked-out submodule HEAD differs from the tree's gitlink; patch output
                        // still resolves the real commit from the submodule directory.
                        let new_oid = if !ref_matches {
                            sub_head.unwrap_or_else(zero_oid)
                        } else {
                            zero_oid()
                        };
                        result.push(DiffEntry {
                            status: DiffStatus::Modified,
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: FileMode::new(te.mode),
                            new_mode: FileMode::new(te.mode),
                            old_oid: te.oid,
                            new_oid,
                            score: None,
                        });
                    }
                }
                (None, Some(idx_oid)) => {
                    // Gitlink staged in the index but absent from the tree: a new submodule. Git
                    // reports it as an addition (status `A`) with the index gitlink on the new side,
                    // so `--submodule` renders `(new submodule)` (t4041 #46). The patch renderer
                    // resolves the real commit from the index OID / submodule HEAD.
                    let new_oid = read_submodule_head_oid(&sub_dir).unwrap_or(idx_oid);
                    result.push(DiffEntry {
                        status: DiffStatus::Added,
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: FileMode::ABSENT,
                        new_mode: FileMode::new(0o160000),
                        old_oid: zero_oid(),
                        new_oid,
                        score: None,
                    });
                }
                (None, None) => {}
            }
            continue;
        }

        let file_path = work_tree.join(path);

        let wt_meta = match fs::symlink_metadata(&file_path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::Io(e)),
        };

        if unmerged_only_paths.contains(path) {
            if let (Some(te), Some(meta)) = (tree_entry, wt_meta.as_ref()) {
                let tree_mode = crate::index::canon_mode(te.mode);
                let file_attrs = file_attrs_for(path);
                let wt_oid = hash_worktree_file(
                    odb,
                    &file_path,
                    meta,
                    &conv,
                    &file_attrs,
                    path,
                    None,
                    filter_fp,
                )?;
                let wt_mode =
                    mode_policy.index_mode_for_worktree(mode_from_metadata(meta), Some(tree_mode));
                if wt_oid != te.oid || wt_mode != tree_mode {
                    result.push(DiffEntry {
                        status: DiffStatus::for_content_change(tree_mode, wt_mode),
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: FileMode::new(tree_mode),
                        new_mode: FileMode::new(wt_mode),
                        old_oid: te.oid,
                        new_oid: wt_oid,
                        score: None,
                    });
                }
            }
            continue;
        }

        match (tree_entry, wt_meta) {
            (Some(te), Some(ref meta)) => {
                let tree_mode = crate::index::canon_mode(te.mode);
                let Some(ie) = index_entries.get(path.as_bytes()) else {
                    continue;
                };
                let wt_mode =
                    mode_policy.index_mode_for_worktree(mode_from_metadata(meta), Some(ie.mode));

                let index_matches_tree = ie.oid == te.oid && ie.mode == tree_mode;

                // Fully clean: index matches `HEAD`, worktree matches index, stat cache fresh.
                let stat_trusted = stat_matches_with_policy(ie, meta, stat_policy)
                    && !entry_is_racy_with_policy(ie, index_mtime, stat_policy);
                if index_matches_tree && stat_trusted && wt_mode == tree_mode {
                    continue;
                }

                let file_attrs = file_attrs_for(path);
                let idx_ent = index_entries.get(path.as_bytes()).copied();

                // Staged mode (same blob as `HEAD`, different mode recorded in the index) with
                // the worktree still matching the index. `core.filemode=false` does not discard
                // an explicit index chmod vs HEAD. Once the worktree drifts from the index, the
                // diff must describe the worktree side, so fall through.
                if ie.oid == te.oid && ie.mode != tree_mode && stat_trusted && wt_mode == ie.mode {
                    result.push(DiffEntry {
                        status: DiffStatus::for_content_change(tree_mode, ie.mode),
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: FileMode::new(tree_mode),
                        new_mode: FileMode::new(ie.mode),
                        old_oid: te.oid,
                        new_oid: te.oid,
                        score: None,
                    });
                    continue;
                }

                // Index still matches `HEAD`: only unstaged worktree drift (content and/or
                // worktree-only exec bit when `update-index` was not run — t4049 harness).
                if index_matches_tree {
                    let wt_oid = hash_worktree_file(
                        odb,
                        &file_path,
                        meta,
                        &conv,
                        &file_attrs,
                        path,
                        None,
                        filter_fp,
                    )?;
                    if wt_oid != te.oid {
                        result.push(DiffEntry {
                            status: DiffStatus::for_content_change(tree_mode, wt_mode),
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: FileMode::new(tree_mode),
                            new_mode: FileMode::new(wt_mode),
                            old_oid: te.oid,
                            // Raw `git diff <tree>` uses a null OID on the worktree side (patch
                            // output resolves content from the working tree).
                            new_oid: zero_oid(),
                            score: None,
                        });
                    } else if wt_mode != tree_mode {
                        result.push(DiffEntry {
                            status: DiffStatus::for_content_change(tree_mode, wt_mode),
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: FileMode::new(tree_mode),
                            new_mode: FileMode::new(wt_mode),
                            old_oid: te.oid,
                            new_oid: te.oid,
                            score: None,
                        });
                    }
                    continue;
                }

                // Staged content (and possibly mode): `git diff <rev>` is tree vs working tree.
                let wt_oid = hash_worktree_file(
                    odb,
                    &file_path,
                    meta,
                    &conv,
                    &file_attrs,
                    path,
                    idx_ent,
                    filter_fp,
                )?;
                if wt_oid != te.oid || wt_mode != tree_mode {
                    let new_oid = if wt_oid != te.oid { zero_oid() } else { te.oid };
                    result.push(DiffEntry {
                        status: DiffStatus::for_content_change(tree_mode, wt_mode),
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: FileMode::new(tree_mode),
                        new_mode: FileMode::new(wt_mode),
                        old_oid: te.oid,
                        new_oid,
                        score: None,
                    });
                }
            }
            (Some(te), None) => {
                let tree_mode = crate::index::canon_mode(te.mode);
                // In tree but missing from worktree
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path.clone()),
                    new_path: None,
                    old_mode: FileMode::new(tree_mode),
                    new_mode: FileMode::ABSENT,
                    old_oid: te.oid,
                    new_oid: zero_oid(),
                    score: None,
                });
            }
            (None, Some(ref meta)) => {
                // In index but not in tree, and exists in worktree
                let file_attrs = file_attrs_for(path);
                let idx_ent = index_entries.get(path.as_bytes()).copied();
                let wt_oid = materialize_worktree_blob(
                    odb,
                    &file_path,
                    meta,
                    &conv,
                    &file_attrs,
                    path,
                    idx_ent,
                    filter_fp,
                )?;
                let wt_mode = mode_from_metadata(meta);
                result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path.clone()),
                    old_mode: FileMode::ABSENT,
                    new_mode: FileMode::new(wt_mode),
                    old_oid: zero_oid(),
                    new_oid: wt_oid,
                    score: None,
                });
            }
            (None, None) => {
                // Tracked in index but neither in tree nor worktree — skip
            }
        }
    }

    sort_entries_by_path(&mut result);
    Ok(result)
}

// ── Rename detection ────────────────────────────────────────────────

fn read_added_entry_bytes(
    odb: &Odb,
    entry: &DiffEntry,
    work_root: Option<&Path>,
) -> Option<Vec<u8>> {
    if entry.new_oid != zero_oid() {
        if let Ok(obj) = odb.read(&entry.new_oid) {
            return Some(obj.data);
        }
    }
    let path = entry.new_path.as_deref()?;
    let root = work_root?;
    fs::read(root.join(path)).ok()
}

/// Detect renames by pairing Deleted and Added entries with similar content.
///
/// `threshold` is the minimum similarity percentage (0–100) for a pair to
/// be considered a rename (Git's default is 50%).  The function reads blob
/// content from the ODB to compute a line-level similarity score.
///
/// Exact-OID matches are always 100% similar regardless of content.
///
/// When `work_root` is set, added entries whose `new_oid` is the zero placeholder (as in
/// uncached `diff-index` when the work tree diverged from the index) load content from disk
/// under that root instead of the object database.
#[allow(clippy::expect_used)]
pub fn detect_renames(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    threshold: u32,
) -> Vec<DiffEntry> {
    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    detect_renames_with_policy(odb, work_root, entries, threshold, &config, &policy, None)
        .expect("rename detection")
}

#[inline]
fn poll_rename_cancel(
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
) -> crate::error::Result<()> {
    if let Some(watch) = cancel_watch {
        crate::progress::poll_watch(watch)?;
    }
    Ok(())
}

/// Like [`detect_renames`], but honors `index.threads` / `--threads` via `thread_policy`.
///
/// A list without both an added and a deleted entry has nothing to pair. When its paths
/// are also unique and in [`sort_entries_by_path`] order, as tree diffs return them, the
/// list comes back unchanged without reading blobs or allocating.
///
/// # Errors
///
/// Returns object database errors from reading candidate blobs, and the cancellation
/// error when `cancel_watch` is set.
pub fn detect_renames_with_policy(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    threshold: u32,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Vec<DiffEntry>> {
    poll_rename_cancel(cancel_watch)?;
    let has_deleted = entries.iter().any(|e| e.status == DiffStatus::Deleted);
    let has_added = entries.iter().any(|e| e.status == DiffStatus::Added);
    if (!has_deleted || !has_added) && entries_strictly_sorted_by_path(&entries) {
        // With unique sorted paths, regrouping and re-sorting would rebuild the same list.
        return Ok(entries);
    }
    // Split entries into deleted, added, and others.
    let mut deleted: Vec<DiffEntry> = Vec::new();
    let mut added: Vec<DiffEntry> = Vec::new();
    let mut others: Vec<DiffEntry> = Vec::new();

    for entry in entries {
        match entry.status {
            DiffStatus::Deleted => deleted.push(entry),
            DiffStatus::Added => added.push(entry),
            _ => others.push(entry),
        }
    }

    if deleted.is_empty() || added.is_empty() {
        // Nothing to pair — return original order.
        let mut result = others;
        result.extend(deleted);
        result.extend(added);
        sort_entries_by_path(&mut result);
        return Ok(result);
    }

    fn is_regularish_mode(mode: FileMode) -> bool {
        mode == FileMode::REGULAR || mode == FileMode::EXECUTABLE
    }

    fn same_path_same_blob(del: &DiffEntry, add: &DiffEntry) -> bool {
        del.old_path == add.new_path && del.old_oid == add.new_oid && del.old_mode == add.new_mode
    }

    fn sort_rename_scores(
        scores: &mut [(u32, usize, usize)],
        deleted: &[DiffEntry],
        added: &[DiffEntry],
    ) {
        scores.sort_by(|a, b| {
            let a_noop = same_path_same_blob(&deleted[a.1], &added[a.2]);
            let b_noop = same_path_same_blob(&deleted[b.1], &added[b.2]);
            let a_same = same_basename(&deleted[a.1], &added[a.2]);
            let b_same = same_basename(&deleted[b.1], &added[b.2]);
            a_noop
                .cmp(&b_noop)
                .then_with(|| b_same.cmp(&a_same))
                .then_with(|| b.0.cmp(&a.0))
        });
    }

    fn apply_rename_scores(
        scores: &[(u32, usize, usize)],
        deleted: &[DiffEntry],
        added: &[DiffEntry],
        used_deleted: &mut [bool],
        used_added: &mut [bool],
        renames: &mut Vec<DiffEntry>,
    ) {
        for (score, di, ai) in scores {
            if used_deleted[*di] || used_added[*ai] {
                continue;
            }
            used_deleted[*di] = true;
            used_added[*ai] = true;

            let del = &deleted[*di];
            let add = &added[*ai];
            if same_path_same_blob(del, add) {
                continue;
            }

            renames.push(DiffEntry {
                status: DiffStatus::Renamed,
                old_path: del.old_path.clone(),
                new_path: add.new_path.clone(),
                old_mode: del.old_mode,
                new_mode: add.new_mode,
                old_oid: del.old_oid,
                new_oid: add.new_oid,
                score: Some(*score),
            });
        }
    }

    let mut used_deleted = vec![false; deleted.len()];
    let mut used_added = vec![false; added.len()];
    let mut renames: Vec<DiffEntry> = Vec::new();

    // Git always runs exact rename detection, regardless of renameLimit.
    let mut exact_scores: Vec<(u32, usize, usize)> = Vec::new();
    for (di, del) in deleted.iter().enumerate() {
        poll_rename_cancel(cancel_watch)?;
        for (ai, add) in added.iter().enumerate() {
            if del.old_oid == add.new_oid {
                exact_scores.push((100, di, ai));
            }
        }
    }
    sort_rename_scores(&mut exact_scores, &deleted, &added);
    apply_rename_scores(
        &exact_scores,
        &deleted,
        &added,
        &mut used_deleted,
        &mut used_added,
        &mut renames,
    );

    let remaining_deleted: Vec<usize> = used_deleted
        .iter()
        .enumerate()
        .filter_map(|(i, used)| (!*used).then_some(i))
        .collect();
    let remaining_added: Vec<usize> = used_added
        .iter()
        .enumerate()
        .filter_map(|(i, used)| (!*used).then_some(i))
        .collect();

    let rename_limit = parse_rename_limit(config);
    if !remaining_deleted.is_empty()
        && !remaining_added.is_empty()
        && !inexact_matrix_exceeds_limit(
            remaining_added.len(),
            remaining_deleted.len(),
            rename_limit,
        )
    {
        let par_opts = diff_rename_parallel_options(config, thread_policy);

        let mut prefetch_entries: Vec<DiffEntry> =
            Vec::with_capacity(remaining_deleted.len() + remaining_added.len());
        for &di in &remaining_deleted {
            prefetch_entries.push(deleted[di].clone());
        }
        for &ai in &remaining_added {
            prefetch_entries.push(added[ai].clone());
        }
        super::tree_content::prefetch_tree_diff_blobs(odb, &prefetch_entries)?;

        let deleted_sizes: Vec<Option<usize>> = remaining_deleted
            .iter()
            .map(|&di| blob_payload_size(odb, &deleted[di].old_oid))
            .collect();
        let added_sizes: Vec<Option<usize>> = remaining_added
            .iter()
            .map(|&ai| added_entry_payload_size(odb, &added[ai], work_root))
            .collect();

        let mut load_deleted = vec![false; remaining_deleted.len()];
        let mut load_added = vec![false; remaining_added.len()];
        let mut candidate_pairs: Vec<(usize, usize)> = Vec::new();
        for (row, &di) in remaining_deleted.iter().enumerate() {
            poll_rename_cancel(cancel_watch)?;
            let del = &deleted[di];
            if !is_regularish_mode(del.old_mode) {
                continue;
            }
            let Some(ds) = deleted_sizes[row] else {
                continue;
            };
            for (col, &ai) in remaining_added.iter().enumerate() {
                let add = &added[ai];
                if !is_regularish_mode(add.new_mode) {
                    continue;
                }
                let Some(asz) = added_sizes[col] else {
                    continue;
                };
                if rename_pair_size_reachable(ds, asz, threshold) {
                    load_deleted[row] = true;
                    load_added[col] = true;
                    candidate_pairs.push((row, col));
                }
            }
        }

        if !candidate_pairs.is_empty() {
            let loads_needed = load_deleted.iter().filter(|&&b| b).count()
                + load_added.iter().filter(|&&b| b).count();
            let load_deleted_blob = |i: usize| -> Option<Vec<u8>> {
                if !load_deleted[i] {
                    return None;
                }
                odb.read(&deleted[remaining_deleted[i]].old_oid)
                    .ok()
                    .map(|obj| obj.data)
            };
            let load_added_blob = |i: usize| -> Option<Vec<u8>> {
                if !load_added[i] {
                    return None;
                }
                read_added_entry_bytes(odb, &added[remaining_added[i]], work_root)
            };
            let (deleted_contents, added_contents) = if loads_needed <= 32 {
                (
                    (0..remaining_deleted.len())
                        .map(load_deleted_blob)
                        .collect(),
                    (0..remaining_added.len()).map(load_added_blob).collect(),
                )
            } else {
                (
                    parallel_load_optional_blobs(
                        odb,
                        remaining_deleted.len(),
                        par_opts,
                        0,
                        load_deleted_blob,
                    ),
                    parallel_load_optional_blobs(
                        odb,
                        remaining_added.len(),
                        par_opts,
                        0,
                        load_added_blob,
                    ),
                )
            };

            let est_table_bytes: usize = deleted_contents
                .iter()
                .chain(added_contents.iter())
                .filter_map(|opt| opt.as_ref().map(|b| b.len()))
                .sum();
            let build_deleted_table = |row: usize| -> std::result::Result<
                Option<RenameLineTable>,
                std::convert::Infallible,
            > {
                Ok(deleted_contents[row]
                    .as_ref()
                    .map(|d| RenameLineTable::from_blob(d)))
            };
            let build_added_table = |col: usize| -> std::result::Result<
                Option<RenameLineTable>,
                std::convert::Infallible,
            > {
                Ok(added_contents[col]
                    .as_ref()
                    .map(|d| RenameLineTable::from_blob(d)))
            };
            let deleted_tables: Vec<Option<RenameLineTable>> = if loads_needed <= 32 {
                (0..remaining_deleted.len())
                    .map(|row| build_deleted_table(row).unwrap())
                    .collect()
            } else {
                parallel_map_indices(
                    odb,
                    remaining_deleted.len(),
                    par_opts,
                    est_table_bytes,
                    build_deleted_table,
                )
                .expect("deleted rename line tables")
            };
            let added_tables: Vec<Option<RenameLineTable>> = if loads_needed <= 32 {
                (0..remaining_added.len())
                    .map(|col| build_added_table(col).unwrap())
                    .collect()
            } else {
                parallel_map_indices(
                    odb,
                    remaining_added.len(),
                    par_opts,
                    est_table_bytes,
                    build_added_table,
                )
                .expect("added rename line tables")
            };

            let mut inexact_scores: Vec<(u32, usize, usize)> = Vec::new();
            for (row, col) in candidate_pairs {
                poll_rename_cancel(cancel_watch)?;
                let di = remaining_deleted[row];
                let ai = remaining_added[col];
                let (Some(old_table), Some(new_table)) = (&deleted_tables[row], &added_tables[col])
                else {
                    continue;
                };
                let score = RenameLineTable::similarity_score(old_table, new_table);
                if score >= threshold {
                    inexact_scores.push((score, di, ai));
                }
            }
            sort_rename_scores(&mut inexact_scores, &deleted, &added);
            apply_rename_scores(
                &inexact_scores,
                &deleted,
                &added,
                &mut used_deleted,
                &mut used_added,
                &mut renames,
            );
        }
    }

    // Collect unmatched entries.
    let mut result = others;
    result.extend(renames);
    for (i, entry) in deleted.into_iter().enumerate() {
        if !used_deleted[i] {
            result.push(entry);
        }
    }
    for (i, entry) in added.into_iter().enumerate() {
        if !used_added[i] {
            result.push(entry);
        }
    }

    result = prune_break_deletions_obviated_by_renames(result);
    sort_entries_by_path(&mut result);
    Ok(result)
}

/// Detect copies among diff entries.
///
/// This first runs rename detection (pairing Deleted+Added), then for any
/// remaining Added entries, looks for copy sources.
///
/// - `find_copies_harder` = false: only Modified entries are copy source candidates.
/// - `find_copies_harder` = true: also examine unmodified files from `source_tree_entries`.
///
/// `source_tree_entries` should be a list of (path, mode, oid) from the source tree;
/// used when `find_copies_harder` is true to consider unmodified files as copy sources.
/// # Errors
///
/// Returns [`Error::HashCollision`] when hashing worktree blob content for copy detection
/// would produce a known-bad SHA-1 object id.
pub fn detect_copies(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    threshold: u32,
    find_copies_harder: bool,
    source_tree_entries: &[(String, FileMode, ObjectId)],
) -> Result<Vec<DiffEntry>> {
    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    detect_copies_with_policy(
        odb,
        work_root,
        entries,
        threshold,
        find_copies_harder,
        source_tree_entries,
        &config,
        &policy,
    )
}

/// Like [`detect_copies`], but honors parallel thread policy and rename limits.
///
/// # Errors
///
/// Same as [`detect_copies`].
#[expect(clippy::too_many_arguments)]
pub fn detect_copies_with_policy(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    threshold: u32,
    find_copies_harder: bool,
    source_tree_entries: &[(String, FileMode, ObjectId)],
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    use std::collections::{HashMap, HashSet};

    // Separate entries by status.
    let mut deleted: Vec<DiffEntry> = Vec::new();
    let mut added: Vec<DiffEntry> = Vec::new();
    let mut others: Vec<DiffEntry> = Vec::new();

    for entry in entries {
        match entry.status {
            DiffStatus::Deleted => deleted.push(entry),
            DiffStatus::Added => added.push(entry),
            _ => others.push(entry),
        }
    }

    // Build source candidates: deleted files, modified files, and optionally tree entries.
    // Track which sources are from deleted files (can become renames).
    let mut sources: Vec<(String, ObjectId, bool)> = Vec::new(); // (path, oid, is_deleted)
    let mut deleted_source_idx: HashMap<String, usize> = HashMap::new();

    for entry in &deleted {
        if let Some(ref path) = entry.old_path {
            deleted_source_idx.insert(path.clone(), sources.len());
            sources.push((path.clone(), entry.old_oid, true));
        }
    }

    // Modified and type-changed files are candidates for `-C` (e.g. symlink rewrite leaves the
    // old blob available as a copy source for another path; see `t4008-diff-break-rewrite`).
    for entry in &others {
        if matches!(entry.status, DiffStatus::Modified | DiffStatus::TypeChanged) {
            if let Some(ref old_path) = entry.old_path {
                if !sources.iter().any(|(p, _, _)| p == old_path) {
                    sources.push((old_path.clone(), entry.old_oid, false));
                }
            }
        }
    }

    // With find_copies_harder, add all source tree entries.
    if find_copies_harder {
        for (path, _mode, oid) in source_tree_entries {
            if !sources.iter().any(|(p, _, _)| p == path) {
                sources.push((path.clone(), *oid, false));
            }
        }
    }

    // Modes of every source, for the old side of copy and rename entries: the deleted and
    // modified entries carry their own, and `source_tree_entries` the rest.
    let mut source_modes: Vec<(String, FileMode, ObjectId)> = source_tree_entries.to_vec();
    for entry in deleted.iter().chain(&others) {
        if let Some(ref path) = entry.old_path {
            if !source_modes.iter().any(|(p, _, _)| p == path) {
                source_modes.push((path.clone(), entry.old_mode, entry.old_oid));
            }
        }
    }

    if sources.is_empty() {
        let mut result = others;
        result.extend(deleted);
        result.extend(added);
        sort_entries_by_path(&mut result);
        return Ok(result);
    }

    let par_opts = diff_rename_parallel_options(config, thread_policy);
    let rename_limit = parse_rename_limit(config);

    fn apply_copy_score_assignments(
        scores: &[(u32, usize, usize)],
        sources: &[(String, ObjectId, bool)],
        added: &[DiffEntry],
        source_tree_entries: &[(String, FileMode, ObjectId)],
        used_added2: &mut [bool],
        renamed_deleted: &mut HashSet<usize>,
        result_entries: &mut Vec<DiffEntry>,
    ) {
        let mut used_added = vec![false; added.len()];
        let mut source_to_added: HashMap<usize, Vec<(usize, u32)>> = HashMap::new();
        for &(score, si, ai) in scores {
            if used_added[ai] {
                continue;
            }
            used_added[ai] = true;
            source_to_added.entry(si).or_default().push((ai, score));
        }

        for (&si, assignments_for_src) in &source_to_added {
            let (_, _, is_deleted) = &sources[si];
            if *is_deleted && !assignments_for_src.is_empty() {
                let rename_ai = assignments_for_src
                    .iter()
                    .max_by_key(|(ai, _score)| added[*ai].path().to_string())
                    .map(|(ai, _)| *ai);

                for &(ai, score) in assignments_for_src {
                    let (ref src_path, _, _) = sources[si];
                    let add = &added[ai];
                    let src_mode = source_tree_entries
                        .iter()
                        .find(|(p, _, _)| p == src_path)
                        .map_or(add.old_mode, |(_, m, _)| *m);

                    let is_rename = Some(ai) == rename_ai;
                    result_entries.push(DiffEntry {
                        status: if is_rename {
                            DiffStatus::Renamed
                        } else {
                            DiffStatus::Copied
                        },
                        old_path: Some(src_path.clone()),
                        new_path: add.new_path.clone(),
                        old_mode: src_mode,
                        new_mode: add.new_mode,
                        old_oid: sources[si].1,
                        new_oid: add.new_oid,
                        score: Some(score),
                    });
                    used_added2[ai] = true;
                }
                renamed_deleted.insert(si);
            } else {
                for &(ai, score) in assignments_for_src {
                    let (ref src_path, _, _) = sources[si];
                    let add = &added[ai];
                    let src_mode = source_tree_entries
                        .iter()
                        .find(|(p, _, _)| p == src_path)
                        .map_or(add.old_mode, |(_, m, _)| *m);

                    result_entries.push(DiffEntry {
                        status: DiffStatus::Copied,
                        old_path: Some(src_path.clone()),
                        new_path: add.new_path.clone(),
                        old_mode: src_mode,
                        new_mode: add.new_mode,
                        old_oid: sources[si].1,
                        new_oid: add.new_oid,
                        score: Some(score),
                    });
                    used_added2[ai] = true;
                }
            }
        }
    }

    fn added_entry_oid(
        odb: &Odb,
        add: &DiffEntry,
        added_contents: &[Option<Vec<u8>>],
        ai: usize,
    ) -> Result<ObjectId> {
        if add.new_oid != zero_oid() {
            Ok(add.new_oid)
        } else if let Some(ref data) = added_contents[ai] {
            odb.hash(ObjectKind::Blob, data)
        } else {
            Ok(zero_oid())
        }
    }

    let mut result_entries: Vec<DiffEntry> = Vec::new();
    let mut renamed_deleted: HashSet<usize> = HashSet::new();
    let mut used_added2 = vec![false; added.len()];
    let mut source_contents = vec![None; sources.len()];

    if !added.is_empty() {
        let added_contents = parallel_load_optional_blobs(odb, added.len(), par_opts, 0, |ai| {
            read_added_entry_bytes(odb, &added[ai], work_root)
        });

        // Git always scores exact OID copy/rename pairs before applying renameLimit.
        let mut exact_scores: Vec<(u32, usize, usize)> = Vec::new();
        for (si, (src_path, src_oid, _)) in sources.iter().enumerate() {
            for (ai, add) in added.iter().enumerate() {
                if add.new_path.as_deref() == Some(src_path.as_str()) {
                    continue;
                }
                let add_oid = added_entry_oid(odb, add, &added_contents, ai)?;
                if *src_oid == add_oid && add_oid != zero_oid() {
                    exact_scores.push((100, si, ai));
                }
            }
        }
        exact_scores.sort_by_key(|a| std::cmp::Reverse(a.0));
        apply_copy_score_assignments(
            &exact_scores,
            &sources,
            &added,
            &source_modes,
            &mut used_added2,
            &mut renamed_deleted,
            &mut result_entries,
        );

        let remaining_added: Vec<usize> = used_added2
            .iter()
            .enumerate()
            .filter_map(|(i, used)| (!*used).then_some(i))
            .collect();
        let remaining_source_count = sources
            .iter()
            .enumerate()
            .filter(|(si, (_, _, is_deleted))| !*is_deleted || !renamed_deleted.contains(si))
            .count();

        let skip_inexact = remaining_added.is_empty()
            || inexact_matrix_exceeds_limit(
                remaining_added.len(),
                remaining_source_count,
                rename_limit,
            );

        if !skip_inexact {
            source_contents = parallel_load_optional_blobs(odb, sources.len(), par_opts, 0, |si| {
                odb.read(&sources[si].1).ok().map(|obj| obj.data)
            });

            let est_bytes: usize = source_contents
                .iter()
                .chain(added_contents.iter())
                .filter_map(|opt| opt.as_ref().map(|b| b.len()))
                .sum();

            let source_tables: Vec<Option<RenameLineTable>> = parallel_map_indices(
                odb,
                sources.len(),
                par_opts,
                est_bytes,
                |si| -> std::result::Result<Option<RenameLineTable>, std::convert::Infallible> {
                    Ok(source_contents[si]
                        .as_ref()
                        .map(|d| RenameLineTable::from_blob(d)))
                },
            )
            .expect("copy source line tables");
            let added_tables: Vec<Option<RenameLineTable>> = parallel_map_indices(
                odb,
                added.len(),
                par_opts,
                est_bytes,
                |ai| -> std::result::Result<Option<RenameLineTable>, std::convert::Infallible> {
                    Ok(added_contents[ai]
                        .as_ref()
                        .map(|d| RenameLineTable::from_blob(d)))
                },
            )
            .expect("copy added line tables");

            let score_rows: Vec<Vec<(u32, usize, usize)>> = parallel_map_indices(
                odb,
                sources.len(),
                par_opts,
                est_bytes,
                |si| -> std::result::Result<Vec<(u32, usize, usize)>, std::convert::Infallible> {
                    if renamed_deleted.contains(&si) {
                        return Ok(Vec::new());
                    }
                    let (src_path, _, _) = &sources[si];
                    let mut row: Vec<(u32, usize, usize)> = Vec::new();
                    for &ai in &remaining_added {
                        let add = &added[ai];
                        if add.new_path.as_deref() == Some(src_path.as_str()) {
                            continue;
                        }
                        let score = match (&source_tables[si], &added_tables[ai]) {
                            (Some(old_table), Some(new_table)) => {
                                RenameLineTable::similarity_score(old_table, new_table)
                            }
                            _ => 0,
                        };
                        if score >= threshold {
                            row.push((score, si, ai));
                        }
                    }
                    Ok(row)
                },
            )
            .expect("copy score rows");
            let mut inexact_scores: Vec<(u32, usize, usize)> =
                score_rows.into_iter().flatten().collect();
            inexact_scores.sort_by_key(|a| std::cmp::Reverse(a.0));
            apply_copy_score_assignments(
                &inexact_scores,
                &sources,
                &added,
                &source_modes,
                &mut used_added2,
                &mut renamed_deleted,
                &mut result_entries,
            );
        }
    }

    // Keep deleted entries that weren't consumed by a rename.
    for entry in deleted.into_iter() {
        if let Some(ref path) = entry.old_path {
            if let Some(&si) = deleted_source_idx.get(path) {
                if renamed_deleted.contains(&si) {
                    // This deletion was consumed by a rename; skip it.
                    continue;
                }
            }
        }
        result_entries.push(entry);
    }

    let mut result = others;
    result.extend(result_entries);
    // Keep unmatched added entries.
    for (i, entry) in added.into_iter().enumerate() {
        if !used_added2[i] {
            result.push(entry);
        }
    }

    // Like `git diff -C`, only created paths become copies: a modification is a copy
    // destination only after `-B` splits it into a deletion and an addition, which
    // `postprocess_diff_entries` does before calling this.
    let mut final_result = result;
    sort_entries_by_path(&mut final_result);
    Ok(final_result)
}

/// Options for Git-style rename, copy, and break-rewrite post-processing on tree diffs.
#[derive(Debug, Clone, Copy)]
pub struct DiffPostprocessOptions {
    /// When set, `--break-rewrites` threshold on Git's internal 0–[`GIT_DIFF_MAX_SCORE`] scale
    /// (`None` = off).
    pub break_score: Option<u64>,
    /// `-M` similarity threshold (0–100); `100` disables rename detection.
    pub rename_threshold: u32,
    /// `-C` copy threshold (0–100); `None` skips copy detection.
    pub copy_threshold: Option<u32>,
    /// `-C --find-copies-harder`: treat unmodified source-tree paths as copy sources.
    pub find_copies_harder: bool,
    /// When break-rewrite runs on tree↔worktree diffs, fill `new_oid` from worktree bytes for
    /// `--raw` output (matches `git diff <tree> -B`; leave false for index↔worktree / diff-files).
    pub materialize_break_worktree_new_oid: bool,
}

impl Default for DiffPostprocessOptions {
    fn default() -> Self {
        Self {
            break_score: None,
            rename_threshold: 100,
            copy_threshold: None,
            find_copies_harder: false,
            materialize_break_worktree_new_oid: false,
        }
    }
}

/// Index and git directory used to read canonical (clean) worktree bytes during break-rewrite
/// post-processing on tree↔worktree diffs.
#[derive(Debug, Clone, Copy)]
pub struct DiffPostprocessWorktree<'a> {
    /// Git directory for config and attributes (linked worktrees / gitfiles).
    pub repository_git_dir: &'a Path,
    /// Current index; stage-0 entries supply autocrlf index-guard blobs when needed.
    pub index: &'a Index,
}

fn blob_bytes(odb: &Odb, oid: &ObjectId) -> Result<Vec<u8>> {
    Ok(odb.read(oid)?.data)
}

/// Bytes on the "new" side of a diff entry (ODB blob, or worktree file when `new_oid` is null).
///
/// When `worktree` is set, worktree paths are normalized with the same attribute/filter rules as
/// [`diff_tree_to_worktree`]; otherwise raw filesystem bytes are read.
fn read_new_side_bytes(
    odb: &Odb,
    entry: &DiffEntry,
    work_root: Option<&Path>,
    worktree: Option<&DiffPostprocessWorktree<'_>>,
    config: &ConfigSet,
) -> Result<Vec<u8>> {
    if entry.new_oid != zero_oid() {
        return blob_bytes(odb, &entry.new_oid);
    }
    let path = entry.new_path.as_deref().ok_or_else(|| {
        Error::ObjectNotFound("modified entry missing new_path for worktree read".into())
    })?;
    let root = work_root.ok_or_else(|| {
        Error::ObjectNotFound(format!(
            "worktree bytes required for {} but no work_root was provided",
            path
        ))
    })?;
    if let Some(wctx) = worktree {
        let mode = entry.new_mode.bits();
        let index_entry = wctx.index.get(path.as_bytes(), 0);
        return read_worktree_side_bytes_for_diff(odb, root, path, mode, config, index_entry, None);
    }
    fs::read(root.join(path)).map_err(Error::Io)
}

/// True when clean-smudged worktree bytes differ from the raw on-disk file (e.g. `eol=lf` + CRLF).
fn worktree_side_differs_after_normalize(
    odb: &Odb,
    entry: &DiffEntry,
    work_root: &Path,
    worktree: &DiffPostprocessWorktree<'_>,
    config: &ConfigSet,
) -> Result<bool> {
    let path = entry.new_path.as_deref().ok_or_else(|| {
        Error::ObjectNotFound("modified entry missing new_path for worktree read".into())
    })?;
    let raw = fs::read(work_root.join(path)).map_err(Error::Io)?;
    let canonical = read_new_side_bytes(odb, entry, Some(work_root), Some(worktree), config)?;
    Ok(raw != canonical)
}

fn split_broken_modifications(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    break_score: u64,
    worktree: Option<&DiffPostprocessWorktree<'_>>,
    config: &ConfigSet,
) -> Result<Vec<DiffEntry>> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if e.status != DiffStatus::Modified {
            out.push(e);
            continue;
        }
        let old = blob_bytes(odb, &e.old_oid)?;
        let new = read_new_side_bytes(odb, &e, work_root, worktree, config)?;
        if should_break_rewrite_pair(&old, &new, break_score) {
            let path = e.path().to_owned();
            out.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path.clone()),
                new_path: None,
                old_mode: e.old_mode,
                new_mode: FileMode::ABSENT,
                old_oid: e.old_oid,
                new_oid: zero_oid(),
                score: None,
            });
            out.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path),
                old_mode: FileMode::ABSENT,
                new_mode: e.new_mode,
                old_oid: zero_oid(),
                new_oid: e.new_oid,
                score: None,
            });
        } else {
            out.push(e);
        }
    }
    Ok(out)
}

fn annotate_broken_modifications(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    break_score: u64,
    worktree: Option<&DiffPostprocessWorktree<'_>>,
    config: &ConfigSet,
    materialize_worktree_oid: bool,
) -> Result<Vec<DiffEntry>> {
    let mut out = Vec::with_capacity(entries.len());
    for mut e in entries {
        if e.status == DiffStatus::Modified {
            let old = blob_bytes(odb, &e.old_oid)?;
            let new = read_new_side_bytes(odb, &e, work_root, worktree, config)?;
            if should_break_rewrite_pair(&old, &new, break_score) {
                // `git diff-files -B` keeps plain `M` when the worktree side is still null.
                // `git diff <tree> -B` materializes the clean OID; `M###` appears only when
                // attributes/filters changed the bytes used for break scoring (e.g. `eol=lf`).
                let show_score = if e.new_oid != zero_oid() {
                    true
                } else if materialize_worktree_oid {
                    match (work_root, worktree) {
                        (Some(root), Some(wctx)) => {
                            worktree_side_differs_after_normalize(odb, &e, root, wctx, config)?
                        }
                        _ => false,
                    }
                } else {
                    false
                };
                if show_score {
                    e.score = rewrite_dissimilarity_index_percent(&old, &new);
                }
            }
        }
        out.push(e);
    }
    Ok(out)
}

fn materialize_break_worktree_new_oids(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: &mut [DiffEntry],
    worktree: Option<&DiffPostprocessWorktree<'_>>,
    config: &ConfigSet,
) -> Result<()> {
    for e in entries {
        if e.new_oid != zero_oid() {
            continue;
        }
        if !matches!(e.status, DiffStatus::Modified | DiffStatus::Added) {
            continue;
        }
        let bytes = read_new_side_bytes(odb, e, work_root, worktree, config)?;
        e.new_oid = odb.hash(ObjectKind::Blob, &bytes)?;
    }
    Ok(())
}

/// After break-rewrite splits a modified path into delete+add, rename detection may pair the add
/// with another deletion (`g` → `f`). Git drops the unused delete at the rename target path.
fn prune_break_deletions_obviated_by_renames(entries: Vec<DiffEntry>) -> Vec<DiffEntry> {
    use std::collections::HashSet;
    let rename_targets: HashSet<String> = entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .filter_map(|e| e.new_path.clone())
        .collect();
    if rename_targets.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| {
            if e.status != DiffStatus::Deleted {
                return true;
            }
            e.old_path
                .as_ref()
                .is_none_or(|p| !rename_targets.contains(p))
        })
        .collect()
}

/// Apply break-rewrite, rename, and copy detection to a flat tree or index diff.
///
/// `source_tree_oid` supplies `--find-copies-harder` candidates; `work_root` is used when added
/// entries carry a zero OID (index↔worktree). Pass `worktree` when break-rewrite runs on a
/// tree↔worktree diff so worktree-side bytes honor attributes and filters.
///
/// # Errors
///
/// Propagates object read failures while evaluating break scores.
#[allow(clippy::too_many_arguments)]
pub fn postprocess_diff_entries(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    source_tree_oid: Option<&ObjectId>,
    work_root: Option<&Path>,
    worktree: Option<&DiffPostprocessWorktree<'_>>,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    opts: &DiffPostprocessOptions,
) -> Result<Vec<DiffEntry>> {
    let mut entries = entries;
    if let Some(break_score) = opts.break_score {
        let rename_or_copy = opts.rename_threshold < 100 || opts.copy_threshold.is_some();
        entries = if rename_or_copy {
            split_broken_modifications(odb, work_root, entries, break_score, worktree, config)?
        } else {
            annotate_broken_modifications(
                odb,
                work_root,
                entries,
                break_score,
                worktree,
                config,
                opts.materialize_break_worktree_new_oid,
            )?
        };
        if opts.materialize_break_worktree_new_oid {
            materialize_break_worktree_new_oids(odb, work_root, &mut entries, worktree, config)?;
        }
    }

    let source_tree_entries = source_tree_oid
        .map(|t| flatten_tree_entries(odb, t))
        .transpose()?
        .unwrap_or_default();

    if opts.copy_threshold.is_some() || opts.find_copies_harder {
        let threshold = opts.copy_threshold.unwrap_or(opts.rename_threshold.min(99));
        detect_copies_with_policy(
            odb,
            work_root,
            entries,
            threshold,
            opts.find_copies_harder,
            &source_tree_entries,
            config,
            thread_policy,
        )
    } else if opts.rename_threshold < 100 {
        detect_renames_with_policy(
            odb,
            work_root,
            entries,
            opts.rename_threshold,
            config,
            thread_policy,
            None,
        )
    } else {
        Ok(entries)
    }
}

/// Depth-first list of `(path, mode, oid)` blob entries in a tree (for copy detection).
pub fn flatten_tree_entries(
    odb: &Odb,
    tree_oid: &ObjectId,
) -> Result<Vec<(String, FileMode, ObjectId)>> {
    fn walk(
        odb: &Odb,
        tree_oid: &ObjectId,
        prefix: &str,
        out: &mut Vec<(String, FileMode, ObjectId)>,
    ) -> Result<()> {
        let obj = odb.read(tree_oid)?;
        let oid_len = odb.hash_algo().len();
        for e in TreeRefIter::with_oid_len(&obj.data, oid_len) {
            let e = e?;
            let name = String::from_utf8_lossy(e.name);
            let path = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            if e.mode == 0o040000 {
                walk(odb, &e.oid, &path, out)?;
            } else {
                out.push((path, FileMode::new(e.mode), e.oid));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(odb, tree_oid, "", &mut out)?;
    Ok(out)
}

/// Apply Git-style rename and optional copy detection for index↔worktree diffs.
///
/// When `copies` is true (Git `diff.renames` / `status.renames` set to `copy`/`copies`),
/// runs copy detection, which also pairs deleted sources with one rename and any additional
/// destinations as copies.
///
/// # Errors
///
/// Propagates errors from reading the `head_tree` object from `odb`.
#[allow(clippy::too_many_arguments)]
pub fn status_apply_rename_copy_detection(
    odb: &Odb,
    unstaged_raw: Vec<DiffEntry>,
    threshold: u32,
    copies: bool,
    head_tree: Option<&ObjectId>,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    cancel_watch: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Vec<DiffEntry>> {
    if !copies {
        return detect_renames_with_policy(
            odb,
            None,
            unstaged_raw,
            threshold,
            config,
            thread_policy,
            cancel_watch,
        );
    }
    let source_tree_entries: Vec<(String, FileMode, ObjectId)> = match head_tree {
        Some(oid) => flatten_tree(odb, oid, "")?
            .into_iter()
            .map(|e| (e.path, FileMode::new(e.mode), e.oid))
            .collect(),
        None => Vec::new(),
    };
    detect_copies_with_policy(
        odb,
        None,
        unstaged_raw,
        threshold,
        false,
        &source_tree_entries,
        config,
        thread_policy,
    )
}

/// Check if two entries share the same filename (basename).
fn same_basename(del: &DiffEntry, add: &DiffEntry) -> bool {
    let old = del.old_path.as_deref().unwrap_or("");
    let new = add.new_path.as_deref().unwrap_or("");
    let old_base = old.rsplit('/').next().unwrap_or(old);
    let new_base = new.rsplit('/').next().unwrap_or(new);
    old_base == new_base && !old_base.is_empty()
}

const RENAME_SCORE_MAX: u32 = 100;

/// Uncompressed blob size without inflating object bodies (for rename pre-filtering).
fn blob_payload_size(odb: &Odb, oid: &ObjectId) -> Option<usize> {
    if oid.is_zero() {
        return None;
    }
    odb.read_info(oid).ok().map(|info| info.size as usize)
}

/// Payload size for an added diff side (ODB blob or worktree bytes when not stored yet).
fn added_entry_payload_size(odb: &Odb, add: &DiffEntry, work_root: Option<&Path>) -> Option<usize> {
    if !add.new_oid.is_zero() {
        if let Some(size) = blob_payload_size(odb, &add.new_oid) {
            return Some(size);
        }
    }
    read_added_entry_bytes(odb, add, work_root).map(|data| data.len())
}

/// Git `diffcore-delta` size gate: skip pairs that cannot reach `minimum_score`.
fn rename_pair_size_reachable(src_size: usize, dst_size: usize, minimum_score: u32) -> bool {
    let max_size = src_size.max(dst_size);
    let min_size = src_size.min(dst_size);
    if max_size == 0 {
        return minimum_score <= RENAME_SCORE_MAX;
    }
    (max_size - min_size).saturating_mul(RENAME_SCORE_MAX as usize)
        <= max_size.saturating_mul((RENAME_SCORE_MAX - minimum_score) as usize)
}

/// Compute a similarity percentage (0–100) between two byte slices.
///
/// Matches Git rename detection: span-hash intersection (`hash_chars` in diffcore-delta),
/// `score = copied_bytes * 100 / max(src_size, dst_size)` on raw blob sizes.
fn compute_similarity(old: &[u8], new: &[u8]) -> u32 {
    RenameLineTable::similarity_score(
        &RenameLineTable::from_blob(old),
        &RenameLineTable::from_blob(new),
    )
}

/// Compute rename/copy similarity percentage (0–100) between two byte slices.
///
/// Uses Git `diffcore-delta` span hashing (`hash_chars`): 64-byte or newline-bounded
/// chunks, CR skipped before LF in text blobs, `score = copied * 100 / max(len)`.
#[must_use]
pub fn rename_similarity_score(old: &[u8], new: &[u8]) -> u32 {
    compute_similarity(old, new)
}

// ── Output formatting ───────────────────────────────────────────────

/// Format a diff entry in Git's raw diff format.
///
/// Example: `:100644 100644 abc1234... def5678... M\tfile.txt`
pub fn format_raw(entry: &DiffEntry) -> String {
    let path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            format!(
                "{}\t{}",
                entry.old_path.as_deref().unwrap_or(""),
                entry.new_path.as_deref().unwrap_or("")
            )
        }
        _ => entry.path().to_owned(),
    };

    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{:03}", s),
        (DiffStatus::Copied, Some(s)) => format!("C{:03}", s),
        (DiffStatus::Modified, Some(s)) => format!("M{s:03}"),
        (DiffStatus::TypeChanged, Some(s)) => format!("T{s:03}"),
        _ => entry.status.letter().to_string(),
    };

    let (old_hex, new_hex) = raw_oid_hex_pair(&entry.old_oid, &entry.new_oid);
    format!(
        ":{} {} {} {} {}\t{}",
        entry.old_mode, entry.new_mode, old_hex, new_hex, status_str, path
    )
}

/// Render a diff entry's `(old, new)` OIDs as full hex for `--raw` output,
/// widening any null OID to the repository's hash width.
///
/// Diff entries store null OIDs at SHA-1 width regardless of the repository
/// algorithm, but `git diff --raw` prints them at the real hash width (64
/// zeros in a SHA-256 repo). A diff entry never has both sides null, so the
/// width is taken from whichever side carries a real object.
fn raw_oid_hex_pair(old: &ObjectId, new: &ObjectId) -> (String, String) {
    let width = if !old.is_zero() {
        old.algo().hex_len()
    } else if !new.is_zero() {
        new.algo().hex_len()
    } else {
        old.algo().hex_len()
    };
    let render = |oid: &ObjectId| {
        if oid.is_zero() {
            "0".repeat(width)
        } else {
            oid.to_hex()
        }
    };
    (render(old), render(new))
}

/// Format a diff entry with abbreviated OIDs.
pub fn format_raw_abbrev(entry: &DiffEntry, abbrev_len: usize) -> String {
    let ellipsis = if crate::environment::Environment::empty()
        .var("GIT_PRINT_SHA1_ELLIPSIS")
        .as_deref()
        == Some("yes")
    {
        "..."
    } else {
        ""
    };
    let old_hex = format!("{}", entry.old_oid);
    let new_hex = format!("{}", entry.new_oid);
    let old_abbrev = &old_hex[..abbrev_len.min(old_hex.len())];
    let new_abbrev = &new_hex[..abbrev_len.min(new_hex.len())];

    // Renames/copies carry a similarity score and a `<old>\t<new>` path pair.
    let path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => format!(
            "{}\t{}",
            entry.old_path.as_deref().unwrap_or(""),
            entry.new_path.as_deref().unwrap_or("")
        ),
        _ => entry.path().to_owned(),
    };
    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{s:03}"),
        (DiffStatus::Copied, Some(s)) => format!("C{s:03}"),
        (DiffStatus::Modified, Some(s)) => format!("M{s:03}"),
        (DiffStatus::TypeChanged, Some(s)) => format!("T{s:03}"),
        _ => entry.status.letter().to_string(),
    };

    format!(
        ":{} {} {}{} {}{} {}\t{}",
        entry.old_mode,
        entry.new_mode,
        old_abbrev,
        ellipsis,
        new_abbrev,
        ellipsis,
        status_str,
        path
    )
}

/// Generate a unified diff patch for two blobs.
///
/// # Parameters
///
/// - `old_content` — the old file content (empty for added files).
/// - `new_content` — the new file content (empty for deleted files).
/// - `old_path` — display path for the old side.
/// - `new_path` — display path for the new side.
/// - `context_lines` — number of context lines around changes (default: 3).
/// - Inter-hunk context defaults to `0` (see [`unified_diff_with_prefix`]).
///
/// # Returns
///
/// The unified diff as a string.
pub fn unified_diff(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    unified_diff_with_prefix(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        0,
        "a/",
        "b/",
        indent_heuristic,
        quote_path_fully,
    )
}

/// Same as `unified_diff` but with configurable source/destination prefixes.
///
/// `inter_hunk_context` is Git's `--inter-hunk-context`: adjacent hunks merge when
/// the unchanged gap between them is at most `2 * context_lines + inter_hunk_context` lines.
#[allow(clippy::too_many_arguments)] // Mirrors Git-style unified diff parameters.
pub fn unified_diff_with_prefix(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    unified_diff_with_prefix_and_funcname(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        None,
        indent_heuristic,
        quote_path_fully,
    )
}

/// Same as [`unified_diff_with_prefix`] with optional custom hunk-header
/// function-name matching.
#[allow(clippy::too_many_arguments)]
pub fn unified_diff_with_prefix_and_funcname(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    unified_diff_with_prefix_and_funcname_and_algorithm(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        funcname_matcher,
        similar::Algorithm::Myers,
        false,
        false,
        indent_heuristic,
        quote_path_fully,
    )
}

/// Same as [`unified_diff_with_prefix_and_funcname_and_algorithm`], streaming to `out`.
///
/// # Errors
///
/// Returns I/O errors from `out`.
#[allow(clippy::too_many_arguments)]
pub fn write_unified_diff_with_prefix_and_funcname_and_algorithm<W: std::io::Write>(
    out: &mut W,
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    algorithm: similar::Algorithm,
    function_context: bool,
    use_git_histogram: bool,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> Result<()> {
    if function_context {
        let patch = unified_diff_with_function_context(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            inter_hunk_context,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            algorithm,
            indent_heuristic,
            quote_path_fully,
        );
        out.write_all(patch.as_bytes())?;
        return Ok(());
    }

    if use_git_histogram {
        let patch = unified_diff_histogram_with_prefix_and_funcname_indent(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            inter_hunk_context,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            indent_heuristic,
            quote_path_fully,
        );
        out.write_all(patch.as_bytes())?;
        return Ok(());
    }

    if algorithm == similar::Algorithm::Myers && !function_context {
        let patch = unified_diff_git_myers_with_prefix_and_funcname(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            inter_hunk_context,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            indent_heuristic,
            quote_path_fully,
        );
        out.write_all(patch.as_bytes())?;
        return Ok(());
    }

    if algorithm == similar::Algorithm::Lcs && !function_context {
        let patch = unified_diff_git_minimal_with_prefix_and_funcname(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            inter_hunk_context,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            indent_heuristic,
            quote_path_fully,
        );
        out.write_all(patch.as_bytes())?;
        return Ok(());
    }

    if algorithm == similar::Algorithm::Patience && !function_context {
        let patch = unified_diff_git_patience_with_prefix_and_funcname(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            inter_hunk_context,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            indent_heuristic,
            quote_path_fully,
        );
        out.write_all(patch.as_bytes())?;
        return Ok(());
    }

    use crate::quote_path::format_diff_path_with_prefix;
    use similar::{udiff::UnifiedDiffHunk, TextDiff};

    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_lines(old_content, new_content);
    let compacted_ops = diff_indent_heuristic::diff_lines_ops_compacted(
        old_content,
        new_content,
        algorithm,
        indent_heuristic,
    );

    if old_path == "/dev/null" {
        out.write_all(b"--- /dev/null\n")?;
    } else if src_prefix.is_empty() {
        writeln!(out, "--- {old_path}")?;
    } else {
        out.write_all(b"--- ")?;
        out.write_all(
            format_diff_path_with_prefix(src_prefix, old_path, quote_path_fully).as_bytes(),
        )?;
        out.write_all(b"\n")?;
    }
    if new_path == "/dev/null" {
        out.write_all(b"+++ /dev/null\n")?;
    } else if dst_prefix.is_empty() {
        writeln!(out, "+++ {new_path}")?;
    } else {
        out.write_all(b"+++ ")?;
        out.write_all(
            format_diff_path_with_prefix(dst_prefix, new_path, quote_path_fully).as_bytes(),
        )?;
        out.write_all(b"\n")?;
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    let max_common_gap = context_lines
        .saturating_mul(2)
        .saturating_add(inter_hunk_context);
    let op_groups = group_diff_ops_gap(compacted_ops, context_lines, max_common_gap);

    let mut hunk_buf = String::new();
    let mut eof_annot = UnifiedDiffNoNewlineAnnotator::new(old_content, new_content);
    for ops in op_groups {
        if ops.is_empty() {
            continue;
        }
        let hunk = UnifiedDiffHunk::new(ops, &diff, true);
        hunk_buf.clear();
        use std::fmt::Write as FmtWrite;
        let _ = FmtWrite::write_fmt(&mut hunk_buf, format_args!("{hunk}"));
        let mut fragment = hunk_buf.clone();
        if let Some(first_newline) = hunk_buf.find('\n') {
            let header_line = &hunk_buf[..first_newline];
            let rest = &hunk_buf[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher, Some(old_path))
            {
                fragment.clear();
                fragment.push_str(header_line);
                fragment.push(' ');
                fragment.push_str(&func_ctx);
                fragment.push_str(rest);
            }
        }
        eof_annot.write_hunk(out, &fragment)?;
    }
    Ok(())
}

/// Same as [`unified_diff_with_prefix_and_funcname`] but allows callers to
/// choose the line diff algorithm used for hunk generation.
///
/// When `function_context` is true (`git diff -W`), hunks are expanded to
/// whole logical functions using the same rules as Git's `XDL_EMIT_FUNCCONTEXT`.
#[allow(clippy::too_many_arguments)]
pub fn unified_diff_with_prefix_and_funcname_and_algorithm(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    algorithm: similar::Algorithm,
    function_context: bool,
    use_git_histogram: bool,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    let mut output = Vec::new();
    if write_unified_diff_with_prefix_and_funcname_and_algorithm(
        &mut output,
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        funcname_matcher,
        algorithm,
        function_context,
        use_git_histogram,
        indent_heuristic,
        quote_path_fully,
    )
    .is_err()
    {
        output.clear();
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// Group diff ops into hunks like Git's `xdl_get_hunk`: two changes merge into one hunk while
/// the run of unchanged lines between them is at most `max_common_gap`
/// (`2 * context + inter_hunk_context`), and each hunk keeps at most `context` unchanged lines
/// at its edges. Unlike `similar::group_diff_ops`, the split threshold is decoupled from the
/// edge context so odd gap limits group exactly like Git.
pub(crate) fn group_diff_ops_gap(
    mut ops: Vec<similar::DiffOp>,
    context: usize,
    max_common_gap: usize,
) -> Vec<Vec<similar::DiffOp>> {
    use similar::DiffOp;
    if ops.is_empty() {
        return vec![];
    }

    let mut pending_group = Vec::new();
    let mut rv = Vec::new();

    if let Some(DiffOp::Equal {
        old_index,
        new_index,
        len,
    }) = ops.first_mut()
    {
        let offset = (*len).saturating_sub(context);
        *old_index += offset;
        *new_index += offset;
        *len -= offset;
    }

    if let Some(DiffOp::Equal { len, .. }) = ops.last_mut() {
        *len -= (*len).saturating_sub(context);
    }

    for op in ops.into_iter() {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            // End the current group and start a new one whenever the unchanged
            // run is too long to fuse the surrounding changes.
            if len > max_common_gap {
                pending_group.push(DiffOp::Equal {
                    old_index,
                    new_index,
                    len: context,
                });
                rv.push(pending_group);
                let offset = len.saturating_sub(context);
                pending_group = vec![DiffOp::Equal {
                    old_index: old_index + offset,
                    new_index: new_index + offset,
                    len: len - offset,
                }];
                continue;
            }
        }
        pending_group.push(op);
    }

    match &pending_group[..] {
        &[] | &[similar::DiffOp::Equal { .. }] => {}
        _ => rv.push(pending_group),
    }

    rv
}
#[expect(clippy::too_many_arguments)]
/// `git diff -W`: expand each hunk to include full function bodies (see Git `xemit.c`).
fn unified_diff_with_function_context(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    algorithm: similar::Algorithm,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;
    use similar::{udiff::UnifiedDiffHunk, TextDiff};

    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_lines(old_content, new_content);

    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let n_old = old_lines.len();
    let n_new = new_lines.len();

    // Group changes the way Git's xdl_get_hunk does: merge while the unchanged
    // gap is at most `2*context + inter_hunk_context`. `similar::group_diff_ops`
    // splits only at gap > `2*radius`, which over-merges changes in *different*
    // functions (e.g. a leading insertion and a body change) into one hunk and
    // then over-expands the function context (t4015 #136). Use the gap-correct
    // grouping (same as the non-function-context path).
    let max_common_gap = context_lines
        .saturating_mul(2)
        .saturating_add(inter_hunk_context);
    let all_ops = diff.ops().to_vec();
    let op_groups = group_diff_ops_gap(all_ops.clone(), context_lines, max_common_gap);

    let mut ranges: Vec<(usize, usize, usize, usize)> = Vec::new();

    for ops in op_groups {
        if ops.is_empty() {
            continue;
        }
        let i1_anchor = func_context_old_anchor(&ops, n_old);
        let i1_end = hunk_old_change_end_exclusive(&ops);
        let skip_preimage_pull =
            append_with_whole_function_added(&ops, n_old, n_new, &new_lines, funcname_matcher);
        let hunk = UnifiedDiffHunk::new(ops, &diff, true);
        let hunk_str = format!("{hunk}");
        let header_line = hunk_str
            .lines()
            .next()
            .unwrap_or("")
            .trim_end_matches(['\r', '\n']);
        let Some((base_s1, _base_e1, _base_s2, _base_e2)) =
            parse_unified_hunk_header_ranges(header_line)
        else {
            continue;
        };

        let ctx = context_lines;
        let (s1, e1, s2, e2) = if skip_preimage_pull {
            let s = n_old.saturating_sub(ctx);
            let s2 = map_old_line_to_new(&all_ops, s, n_new).min(n_new);
            (s, n_old, s2, n_new)
        } else {
            let mut s1 = base_s1.saturating_sub(ctx);
            let mut s2 = map_old_line_to_new(&all_ops, s1, n_new).min(n_new);

            let base_pre_s1 = i1_anchor.saturating_sub(ctx);
            if base_pre_s1 < s1 {
                s1 = base_pre_s1;
                s2 = map_old_line_to_new(&all_ops, s1, n_new).min(n_new);
            }

            let fs1 = expand_func_pre_start(s1, i1_anchor, n_old, &old_lines, funcname_matcher);
            if fs1 < s1 {
                s1 = fs1;
                s2 = map_old_line_to_new(&all_ops, s1, n_new).min(n_new);
            }

            // `i1_end` is the exclusive end of the changed region; its post-image
            // context is `ctx` lines (Git's `xche->i1 + xche->chg1 + lctx`). The
            // hunk's `base_e1` already includes the group's trailing context, so
            // use the change end + ctx directly to avoid double-counting context
            // (which over-extended the hunk and merged separate functions — t4015 #136).
            let mut e1 = (i1_end + ctx).min(n_old);
            let mut e2 = map_old_line_to_new(&all_ops, e1, n_new).min(n_new);
            let fe1 = expand_func_post_end(e1, i1_end, n_old, &old_lines, funcname_matcher);
            if fe1 > e1 {
                e1 = fe1;
                e2 = map_old_line_to_new(&all_ops, e1, n_new).min(n_new);
            }
            (s1, e1, s2, e2)
        };

        ranges.push((s1, e1, s2, e2));
    }

    // Merge ranges whose function-context expansion made them overlap on the old
    // side (Git emits a single hunk in that case).
    ranges.sort_by_key(|r| (r.0, r.2));
    let mut merged: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(ranges.len());
    for (s1, e1, s2, e2) in ranges {
        if let Some(last) = merged.last_mut() {
            if s1 < last.1 {
                last.1 = last.1.max(e1);
                last.3 = last.3.max(e2);
                continue;
            }
        }
        merged.push((s1, e1, s2, e2));
    }
    let ranges = merged;

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    for (s1, e1, s2, e2) in ranges {
        if s1 >= e1 && s2 >= e2 {
            continue;
        }
        let old_seg =
            line_slice_for_diff_with_eof_nl(&old_lines, s1, e1, old_content.ends_with('\n'));
        let new_seg =
            line_slice_for_diff_with_eof_nl(&new_lines, s2, e2, new_content.ends_with('\n'));
        let inner_ctx = old_seg.lines().count().max(new_seg.lines().count()).max(1);
        let piece = unified_diff_with_prefix_and_funcname_and_algorithm(
            &old_seg,
            &new_seg,
            old_path,
            new_path,
            inner_ctx,
            0,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            algorithm,
            false,
            false,
            indent_heuristic,
            quote_path_fully,
        );
        let shifted = shift_unified_hunk_headers_to_full_file(&piece, s1, s2);
        let with_func =
            enrich_unified_hunk_headers_funcname(&shifted, &old_lines, funcname_matcher);
        for line in with_func.lines() {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                continue;
            }
            output.push_str(line);
            output.push('\n');
        }
    }

    output
}

/// `piece` is a unified diff for a slice of the file; hunk headers use 1-based
/// coordinates relative to that slice. Shift them by `delta_old` / `delta_new`
/// (0-based offsets of the slice in the full file) so the combined patch applies
/// to the whole file.
fn shift_unified_hunk_headers_to_full_file(
    patch: &str,
    delta_old: usize,
    delta_new: usize,
) -> String {
    if delta_old == 0 && delta_new == 0 {
        return patch.to_owned();
    }
    let mut out = String::with_capacity(patch.len());
    for line in patch.lines() {
        if let Some(shifted) = shift_one_unified_hunk_header(line, delta_old, delta_new) {
            out.push_str(&shifted);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn shift_one_unified_hunk_header(line: &str, delta_old: usize, delta_new: usize) -> Option<String> {
    let rest = line.strip_prefix("@@ ")?;
    let (old_chunk, after_plus) = rest.split_once(" +")?;
    let old_spec = old_chunk.strip_prefix('-')?;
    let (new_spec, suffix) = after_plus.split_once(" @@")?;
    let shifted_old = shift_unified_range_spec(old_spec, delta_old)?;
    let shifted_new = shift_unified_range_spec(new_spec, delta_new)?;
    Some(format!("@@ -{shifted_old} +{shifted_new} @@{suffix}"))
}

fn shift_unified_range_spec(spec: &str, delta: usize) -> Option<String> {
    let spec = spec.trim();
    if let Some((start_s, count_s)) = spec.split_once(',') {
        let start: usize = start_s.parse().ok()?;
        let count: usize = count_s.parse().ok()?;
        Some(format!("{},{}", start.saturating_add(delta), count))
    } else {
        let start: usize = spec.parse().ok()?;
        Some(format!("{}", start.saturating_add(delta)))
    }
}

/// Re-attach `@@ ... @@ <funcname>` using full-file line indices (inner diffs use slices).
fn enrich_unified_hunk_headers_funcname(
    patch: &str,
    full_old_lines: &[&str],
    funcname_matcher: Option<&FuncnameMatcher>,
) -> String {
    let mut out = String::with_capacity(patch.len());
    for line in patch.lines() {
        if let Some(fixed) = enrich_one_hunk_header_funcname(line, full_old_lines, funcname_matcher)
        {
            out.push_str(&fixed);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn enrich_one_hunk_header_funcname(
    line: &str,
    full_old_lines: &[&str],
    funcname_matcher: Option<&FuncnameMatcher>,
) -> Option<String> {
    let after_at = line.strip_prefix("@@ ")?;
    let idx = after_at.find(" @@")?;
    let mid = after_at[..idx].trim();
    let tail = after_at[idx + 3..].trim_start();
    let header_for_parse = format!("@@ {mid} @@");
    let func = extract_function_context(&header_for_parse, full_old_lines, funcname_matcher, None);
    Some(if let Some(f) = func {
        format!("@@ {mid} @@ {f}")
    } else if !tail.is_empty() {
        format!("@@ {mid} @@ {tail}")
    } else {
        format!("@@ {mid} @@")
    })
}

fn line_slice_for_diff_with_eof_nl(
    lines: &[&str],
    start: usize,
    end: usize,
    full_file_ends_with_newline: bool,
) -> String {
    if start >= end {
        return String::new();
    }
    let mut s = lines[start..end].join("\n");
    let slice_is_suffix_of_file = end == lines.len();
    let need_trailing_nl = if slice_is_suffix_of_file {
        full_file_ends_with_newline
    } else {
        true
    };
    if need_trailing_nl && !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Map a 0-based old line index to the corresponding 0-based new line index using the full-file
/// diff ops (Git aligns context across deletions/insertions).
fn map_old_line_to_new(ops: &[similar::DiffOp], old_line: usize, n_new: usize) -> usize {
    use similar::DiffOp;
    let mut n = 0usize;
    for op in ops {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                if old_index + len <= old_line {
                    n = new_index + len;
                    continue;
                }
                if old_index < old_line {
                    let take = old_line - old_index;
                    return (new_index + take).min(n_new);
                }
                return new_index.min(n_new);
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                if old_index + old_len <= old_line {
                    n = new_index;
                    continue;
                }
                if old_index < old_line {
                    return new_index.min(n_new);
                }
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                if old_index < old_line {
                    n = new_index + new_len;
                    continue;
                }
                if old_index == old_line {
                    // `old_line` is an exclusive end or insertion point aligned with this insert
                    // (e.g. EOF append maps to after the inserted block).
                    return (new_index + new_len).min(n_new);
                }
                return new_index.min(n_new);
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                if old_index + old_len <= old_line {
                    n = new_index + new_len;
                    continue;
                }
                if old_index < old_line {
                    let into_old = old_line - old_index;
                    let mapped = new_index + into_old.min(new_len);
                    return mapped.min(n_new);
                }
                return new_index.min(n_new);
            }
        }
    }
    n.min(n_new)
}

/// Parse `@@ -old +new @@` into 0-based half-open ranges in each file.
fn parse_unified_hunk_header_ranges(header: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = header.strip_prefix("@@ ")?;
    let (old_tok, rest2) = rest.split_once(" +")?;
    let old_tok = old_tok.strip_prefix('-')?;
    let new_tok = rest2.split_once(" @@").map(|(a, _)| a)?;

    fn parse_side(spec: &str) -> Option<(usize, usize)> {
        let spec = spec.trim();
        let (start_one_based, count) = if let Some((a, b)) = spec.split_once(',') {
            (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)
        } else {
            let s = spec.parse::<usize>().ok()?;
            (s, 1usize)
        };
        let s0 = start_one_based.saturating_sub(1);
        let e0 = s0.saturating_add(count);
        Some((s0, e0))
    }

    let (os, oe) = parse_side(old_tok)?;
    let (ns, ne) = parse_side(new_tok)?;
    Some((os, oe, ns, ne))
}

/// Git `xemit.c`: when a hunk only inserts at EOF (first inserted line is `new_index == n_old`)
/// and the added text already contains a funcname line, do not pull extra context from the preimage.
fn append_with_whole_function_added(
    ops: &[similar::DiffOp],
    n_old: usize,
    n_new: usize,
    new_lines: &[&str],
    matcher: Option<&FuncnameMatcher>,
) -> bool {
    use similar::DiffOp;
    if n_old == 0 {
        return false;
    }
    let mut only_ins_or_eq = true;
    let mut min_new_ins = usize::MAX;
    for op in ops {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                min_new_ins = min_new_ins.min(new_index);
                if new_len == 0 {
                    only_ins_or_eq = false;
                }
            }
            DiffOp::Delete { .. } | DiffOp::Replace { .. } => {
                only_ins_or_eq = false;
            }
        }
    }
    let mut insert_at_eof = false;
    for op in ops {
        if let DiffOp::Insert { old_index, .. } = *op {
            if old_index == n_old {
                insert_at_eof = true;
                break;
            }
        }
    }
    let append_at_eof = min_new_ins == n_old || insert_at_eof;
    if !only_ins_or_eq || !append_at_eof || min_new_ins == usize::MAX {
        return false;
    }
    // Git only skips preimage pull when the inserted block is clearly a new logical
    // function (see `xemit.c` walking `xdf2` for `is_func_rec`). A loose "any line
    // looks like a function" check would match `return` / `printf` and break `-W`
    // hunks that still need preimage context (t4051 `extended`).
    let mut j = min_new_ins;
    while j < n_new {
        let line = new_lines[j];
        if line.trim().is_empty() {
            j += 1;
            continue;
        }
        if let Some(m) = matcher {
            if m.match_line(line).is_some() {
                return true;
            }
        } else if inserted_block_starts_with_c_like_function_definition(line) {
            return true;
        }
        j += 1;
    }
    false
}

fn inserted_block_starts_with_c_like_function_definition(line: &str) -> bool {
    let t = line.trim_start();
    let Some(open_paren) = t.find('(') else {
        return false;
    };
    let head = &t[..open_paren];
    let tokens: Vec<&str> = head.split_whitespace().collect();
    if tokens.len() < 2 {
        // `printf(...)`, `return (`, etc. — not `return_type name(`.
        return false;
    }
    let nameish = tokens.last().copied().unwrap_or("");
    let name = nameish.trim_end_matches(['*', '&']);
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let type_or_modifier = |tok: &str| {
        matches!(
            tok,
            "static"
                | "extern"
                | "inline"
                | "void"
                | "int"
                | "char"
                | "short"
                | "long"
                | "float"
                | "double"
                | "unsigned"
                | "signed"
                | "struct"
                | "enum"
                | "union"
                | "const"
                | "volatile"
                | "typedef"
        )
    };
    tokens[..tokens.len() - 1]
        .iter()
        .any(|tok| type_or_modifier(tok))
}

fn hunk_old_change_end_exclusive(ops: &[similar::DiffOp]) -> usize {
    use similar::DiffOp;
    let mut max_o = 0usize;
    for op in ops {
        match *op {
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                max_o = max_o.max(old_index + old_len);
            }
            DiffOp::Replace {
                old_index, old_len, ..
            } => {
                max_o = max_o.max(old_index + old_len);
            }
            DiffOp::Insert { old_index, .. } => {
                // Pure insertions do not consume old lines; Git's post-context anchor is the
                // insertion point (`old_index`), not 0 (t4051 `extended`).
                max_o = max_o.max(old_index);
            }
            DiffOp::Equal { .. } => {}
        }
    }
    max_o
}

fn func_context_old_anchor(ops: &[similar::DiffOp], n_old: usize) -> usize {
    use similar::DiffOp;
    let mut has_delete_or_replace = false;
    let mut min_del = usize::MAX;
    let mut min_ins_old = usize::MAX;

    for op in ops {
        match *op {
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                has_delete_or_replace = true;
                min_del = min_del.min(old_index);
                min_del = min_del.min(old_index + old_len.saturating_sub(1));
            }
            DiffOp::Replace {
                old_index, old_len, ..
            } => {
                has_delete_or_replace = true;
                min_del = min_del.min(old_index);
                min_del = min_del.min(old_index + old_len.saturating_sub(1));
            }
            DiffOp::Insert { old_index, .. } => {
                min_ins_old = min_ins_old.min(old_index);
            }
            DiffOp::Equal { .. } => {}
        }
    }

    let mut i1 = if has_delete_or_replace {
        min_del
    } else if min_ins_old != usize::MAX {
        min_ins_old
    } else {
        0
    };

    let pure_insert = ops
        .iter()
        .all(|op| matches!(op, DiffOp::Insert { .. } | DiffOp::Equal { .. }))
        && ops.iter().any(|op| matches!(op, DiffOp::Insert { .. }));

    if pure_insert && i1 >= n_old && n_old > 0 {
        i1 = n_old - 1;
    }

    i1.min(n_old.saturating_sub(1))
}

fn expand_func_pre_start(
    s1: usize,
    i1: usize,
    n_old: usize,
    old_lines: &[&str],
    matcher: Option<&FuncnameMatcher>,
) -> usize {
    if n_old == 0 {
        return s1;
    }
    let i1 = i1.min(n_old.saturating_sub(1));
    let mut fs1 = get_func_line_backward(old_lines, i1, matcher).unwrap_or(i1);
    while fs1 > 0
        && !is_line_empty_for_func_context(old_lines[fs1 - 1])
        && !is_func_line(old_lines[fs1 - 1], matcher)
    {
        fs1 -= 1;
    }
    s1.min(fs1)
}

fn expand_func_post_end(
    e1: usize,
    i1_end: usize,
    n_old: usize,
    old_lines: &[&str],
    matcher: Option<&FuncnameMatcher>,
) -> usize {
    let from = i1_end.min(n_old);
    let fe1 = get_func_line_forward(old_lines, from, matcher).unwrap_or(n_old);
    let mut fe1_adj = fe1;
    while fe1_adj > 0 && is_line_empty_for_func_context(old_lines[fe1_adj - 1]) {
        fe1_adj -= 1;
    }
    e1.max(fe1_adj).min(n_old)
}

fn is_line_empty_for_func_context(line: &str) -> bool {
    line.chars().all(|c| c.is_whitespace())
}

pub(crate) fn is_func_line(line: &str, matcher: Option<&FuncnameMatcher>) -> bool {
    if let Some(m) = matcher {
        return m.match_line(line).is_some();
    }
    let t = line.trim_end_matches(['\n', '\r']);
    if t.is_empty() {
        return false;
    }
    let b = t.as_bytes()[0];
    if !(b.is_ascii_alphabetic() || b == b'_' || b == b'$') {
        return false;
    }
    true
}

fn get_func_line_backward(
    old_lines: &[&str],
    start: usize,
    matcher: Option<&FuncnameMatcher>,
) -> Option<usize> {
    let mut l = start.min(old_lines.len().saturating_sub(1));
    if old_lines.is_empty() {
        return None;
    }
    loop {
        if is_func_line(old_lines[l], matcher) {
            return Some(l);
        }
        if l == 0 {
            break;
        }
        l -= 1;
    }
    None
}

fn get_func_line_forward(
    old_lines: &[&str],
    start: usize,
    matcher: Option<&FuncnameMatcher>,
) -> Option<usize> {
    let mut l = start;
    while l < old_lines.len() {
        if is_func_line(old_lines[l], matcher) {
            return Some(l);
        }
        l += 1;
    }
    None
}
#[expect(clippy::too_many_arguments)]
/// Compute a unified diff with anchored lines.
///
/// Anchored lines that appear exactly once in both old and new content are
/// forced to match, splitting the diff into segments around those anchor points.
/// This produces diffs where the anchored text stays as context and surrounding
/// lines are shown as additions/removals.
///
/// Segment diffs use `algorithm`. When `use_git_histogram` is true, histogram uses the
/// Git-compatible histogram engine; otherwise `algorithm` is passed to `similar`.
pub fn anchored_unified_diff(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    anchors: &[String],
    algorithm: similar::Algorithm,
    use_git_histogram: bool,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;
    use similar::TextDiff;

    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    // Find anchored lines that appear exactly once in both old and new
    let mut anchor_pairs: Vec<(usize, usize)> = Vec::new(); // (old_idx, new_idx)

    for anchor in anchors {
        let anchor_str = anchor.as_str();

        // Count occurrences in old
        let old_positions: Vec<usize> = old_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim_end() == anchor_str)
            .map(|(i, _)| i)
            .collect();

        // Count occurrences in new
        let new_positions: Vec<usize> = new_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim_end() == anchor_str)
            .map(|(i, _)| i)
            .collect();

        // Only anchor if unique in both
        if old_positions.len() == 1 && new_positions.len() == 1 {
            anchor_pairs.push((old_positions[0], new_positions[0]));
        }
    }

    // If no valid anchors, fall back to normal diff
    if anchor_pairs.is_empty() {
        return unified_diff_with_prefix_and_funcname_and_algorithm(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            0,
            "a/",
            "b/",
            None,
            algorithm,
            false,
            use_git_histogram,
            indent_heuristic,
            quote_path_fully,
        );
    }

    // Sort anchor pairs by their position in the old file
    anchor_pairs.sort_by_key(|&(old_idx, _)| old_idx);

    // Filter to only keep pairs where new positions are also increasing
    // (longest increasing subsequence of new positions)
    let mut filtered: Vec<(usize, usize)> = Vec::new();
    for &pair in &anchor_pairs {
        if filtered.is_empty() || filtered.last().is_some_and(|last| pair.1 > last.1) {
            filtered.push(pair);
        }
    }
    let anchor_pairs = filtered;

    // Build a modified version of old/new where we diff segments between anchors.
    // We'll construct the diff by processing segments:
    // - Before first anchor
    // - Between consecutive anchors
    // - After last anchor
    // Each anchor line itself is a fixed context match.

    // Collect all diff operations
    struct LineDiffOp {
        tag: char, // ' ', '+', '-'
        line: String,
    }

    let append_segment_diff =
        |ops: &mut Vec<LineDiffOp>, old_seg_input: &str, new_seg_input: &str| {
            use similar::ChangeTag;
            let old_ls: Vec<&str> = old_seg_input.lines().collect();
            let new_ls: Vec<&str> = new_seg_input.lines().collect();
            if old_ls.is_empty() && new_ls.is_empty() {
                return;
            }
            let seg_diff = TextDiff::configure()
                .algorithm(algorithm)
                .diff_slices(&old_ls, &new_ls);
            let raw = seg_diff.ops().to_vec();
            let compacted = diff_indent_heuristic::apply_change_compact_to_ops(
                &raw,
                &old_ls,
                &new_ls,
                indent_heuristic,
            );
            for op in &compacted {
                for ch in op.iter_changes(&old_ls, &new_ls) {
                    let t = match ch.tag() {
                        ChangeTag::Equal => ' ',
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                    };
                    ops.push(LineDiffOp {
                        tag: t,
                        line: ch.value().to_string(),
                    });
                }
            }
        };

    let mut ops: Vec<LineDiffOp> = Vec::new();
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;

    for &(old_anchor, new_anchor) in &anchor_pairs {
        // Diff the segment before this anchor
        let old_segment: Vec<&str> = old_lines[old_pos..old_anchor].to_vec();
        let new_segment: Vec<&str> = new_lines[new_pos..new_anchor].to_vec();

        let old_seg_text = old_segment.join("\n");
        let new_seg_text = new_segment.join("\n");

        if !old_seg_text.is_empty() || !new_seg_text.is_empty() {
            let old_seg_input = if old_seg_text.is_empty() {
                String::new()
            } else {
                format!("{}\n", old_seg_text)
            };
            let new_seg_input = if new_seg_text.is_empty() {
                String::new()
            } else {
                format!("{}\n", new_seg_text)
            };
            append_segment_diff(&mut ops, &old_seg_input, &new_seg_input);
        }

        // The anchor line itself is always context
        ops.push(LineDiffOp {
            tag: ' ',
            line: old_lines[old_anchor].to_string(),
        });

        old_pos = old_anchor + 1;
        new_pos = new_anchor + 1;
    }

    // Diff the remaining segment after the last anchor
    let old_segment: Vec<&str> = old_lines[old_pos..].to_vec();
    let new_segment: Vec<&str> = new_lines[new_pos..].to_vec();
    let old_seg_text = old_segment.join("\n");
    let new_seg_text = new_segment.join("\n");

    if !old_seg_text.is_empty() || !new_seg_text.is_empty() {
        let old_seg_input = if old_seg_text.is_empty() {
            String::new()
        } else {
            format!("{}\n", old_seg_text)
        };
        let new_seg_input = if new_seg_text.is_empty() {
            String::new()
        } else {
            format!("{}\n", new_seg_text)
        };
        append_segment_diff(&mut ops, &old_seg_input, &new_seg_input);
    }

    // Now format as unified diff with hunks
    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            "a/",
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            "b/",
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    // Group ops into hunks with context
    let total_ops = ops.len();
    if total_ops == 0 {
        return output;
    }

    // Find ranges of changes
    let mut hunks: Vec<(usize, usize)> = Vec::new(); // (start, end) indices into ops
    let mut i = 0;
    while i < total_ops {
        if ops[i].tag != ' ' {
            let start = i.saturating_sub(context_lines);
            let mut end = i;
            // Extend to include consecutive changes and their context
            while end < total_ops {
                if ops[end].tag != ' ' {
                    end += 1;
                    continue;
                }
                // Check if there's another change within context_lines
                let mut next_change = end;
                while next_change < total_ops && ops[next_change].tag == ' ' {
                    next_change += 1;
                }
                if next_change < total_ops && next_change - end <= context_lines * 2 {
                    end = next_change + 1;
                } else {
                    end = (end + context_lines).min(total_ops);
                    break;
                }
            }
            // Merge with previous hunk if overlapping
            if let Some(last) = hunks.last_mut() {
                if start <= last.1 {
                    last.1 = end;
                } else {
                    hunks.push((start, end));
                }
            } else {
                hunks.push((start, end));
            }
            i = end;
        } else {
            i += 1;
        }
    }

    // Output each hunk
    for (start, end) in hunks {
        // Count old/new lines in this hunk
        let mut old_start = 1usize;
        let mut new_start = 1usize;
        // Calculate line numbers by counting ops before this hunk
        for op in &ops[..start] {
            match op.tag {
                ' ' => {
                    old_start += 1;
                    new_start += 1;
                }
                '-' => {
                    old_start += 1;
                }
                '+' => {
                    new_start += 1;
                }
                _ => {}
            }
        }
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for op in &ops[start..end] {
            match op.tag {
                ' ' => {
                    old_count += 1;
                    new_count += 1;
                }
                '-' => {
                    old_count += 1;
                }
                '+' => {
                    new_count += 1;
                }
                _ => {}
            }
        }

        output.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start, old_count, new_start, new_count
        ));
        for op in &ops[start..end] {
            output.push(op.tag);
            output.push_str(&op.line);
            output.push('\n');
        }
    }

    output
}

/// Extract function context for a hunk header.
///
/// Given a hunk header like `@@ -8,7 +8,7 @@`, find the last line
/// before line 8 in the old content that looks like a function header
/// (starts with a non-whitespace character, like Git's default).
fn func_context_line_matches(line: &str, funcname_matcher: Option<&FuncnameMatcher>) -> bool {
    if let Some(m) = funcname_matcher {
        return m.match_line(line).is_some();
    }
    is_func_line(line, None)
}

fn extract_function_context(
    header: &str,
    old_lines: &[&str],
    funcname_matcher: Option<&FuncnameMatcher>,
    _source_path: Option<&str>,
) -> Option<String> {
    // Parse the old start line number from "@@ -<start>,<count> ..."
    let at_pos = header.find("-")?;
    let rest = &header[at_pos + 1..];
    let comma_or_space = rest.find([',', ' '])?;
    let start_str = &rest[..comma_or_space];
    let start_line: usize = start_str.parse().ok()?;

    // Parse the old line count; "@@ -<start>,<count> ..." (no comma means count 1).
    // Only look for the comma inside the old-range token itself — searching the
    // whole remainder would pick up the comma from the new side (e.g. "+0,0").
    let old_token_end = rest.find([' ', '\t']).unwrap_or(rest.len());
    let old_token = &rest[..old_token_end];
    let old_count: usize = if let Some(comma) = old_token.find(',') {
        old_token[comma + 1..].parse().unwrap_or(1)
    } else {
        1
    };

    if start_line == 0 {
        return None;
    }

    // Look backwards for a line that matches the funcname pattern. start_line is
    // 1-indexed. For a normal hunk the first changed pre-image line is
    // old_lines[start_line-1], so we search lines strictly before it
    // (old_lines[0..start_line-1]). For a pure insertion (old count 0) the
    // content is inserted *after* old line start_line, so Git's function search
    // begins at that line itself: search old_lines[0..start_line].
    if start_line == 0 {
        return None;
    }
    let search_end = if old_count == 0 {
        start_line.min(old_lines.len())
    } else {
        if start_line <= 1 {
            return None;
        }
        (start_line - 1).min(old_lines.len())
    };
    let truncate = |line: &str| {
        let mut end = line.len().min(80);
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = line[..end].to_owned();
        let tail = line[end..].trim_start();
        // Git drops trailing whitespace on truncated funcname lines when the
        // remainder of the source line continues with normal text (t2021-style
        // shell tests), but keeps it when the next token is a string delimiter
        // (gettext `.po` msgstr lines).
        if tail.is_empty()
            || (!tail.starts_with('"')
                && !tail.starts_with('\'')
                && !line.starts_with("msgstr")
                && !line.starts_with("msgid"))
        {
            out = out.trim_end().to_owned();
        }
        out
    };

    for i in (0..search_end).rev() {
        let line = old_lines[i];
        if line.is_empty() {
            continue;
        }
        if func_context_line_matches(line, funcname_matcher) {
            if let Some(matcher) = funcname_matcher {
                if let Some(matched) = matcher.match_line(line) {
                    return Some(truncate(&matched));
                }
                continue;
            }
            return Some(truncate(line));
        }
    }
    None
}

fn last_line_index_without_trailing_newline(text: &str) -> Option<usize> {
    LineSide::from_text(text).incomplete_last_line_index()
}

fn parse_unified_hunk_starts(header: &str) -> Option<(usize, usize)> {
    let rest = header.strip_prefix("@@ ")?;
    let (old_part, new_part) = rest.split_once(" +")?;
    let old_start = parse_unified_range_start(old_part.strip_prefix('-')?)?;
    let new_start = parse_unified_range_start(new_part.split(" @@").next()?)?;
    Some((old_start, new_start))
}

fn parse_unified_range_start(spec: &str) -> Option<usize> {
    let spec = spec.trim();
    if let Some((start, _)) = spec.split_once(',') {
        start.parse().ok()
    } else {
        spec.parse().ok()
    }
}

/// Tracks hunk line numbers while streaming `\ No newline at end of file` markers.
struct UnifiedDiffNoNewlineAnnotator {
    old_last: Option<usize>,
    new_last: Option<usize>,
    old_line: usize,
    new_line: usize,
    passthrough: bool,
}

impl UnifiedDiffNoNewlineAnnotator {
    fn new(old: &str, new: &str) -> Self {
        let old_last = last_line_index_without_trailing_newline(old);
        let new_last = last_line_index_without_trailing_newline(new);
        Self {
            old_last,
            new_last,
            old_line: 1,
            new_line: 1,
            passthrough: old_last.is_none() && new_last.is_none(),
        }
    }

    fn write_hunk<W: std::io::Write>(&mut self, out: &mut W, patch: &str) -> Result<()> {
        if self.passthrough {
            out.write_all(patch.as_bytes())?;
            return Ok(());
        }
        let mut lines = patch.lines().peekable();
        while let Some(line) = lines.next() {
            self.write_annotated_line(out, line, &mut lines)?;
        }
        Ok(())
    }

    fn write_annotated_line<W: std::io::Write>(
        &mut self,
        out: &mut W,
        line: &str,
        lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
    ) -> Result<()> {
        if let Some((o, n)) = line
            .strip_prefix("@@ ")
            .and_then(|_| parse_unified_hunk_starts(line))
        {
            self.old_line = o;
            self.new_line = n;
            writeln!(out, "{line}")?;
            return Ok(());
        }
        if line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("diff --git ")
            || line.starts_with("diff --cc ")
            || line.starts_with("diff --combined ")
        {
            writeln!(out, "{line}")?;
            return Ok(());
        }
        if line.starts_with('-') {
            writeln!(out, "{line}")?;
            let already_marked = lines
                .peek()
                .is_some_and(|next| *next == "\\ No newline at end of file");
            if !already_marked && self.old_last == Some(self.old_line.saturating_sub(1)) {
                writeln!(out, "\\ No newline at end of file")?;
            }
            self.old_line += 1;
            return Ok(());
        }
        if line.starts_with('+') {
            writeln!(out, "{line}")?;
            let already_marked = lines
                .peek()
                .is_some_and(|next| *next == "\\ No newline at end of file");
            if !already_marked && self.new_last == Some(self.new_line.saturating_sub(1)) {
                writeln!(out, "\\ No newline at end of file")?;
            }
            self.new_line += 1;
            return Ok(());
        }
        if line.starts_with(' ') {
            writeln!(out, "{line}")?;
            let already_marked = lines
                .peek()
                .is_some_and(|next| *next == "\\ No newline at end of file");
            if !already_marked {
                let old_idx = self.old_line.saturating_sub(1);
                let new_idx = self.new_line.saturating_sub(1);
                if self.old_last == Some(old_idx) || self.new_last == Some(new_idx) {
                    writeln!(out, "\\ No newline at end of file")?;
                }
            }
            self.old_line += 1;
            self.new_line += 1;
            return Ok(());
        }
        writeln!(out, "{line}")?;
        Ok(())
    }
}

/// Insert Git `\ No newline at end of file` after `-`/`+` hunk lines when that side of the
/// file lacks a trailing newline on the last line.
#[must_use]
pub fn annotate_unified_diff_no_newline_at_eof(patch: &str, old: &str, new: &str) -> String {
    let mut annotator = UnifiedDiffNoNewlineAnnotator::new(old, new);
    if annotator.passthrough {
        return patch.to_string();
    }
    let mut out = Vec::with_capacity(patch.len() + 64);
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if annotator
            .write_annotated_line(&mut out, line, &mut lines)
            .is_err()
        {
            return patch.to_string();
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| patch.to_string())
}

/// Normalise one line like Git's `-b` / `--ignore-space-change`.
#[must_use]
pub fn normalize_ignore_space_change_line(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut in_space = false;
    for c in line.chars() {
        if c.is_whitespace() {
            if !in_space {
                result.push(' ');
                in_space = true;
            }
        } else {
            result.push(c);
            in_space = false;
        }
    }
    while result.ends_with(' ') {
        result.pop();
    }
    result
}

/// Normalise text like Git's `-b` / `--ignore-space-change`: on each line, collapse runs of
/// whitespace to a single ASCII space and trim trailing spaces.
///
/// Line breaks are preserved by splitting on [`str::lines`] and rejoining with `\n` (same approach
/// as the porcelain `diff` whitespace handling in `grit`).
#[must_use]
pub fn normalize_ignore_space_change(content: &str) -> String {
    content
        .lines()
        .map(normalize_ignore_space_change_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Count insertions and deletions using the given line-diff algorithm.
///
/// Uses the same compacted line alignment as unified patches and [`crate::diff::line_change_counts`].
#[must_use]
pub fn count_changes_with_algorithm(
    old_content: &str,
    new_content: &str,
    algorithm: similar::Algorithm,
    use_git_histogram: bool,
) -> (usize, usize) {
    use crate::diff::options::{DiffAlgorithm, DiffOptions};
    let diff_algorithm = if use_git_histogram {
        DiffAlgorithm::Histogram
    } else {
        match algorithm {
            similar::Algorithm::Myers => DiffAlgorithm::Myers,
            similar::Algorithm::Lcs => DiffAlgorithm::Minimal,
            similar::Algorithm::Patience => DiffAlgorithm::Patience,
            _ => DiffAlgorithm::Myers,
        }
    };
    count_line_changes_with_options(
        old_content,
        new_content,
        &DiffOptions {
            algorithm: diff_algorithm,
            ..DiffOptions::default()
        },
    )
}

/// Count insertions and deletions using Git's histogram line-diff engine.
#[must_use]
pub fn count_changes_git_histogram(old_content: &str, new_content: &str) -> (usize, usize) {
    count_changes_with_algorithm(old_content, new_content, similar::Algorithm::Myers, true)
}

/// Line count for diffstat/`--numstat`, matching Git's `count_lines()` in `diff.c`.
///
/// Counts newline-terminated lines; a final line without trailing newline still counts as one line.
/// An empty buffer yields `0`.
#[must_use]
pub fn count_git_lines(data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut nl_just_seen = false;
    for &ch in data {
        if ch == b'\n' {
            count += 1;
            nl_just_seen = true;
        } else {
            nl_just_seen = false;
        }
    }
    if !nl_just_seen {
        count += 1;
    }
    count
}

/// Internal maximum diff score used by Git rename/break heuristics (`MAX_SCORE` in `diffcore.h`).
pub const GIT_DIFF_MAX_SCORE: u64 = 60_000;
const DIFF_MAX_SCORE: u64 = GIT_DIFF_MAX_SCORE;
const DIFF_MINIMUM_BREAK_SIZE: usize = 400;
const DIFF_DEFAULT_BREAK_SCORE: u64 = 30_000;
/// Default break threshold (`DEFAULT_BREAK_SCORE` in `diffcore.h`), internal 0–[`GIT_DIFF_MAX_SCORE`] scale.
pub const GIT_DIFF_DEFAULT_BREAK_SCORE: u64 = DIFF_DEFAULT_BREAK_SCORE;
const DIFF_HASHBASE: u32 = 107_927;

#[derive(Clone, Copy, Default)]
struct SpanSlot {
    hashval: u32,
    cnt: u32,
}

struct SpanHashTop {
    alloc_log2: u8,
    free_slots: i32,
    data: Vec<SpanSlot>,
}

impl SpanHashTop {
    fn new(initial_log2: u8) -> Self {
        let cap = 1usize << initial_log2;
        Self {
            alloc_log2: initial_log2,
            free_slots: initial_free(initial_log2),
            data: vec![SpanSlot::default(); cap],
        }
    }

    fn len(&self) -> usize {
        1usize << self.alloc_log2
    }

    fn add_span(&mut self, hashval: u32, cnt: u32) {
        loop {
            let lim = self.len();
            let mut bucket = (hashval as usize) & (lim - 1);
            loop {
                let h = &mut self.data[bucket];
                if h.cnt == 0 {
                    h.hashval = hashval;
                    h.cnt = cnt;
                    self.free_slots -= 1;
                    if self.free_slots < 0 {
                        self.rehash();
                        break;
                    }
                    return;
                }
                if h.hashval == hashval {
                    h.cnt = h.cnt.saturating_add(cnt);
                    return;
                }
                bucket += 1;
                if bucket >= lim {
                    bucket = 0;
                }
            }
        }
    }

    fn rehash(&mut self) {
        let old = std::mem::take(&mut self.data);
        let old_log = self.alloc_log2;
        self.alloc_log2 = old_log.saturating_add(1);
        let new_len = 1usize << self.alloc_log2;
        self.free_slots = initial_free(self.alloc_log2);
        self.data = vec![SpanSlot::default(); new_len];
        let old_sz = 1usize << old_log;
        for o in old.iter().take(old_sz) {
            let o = *o;
            if o.cnt == 0 {
                continue;
            }
            self.add_span_after_rehash(o.hashval, o.cnt);
        }
    }

    fn add_span_after_rehash(&mut self, hashval: u32, cnt: u32) {
        loop {
            let lim = self.len();
            let mut bucket = (hashval as usize) & (lim - 1);
            loop {
                let h = &mut self.data[bucket];
                if h.cnt == 0 {
                    h.hashval = hashval;
                    h.cnt = cnt;
                    self.free_slots -= 1;
                    if self.free_slots < 0 {
                        self.rehash();
                        break;
                    }
                    return;
                }
                if h.hashval == hashval {
                    h.cnt = h.cnt.saturating_add(cnt);
                    return;
                }
                bucket += 1;
                if bucket >= lim {
                    bucket = 0;
                }
            }
        }
    }

    fn sort_by_hashval(&mut self) {
        let sz = self.len();
        self.data[..sz].sort_by(|a, b| {
            if a.cnt == 0 {
                return std::cmp::Ordering::Greater;
            }
            if b.cnt == 0 {
                return std::cmp::Ordering::Less;
            }
            a.hashval.cmp(&b.hashval)
        });
    }
}

fn initial_free(sz_log2: u8) -> i32 {
    let sz = sz_log2 as i32;
    ((1i32 << sz_log2) * (sz - 3) / sz).max(0)
}

fn hash_blob_spans(buf: &[u8], is_text: bool) -> SpanHashTop {
    let mut hash = SpanHashTop::new(9);
    let mut n = 0u32;
    let mut accum1: u32 = 0;
    let mut accum2: u32 = 0;
    let mut i = 0usize;
    while i < buf.len() {
        let c = buf[i] as u32;
        let old_1 = accum1;
        i += 1;

        if is_text && c == b'\r' as u32 && i < buf.len() && buf[i] == b'\n' {
            continue;
        }

        accum1 = accum1.wrapping_shl(7) ^ accum2.wrapping_shr(25);
        accum2 = accum2.wrapping_shl(7) ^ old_1.wrapping_shr(25);
        accum1 = accum1.wrapping_add(c);
        n += 1;
        if n < 64 && c != b'\n' as u32 {
            continue;
        }
        let hashval = (accum1.wrapping_add(accum2.wrapping_mul(0x61))) % DIFF_HASHBASE;
        hash.add_span(hashval, n);
        n = 0;
        accum1 = 0;
        accum2 = 0;
    }
    if n > 0 {
        let hashval = (accum1.wrapping_add(accum2.wrapping_mul(0x61))) % DIFF_HASHBASE;
        hash.add_span(hashval, n);
    }
    hash.sort_by_hashval();
    hash
}

/// Approximate copied vs added material between two blobs (Git `diffcore_count_changes`).
///
/// Returns `(copied_bytes_from_src, literal_added_bytes_in_dst)` matching Git's
/// `diffcore_count_changes` semantics (used for `--dirstat=changes` damage).
#[must_use]
pub fn diffcore_count_changes(old: &[u8], new: &[u8]) -> (u64, u64) {
    crate::trace::trace_span!("grit.diff.blob");
    let src_is_text = !crate::merge_file::is_binary(old);
    let dst_is_text = !crate::merge_file::is_binary(new);
    let src_count = hash_blob_spans(old, src_is_text);
    let dst_count = hash_blob_spans(new, dst_is_text);
    let mut sc: u64 = 0;
    let mut la: u64 = 0;
    let mut si = 0usize;
    let mut di = 0usize;
    let src_len = src_count.len();
    let dst_len = dst_count.len();
    loop {
        if si >= src_len || src_count.data[si].cnt == 0 {
            break;
        }
        let s_hash = src_count.data[si].hashval;
        let s_cnt = u64::from(src_count.data[si].cnt);
        while di < dst_len && dst_count.data[di].cnt != 0 && dst_count.data[di].hashval < s_hash {
            la += u64::from(dst_count.data[di].cnt);
            di += 1;
        }
        let mut dst_cnt = 0u64;
        if di < dst_len && dst_count.data[di].cnt != 0 && dst_count.data[di].hashval == s_hash {
            dst_cnt = u64::from(dst_count.data[di].cnt);
            di += 1;
        }
        if s_cnt < dst_cnt {
            la += dst_cnt - s_cnt;
            sc += s_cnt;
        } else {
            sc += dst_cnt;
        }
        si += 1;
    }
    while di < dst_len && dst_count.data[di].cnt != 0 {
        la += u64::from(dst_count.data[di].cnt);
        di += 1;
    }
    (sc, la)
}

/// Whether an in-place blob edit should be split into delete+create for rename/copy (`should_break`
/// in `diffcore-break.c`). `break_score` is on the internal 0–[`GIT_DIFF_MAX_SCORE`] scale (default
/// `DIFF_DEFAULT_BREAK_SCORE`).
#[must_use]
pub fn should_break_rewrite_pair(old: &[u8], new: &[u8], break_score: u64) -> bool {
    should_break_rewrite_inner(old, new, break_score)
}

/// Drop entries that do not match `filter` (Git `--diff-filter`, after rename detection).
#[must_use]
pub fn apply_diff_filter(
    entries: Vec<DiffEntry>,
    filter: Option<super::filter::DiffFilter>,
) -> Vec<DiffEntry> {
    match filter {
        Some(f) => f.filter_entries(entries),
        None => entries,
    }
}

/// Parse `-M` / `--find-renames` threshold into Git's 0–100 similarity percent.
#[must_use]
pub fn parse_diff_rename_threshold_percent(arg: &str) -> Option<u8> {
    let token = if arg.is_empty() { "50" } else { arg };
    let internal = parse_diff_rename_score_token(token)?;
    Some((internal.saturating_mul(100) / GIT_DIFF_MAX_SCORE).min(100) as u8)
}

/// Parse a single Git `parse_rename_score` token (`50`, `50%`, decimal forms) into internal
/// 0–[`GIT_DIFF_MAX_SCORE`] units.
pub fn parse_diff_rename_score_token(arg: &str) -> Option<u64> {
    let mut num: u64 = 0;
    let mut scale: u64 = 1;
    let mut dot = false;
    let mut saw_digit = false;
    for ch in arg.chars() {
        if !dot && ch == '.' {
            scale = 1;
            dot = true;
            continue;
        }
        if ch == '%' {
            scale = if dot { scale.saturating_mul(100) } else { 100 };
            break;
        }
        if ch.is_ascii_digit() {
            saw_digit = true;
            if scale < 100_000 {
                scale = scale.saturating_mul(10);
                num = num.saturating_mul(10) + u64::from(ch as u8 - b'0');
            }
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    Some(if num >= scale {
        GIT_DIFF_MAX_SCORE
    } else {
        GIT_DIFF_MAX_SCORE * num / scale
    })
}

/// Git `merge_score` from `diffcore-break.c` when a pair is considered broken: how much of the
/// source blob was removed (0–`DIFF_MAX_SCORE` scale). Used for `dissimilarity index` metadata.
#[must_use]
pub fn rewrite_merge_score(old: &[u8], new: &[u8]) -> Option<u64> {
    if old.is_empty() {
        return None;
    }
    let max_size = old.len().max(new.len());
    if max_size < DIFF_MINIMUM_BREAK_SIZE {
        return None;
    }
    let (src_copied, _) = diffcore_count_changes(old, new);
    let src_copied = src_copied.min(old.len() as u64);
    let src_removed = (old.len() as u64).saturating_sub(src_copied);
    Some(src_removed * DIFF_MAX_SCORE / old.len() as u64)
}

/// Percentage shown in `dissimilarity index N%` for a rewrite (`similarity_index` in Git's diff.c).
#[must_use]
pub fn rewrite_dissimilarity_index_percent(old: &[u8], new: &[u8]) -> Option<u32> {
    let score = rewrite_merge_score(old, new)?;
    Some((score * 100 / DIFF_MAX_SCORE).min(100) as u32)
}

fn should_break_rewrite_inner(src: &[u8], dst: &[u8], break_score: u64) -> bool {
    if src.is_empty() {
        return false;
    }
    let max_size = src.len().max(dst.len());
    if max_size < DIFF_MINIMUM_BREAK_SIZE {
        return false;
    }
    let (src_copied, literal_added) = diffcore_count_changes(src, dst);
    let src_copied = src_copied.min(src.len() as u64);
    let mut literal_added = literal_added;
    let dst_len = dst.len() as u64;
    if src_copied < dst_len && literal_added + src_copied > dst_len {
        literal_added = dst_len.saturating_sub(src_copied);
    }
    let src_removed = (src.len() as u64).saturating_sub(src_copied);
    let merge_score = src_removed * DIFF_MAX_SCORE / src.len() as u64;
    if merge_score > break_score {
        return true;
    }
    let delta_size = src_removed.saturating_add(literal_added);
    if delta_size * DIFF_MAX_SCORE / (max_size as u64) < break_score {
        return false;
    }
    let s = src.len() as u64;
    if (s * break_score < src_removed * DIFF_MAX_SCORE)
        && (literal_added * 20 < src_removed)
        && (literal_added * 20 < src_copied)
    {
        return false;
    }
    true
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Flatten a tree object recursively into a sorted list of (path, mode, oid).
struct FlatEntry {
    path: String,
    mode: u32,
    oid: ObjectId,
}

fn flatten_tree(odb: &Odb, tree_oid: &ObjectId, prefix: &str) -> Result<Vec<FlatEntry>> {
    flatten_tree_with_index(odb, tree_oid, prefix, None)
}

fn sparse_index_skips_tree_descent(index: &Index, dir_path: &str, tree_oid: &ObjectId) -> bool {
    let mut storage = crate::repo_relpath::storage_bytes(dir_path).into_owned();
    if !storage.ends_with(b"/") {
        storage.push(b'/');
    }
    index.find_entry_pos(&storage, 0).is_some_and(|pos| {
        let entry = &index.entries[pos];
        entry.is_sparse_directory_placeholder() && entry.oid == *tree_oid
    })
}

/// Tree OID at `path` (slash-separated, no trailing slash) under `root`, if every component is a tree.
fn subtree_oid_at_path(odb: &Odb, root: &ObjectId, path: &str) -> Result<Option<ObjectId>> {
    let mut oid = *root;
    for component in path.split('/').filter(|c| !c.is_empty()) {
        let entries = read_tree(odb, &oid)?;
        let Some(entry) = entries
            .iter()
            .find(|e| e.name.as_slice() == component.as_bytes())
        else {
            return Ok(None);
        };
        if !is_tree_mode(entry.mode) {
            return Ok(None);
        }
        oid = entry.oid;
    }
    Ok(Some(oid))
}

/// Report every blob in an index subtree as staged additions (HEAD has no tree at this path).
fn emit_flattened_subtree_as_added(
    odb: &Odb,
    subtree: &ObjectId,
    prefix: &str,
    ignore_submodules: bool,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    for entry in flatten_tree(odb, subtree, prefix)? {
        if ignore_submodules && entry.mode == 0o160000 {
            continue;
        }
        result.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(entry.path),
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::new(entry.mode),
            old_oid: zero_oid(),
            new_oid: entry.oid,
            score: None,
        });
    }
    Ok(())
}

/// Compare two flattened subtrees and append staged diff entries.
fn diff_flattened_subtrees(
    head: &[FlatEntry],
    index: &[FlatEntry],
    ignore_submodules: bool,
    result: &mut Vec<DiffEntry>,
    stage0_paths: &mut std::collections::BTreeSet<String>,
    tree_map: &mut std::collections::BTreeMap<&str, &FlatEntry>,
) {
    let mut head_map: std::collections::BTreeMap<&str, &FlatEntry> = head
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    for entry in index {
        stage0_paths.insert(entry.path.clone());
        tree_map.remove(entry.path.as_str());
        match head_map.remove(entry.path.as_str()) {
            Some(te) => {
                if te.oid != entry.oid || te.mode != entry.mode {
                    result.push(DiffEntry {
                        status: DiffStatus::for_content_change(te.mode, entry.mode),
                        old_path: Some(entry.path.clone()),
                        new_path: Some(entry.path.clone()),
                        old_mode: FileMode::new(te.mode),
                        new_mode: FileMode::new(entry.mode),
                        old_oid: te.oid,
                        new_oid: entry.oid,
                        score: None,
                    });
                }
            }
            None => {
                result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(entry.path.clone()),
                    old_mode: FileMode::ABSENT,
                    new_mode: FileMode::new(entry.mode),
                    old_oid: zero_oid(),
                    new_oid: entry.oid,
                    score: None,
                });
            }
        }
    }
    for (path, te) in head_map {
        if ignore_submodules && te.mode == 0o160000 {
            continue;
        }
        tree_map.remove(path);
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path.to_owned()),
            new_path: None,
            old_mode: FileMode::new(te.mode),
            new_mode: FileMode::ABSENT,
            old_oid: te.oid,
            new_oid: zero_oid(),
            score: None,
        });
    }
}

fn flatten_tree_with_index(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    index: Option<&Index>,
) -> Result<Vec<FlatEntry>> {
    let entries = read_tree(odb, tree_oid)?;
    let mut result = Vec::new();

    for entry in entries {
        let name_str = String::from_utf8_lossy(&entry.name);
        let path = format_path(prefix, &name_str);
        if is_tree_mode(entry.mode) {
            if index.is_some_and(|idx| sparse_index_skips_tree_descent(idx, &path, &entry.oid)) {
                continue;
            }
            let nested = flatten_tree_with_index(odb, &entry.oid, &path, index)?;
            result.extend(nested);
        } else {
            result.push(FlatEntry {
                path,
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }

    Ok(result)
}

/// Paths present in `HEAD`'s tree with mode and blob/commit OID (for status porcelain v2).
pub fn head_path_states(
    odb: &Odb,
    head_tree: Option<&ObjectId>,
) -> Result<std::collections::BTreeMap<String, (u32, ObjectId)>> {
    let mut m = std::collections::BTreeMap::new();
    let Some(t) = head_tree else {
        return Ok(m);
    };
    for fe in flatten_tree(odb, t, "")? {
        m.insert(fe.path, (fe.mode, fe.oid));
    }
    Ok(m)
}

/// Whether a mode represents a tree (directory).
fn is_tree_mode(mode: u32) -> bool {
    crate::index::is_tree_mode(mode)
}

/// Git orders diff lists by raw path bytes, which differs from the stored form of
/// non-UTF-8 paths (see [`crate::repo_relpath::storage_bytes`]).
fn path_order(a: &DiffEntry, b: &DiffEntry) -> std::cmp::Ordering {
    use crate::repo_relpath::storage_bytes;
    storage_bytes(a.path()).cmp(&storage_bytes(b.path()))
}

fn entries_sorted_by_path(entries: &[DiffEntry]) -> bool {
    entries.is_sorted_by(|a, b| path_order(a, b).is_le())
}

fn entries_strictly_sorted_by_path(entries: &[DiffEntry]) -> bool {
    entries.is_sorted_by(|a, b| path_order(a, b).is_lt())
}

/// Stable-sort diff entries by [`DiffEntry::path`] in raw byte order, Git's output order
/// for diff lists (non-UTF-8 paths compare by their decoded bytes).
///
/// Tree diffs and rename detection already return entries in this order, so the common
/// case is a single comparison pass with no allocation (a stable sort would otherwise
/// allocate scratch space proportional to the list).
///
/// # Parameters
///
/// - `entries`: the list to order in place; entries with equal paths keep their order.
pub fn sort_entries_by_path(entries: &mut [DiffEntry]) {
    if entries_sorted_by_path(entries) {
        return;
    }
    entries.sort_by(path_order);
}

/// Build a path with an optional prefix.
fn format_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        let mut path = String::with_capacity(prefix.len() + 1 + name.len());
        path.push_str(prefix);
        path.push('/');
        path.push_str(name);
        path
    }
}

/// Format a numeric mode as a zero-padded octal string.
pub fn format_mode(mode: u32) -> String {
    format!("{mode:06o}")
}

/// Read the HEAD commit OID from a submodule checkout directory.
///
/// Returns `None` if the path is missing, not a submodule checkout, or has no resolvable HEAD.
#[must_use]
pub fn read_submodule_head_for_checkout(sub_dir: &Path) -> Option<ObjectId> {
    read_submodule_head(sub_dir)
}

/// First line of a commit's message for `git diff --submodule=log` output.
///
/// Honors `encoding` in the commit object (Latin-1 vs UTF-8) using the same
/// rules as Git's submodule summary.
#[must_use]
pub fn submodule_commit_subject_line(c: &CommitData) -> String {
    let enc = c.encoding.as_deref().unwrap_or("UTF-8");
    let is_latin1 = enc.eq_ignore_ascii_case("ISO8859-1")
        || enc.eq_ignore_ascii_case("ISO-8859-1")
        || enc.eq_ignore_ascii_case("LATIN1")
        || enc.eq_ignore_ascii_case("ISO-8859-15");
    if let Some(raw) = c.raw_message.as_deref() {
        let line = raw.split(|b| *b == b'\n').next().unwrap_or(raw);
        if is_latin1 {
            return line
                .iter()
                .map(|&b| b as char)
                .collect::<String>()
                .trim()
                .to_owned();
        }
        return String::from_utf8_lossy(line).trim().to_string();
    }
    c.message.lines().next().unwrap_or("").trim().to_owned()
}

/// True when a gitlink directory has no initialized submodule (no embedded `.git`), so the
/// recorded index OID is treated as the worktree reference (Git `ce_compare_gitlink` when
/// `resolve_gitlink_ref` fails).
fn uninitialized_gitlink_worktree_matches_index(sub_dir: &Path) -> bool {
    submodule_embedded_git_dir(sub_dir).is_none()
}

fn read_submodule_head(sub_dir: &Path) -> Option<ObjectId> {
    read_submodule_head_oid(sub_dir)
}

/// Resolve the embedded git directory for a submodule work tree (`sub_dir/.git`).
#[must_use]
pub fn submodule_embedded_git_dir(sub_dir: &Path) -> Option<PathBuf> {
    let gitfile = sub_dir.join(".git");
    if gitfile.is_file() {
        let content = fs::read_to_string(&gitfile).ok()?;
        let gitdir = content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir: "))?
            .trim();
        Some(if Path::new(gitdir).is_absolute() {
            PathBuf::from(gitdir)
        } else {
            sub_dir.join(gitdir)
        })
    } else if gitfile.is_dir() {
        Some(gitfile)
    } else {
        None
    }
}

/// Whether `abs` is a nested Git checkout (embedded `.git`) not already indexed as a gitlink.
///
/// Matches Git's `is_nonbare_repository_dir` / `is_git_directory`: the embedded git dir must look
/// like a real repository (valid `HEAD` plus `objects/`, or `objects/` and `refs/`), not a bare
/// `.git` directory placeholder.
#[must_use]
pub(crate) fn is_unregistered_embedded_git_repo(index: &Index, rel: &str, abs: &Path) -> bool {
    if rel.is_empty() {
        return false;
    }
    if index
        .entries
        .iter()
        .any(|e| e.path.as_slice() == rel.as_bytes() && e.mode == crate::index::MODE_GITLINK)
    {
        return false;
    }
    let Some(git_dir) = submodule_embedded_git_dir(abs) else {
        return false;
    };
    crate::submodule_gitdir::is_valid_git_dir(&git_dir)
}

/// Walk upward from `sub_dir` to find the nearest containing Git work tree.
fn find_superproject_git(sub_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut cur = sub_dir.parent()?;
    loop {
        let git_path = cur.join(".git");
        if git_path.exists() {
            let gd = if git_path.is_file() {
                let content = fs::read_to_string(&git_path).ok()?;
                let line = content
                    .lines()
                    .find_map(|l| l.strip_prefix("gitdir: "))?
                    .trim();
                if Path::new(line).is_absolute() {
                    PathBuf::from(line)
                } else {
                    cur.join(line)
                }
            } else {
                git_path
            };
            return Some((cur.to_path_buf(), gd));
        }
        cur = cur.parent()?;
    }
}

/// Read the HEAD commit OID from a submodule working tree directory.
///
/// Handles both embedded `.git` directories and `gitdir:` gitfiles pointing at
/// `.git/modules/...` (or other locations). Returns `None` if the path is not
/// a checkout or has no resolvable HEAD.
pub fn read_submodule_head_oid(sub_dir: &Path) -> Option<ObjectId> {
    // Submodule `.git` may be a gitfile pointing at `.git/modules/<name>` in another superproject
    // after `cp -R`. Prefer the current superproject's module dir when present.
    let mut git_dir = submodule_embedded_git_dir(sub_dir)?;
    if let Some((super_wt, super_git_dir)) = find_superproject_git(sub_dir) {
        let rel = sub_dir.strip_prefix(&super_wt).ok()?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let local_mod = super_git_dir
            .join("modules")
            .join(rel_str.trim_start_matches('/'));
        if local_mod.join("HEAD").exists() {
            let sg = super_git_dir
                .canonicalize_simplified()
                .unwrap_or(super_git_dir);
            let cur = git_dir
                .canonicalize_simplified()
                .unwrap_or_else(|_| git_dir.clone());
            if !cur.starts_with(&sg) {
                git_dir = local_mod;
            }
        }
    }
    let head_content = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head_trimmed = head_content.trim();
    if head_trimmed.starts_with("ref: ") {
        // Use the full ref resolver so packed-refs and worktrees match Git. If `HEAD` is a stale
        // symref (e.g. still `refs/heads/master` while only `main` exists), fall back like
        // `resolve_gitlink_ref` / `git add` on embedded repos (`t6437-submodule-merge`).
        match crate::refs::resolve_ref(&git_dir, "HEAD") {
            Ok(oid) => Some(oid),
            Err(_) => {
                let mut found = None;
                for branch in ["main", "master"] {
                    let p = git_dir.join("refs/heads").join(branch);
                    if let Ok(s) = fs::read_to_string(&p) {
                        if let Ok(o) = ObjectId::from_hex(s.trim()) {
                            found = Some(o);
                            break;
                        }
                    }
                }
                found
            }
        }
    } else {
        ObjectId::from_hex(head_trimmed).ok()
    }
}

/// True when a populated submodule checkout is *broken*: its `HEAD` resolves to a commit OID, but
/// that commit object cannot be read from the submodule's own object database.
///
/// This mirrors Git's `is_submodule_modified`, which shells out to `git status --porcelain=2`
/// inside the submodule; when the submodule's object store is corrupt (e.g. `rm -r .git/objects`),
/// that inner status fails and Git aborts the surrounding `status`/`diff`/`fetch`. We detect the
/// same condition in-process so the superproject operation can return a fatal error rather than
/// silently treating the submodule as clean (t5526 "fetching submodule into a broken repository").
///
/// Returns `false` when the submodule is not checked out, has no embedded git dir, or has an
/// unresolvable HEAD (those are handled separately by the unpopulated/placeholder logic).
#[must_use]
pub fn submodule_head_object_broken(sub_dir: &Path) -> bool {
    let Some(sub_git_dir) = submodule_embedded_git_dir(sub_dir) else {
        return false;
    };
    let Some(head_oid) = read_submodule_head_oid(sub_dir) else {
        return false;
    };
    let odb = Odb::new(&sub_git_dir.join("objects"));
    match odb.read(&head_oid) {
        // HEAD object present but not a commit would be a different kind of corruption; only the
        // missing-object case is the broken-repo scenario Git guards against here.
        Ok(obj) => parse_commit(&obj.data).is_err(),
        Err(_) => true,
    }
}

/// Submodule dirty bits aligned with Git's `DIRTY_SUBMODULE_*` / porcelain v2 `S???` token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubmodulePorcelainFlags {
    /// Submodule checkout HEAD differs from the gitlink OID recorded in the parent index.
    pub new_commits: bool,
    /// The submodule has its own staged or unstaged changes (`DIRTY_SUBMODULE_MODIFIED`).
    pub modified: bool,
    /// The submodule work tree contains paths not in its index (`DIRTY_SUBMODULE_UNTRACKED`).
    pub untracked: bool,
}

/// Inspect a checked-out submodule at `rel_path` (relative to `super_worktree`) and return
/// flags used for `git status --porcelain=v2` submodule tokens.
///
/// `recorded_oid` is the gitlink OID stored in the **parent** index (stage 0). When the
/// submodule is not checked out or cannot be opened, returns [`Default::default()`].
pub fn submodule_porcelain_flags(
    super_worktree: &Path,
    rel_path: &str,
    recorded_oid: ObjectId,
) -> SubmodulePorcelainFlags {
    let sub_dir = super_worktree.join(rel_path);
    let Some(sub_git_dir) = submodule_embedded_git_dir(&sub_dir) else {
        return SubmodulePorcelainFlags::default();
    };
    let Some(sub_head) = read_submodule_head_oid(&sub_dir) else {
        return SubmodulePorcelainFlags::default();
    };

    let new_commits = sub_head != recorded_oid;

    let index_path = sub_git_dir.join("index");
    let mut sub_index = match crate::index::Index::load(&index_path) {
        Ok(ix) => ix,
        Err(_) => {
            return SubmodulePorcelainFlags {
                new_commits,
                ..Default::default()
            }
        }
    };

    let tracked: std::collections::BTreeSet<String> = sub_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    let untracked = submodule_dir_has_untracked_inner(&sub_dir, &sub_dir, &tracked, &sub_index);

    let objects_dir = sub_git_dir.join("objects");
    let odb = Odb::new(&objects_dir);

    let sub_head_tree = (|| -> Option<ObjectId> {
        let h = fs::read_to_string(sub_git_dir.join("HEAD")).ok()?;
        let h_str = h.trim();
        let commit_oid = if let Some(r) = h_str.strip_prefix("ref: ") {
            let oid_hex = fs::read_to_string(sub_git_dir.join(r)).ok()?;
            ObjectId::from_hex(oid_hex.trim()).ok()?
        } else {
            ObjectId::from_hex(h_str).ok()?
        };
        let obj = odb.read(&commit_oid).ok()?;
        let commit = parse_commit(&obj.data).ok()?;
        Some(commit.tree)
    })();

    let staged_dirty = sub_head_tree
        .as_ref()
        .map(|t| diff_index_to_tree(&odb, &sub_index, Some(t), false).map(|v| !v.is_empty()))
        .unwrap_or(Ok(false));
    let staged_dirty = staged_dirty.unwrap_or(false);

    let unstaged_dirty = diff_index_to_worktree(&odb, &mut sub_index, &sub_dir, false, true)
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    let mut modified = staged_dirty || unstaged_dirty;

    // Nested submodule has its own index: OR `modified` from immediate gitlink children so a
    // dirty nested checkout (e.g. staged `file` under `sub1/sub2`) marks the parent gitlink as
    // modified in the superproject (t7506). Do **not** OR `untracked` — untracked-only inside a
    // nested submodule must stay `S..U` on the parent, not `S.U` / `S.M.`.
    for e in &sub_index.entries {
        if e.stage() != 0 || e.mode != 0o160000 {
            continue;
        }
        let child = String::from_utf8_lossy(&e.path).into_owned();
        let full_rel = if rel_path.is_empty() {
            child
        } else {
            format!("{rel_path}/{child}")
        };
        let nested = submodule_porcelain_flags(super_worktree, &full_rel, e.oid);
        modified |= nested.modified;
    }

    SubmodulePorcelainFlags {
        new_commits,
        modified,
        untracked,
    }
}

fn submodule_dir_has_untracked_inner(
    dir: &Path,
    root: &Path,
    tracked: &std::collections::BTreeSet<String>,
    owning_index: &Index,
) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| name.clone());

        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            let is_gitlink = owning_index
                .get(rel.as_bytes(), 0)
                .is_some_and(|e| e.mode == 0o160000);
            if is_gitlink {
                let Some(nested_git) = submodule_embedded_git_dir(&path) else {
                    continue;
                };
                let nested_index_path = nested_git.join("index");
                let Ok(nested_ix) = crate::index::Index::load(&nested_index_path) else {
                    continue;
                };
                let nested_tracked: std::collections::BTreeSet<String> = nested_ix
                    .entries
                    .iter()
                    .filter(|e| e.stage() == 0)
                    .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                    .collect();
                if submodule_dir_has_untracked_inner(&path, &path, &nested_tracked, &nested_ix) {
                    return true;
                }
            } else if submodule_dir_has_untracked_inner(&path, root, tracked, owning_index) {
                return true;
            }
        } else if !tracked.contains(&rel) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod line_compare_key_tests {
    use super::*;
    use crate::diff::unified_emit::line_diff_token;
    use crate::diff::WhitespaceMode;

    #[test]
    fn borrowed_keys_match_owned_line_tokens() {
        let texts = [
            "",
            "\n",
            "a",
            "a\n",
            "a\nb",
            "a\r\nb\r\n",
            "a\r\nb\r",
            "\n\n\nlast",
            "x\n\r\n\ry\n",
        ];
        for text in texts {
            let side = LineSide::from_text(text);
            let mut normalized = Vec::new();
            let keys = line_compare_keys(text, &side, WhitespaceMode::default(), &mut normalized);
            let expected: Vec<String> = side.lines.iter().map(line_diff_token).collect();
            assert_eq!(keys, expected, "text {text:?}");
            let source = text.as_bytes().as_ptr_range();
            assert!(
                keys.iter()
                    .all(|key| source.contains(&key.as_ptr()) || key.is_empty()),
                "text {text:?}"
            );
        }
    }

    #[test]
    fn whitespace_modes_compare_normalized_bodies() {
        let text = "a  b\nab \n";
        let side = LineSide::from_text(text);
        let ws = WhitespaceMode {
            ignore_all_space: true,
            ..WhitespaceMode::default()
        };
        let mut normalized = Vec::new();
        let keys = line_compare_keys(text, &side, ws, &mut normalized);
        assert_eq!(keys, vec!["ab", "ab"]);
    }

    #[test]
    fn xdiff_marks_only_edited_lines_in_large_file() {
        let old: String = (0..5_000).map(|i| format!("line{i}\n")).collect();
        let new: String = (0..5_000)
            .map(|i| {
                if i % 100 == 0 {
                    format!("LINE{i}\n")
                } else {
                    format!("line{i}\n")
                }
            })
            .collect();
        let body =
            git_xdiff_unified_body_raw(&old, &new, 3, 0, true, false, WhitespaceMode::default());
        assert_eq!(body.matches("\n@@ ").count() + 1, 50);
        assert_eq!(body.lines().filter(|l| l.starts_with('-')).count(), 50);
        assert_eq!(body.lines().filter(|l| l.starts_with('+')).count(), 50);
        assert!(body.starts_with("@@ -1,4 +1,4 @@\n-line0\n+LINE0\n line1\n"));
    }
}

#[cfg(test)]
mod entry_order_tests {
    use super::*;

    fn entry(status: DiffStatus, path: &str, score: Option<u32>) -> DiffEntry {
        let mode = |present: bool| {
            if present {
                FileMode::REGULAR
            } else {
                FileMode::ABSENT
            }
        };
        DiffEntry {
            status,
            old_path: (status != DiffStatus::Added).then(|| path.to_owned()),
            new_path: (status != DiffStatus::Deleted).then(|| path.to_owned()),
            old_mode: mode(status != DiffStatus::Added),
            new_mode: mode(status != DiffStatus::Deleted),
            old_oid: ObjectId::zero(),
            new_oid: ObjectId::zero(),
            score,
        }
    }

    #[test]
    fn sort_entries_by_path_orders_by_path_and_keeps_equal_paths_in_place() {
        let mut entries = vec![
            entry(DiffStatus::Modified, "b", None),
            entry(DiffStatus::Deleted, "a/x", Some(1)),
            entry(DiffStatus::Added, "a/x", Some(2)),
            entry(DiffStatus::Added, "a", None),
        ];
        sort_entries_by_path(&mut entries);
        let order: Vec<_> = entries.iter().map(|e| (e.path(), e.score)).collect();
        assert_eq!(
            order,
            [("a", None), ("a/x", Some(1)), ("a/x", Some(2)), ("b", None)]
        );
        let sorted = entries.clone();
        sort_entries_by_path(&mut entries);
        assert_eq!(entries, sorted);
        sort_entries_by_path(&mut []);
    }

    #[test]
    fn sort_entries_by_path_compares_non_utf8_paths_by_raw_bytes() {
        let raw = crate::repo_relpath::format_path_stored("", b"raw-\xff");
        assert!(raw.starts_with("grit+pathhex:"));
        let mut entries = vec![
            entry(DiffStatus::Modified, &raw, None),
            entry(DiffStatus::Modified, "node/leaf", None),
            entry(DiffStatus::Modified, "zeta", None),
        ];
        sort_entries_by_path(&mut entries);
        let paths: Vec<_> = entries.iter().map(DiffEntry::path).collect();
        assert_eq!(paths, ["node/leaf", raw.as_str(), "zeta"]);
    }

    #[test]
    fn rename_pass_returns_unpairable_lists_in_path_order() {
        let config = ConfigSet::new();
        let policy = ThreadPolicy::default();
        let odb = Odb::new(Path::new("/nonexistent-objects"));
        let run = |entries: Vec<DiffEntry>| {
            detect_renames_with_policy(&odb, None, entries, 50, &config, &policy, None)
                .expect("no blob is read without both adds and deletes")
        };
        let sorted = vec![
            entry(DiffStatus::Deleted, "a", None),
            entry(DiffStatus::Modified, "b", None),
            entry(DiffStatus::Deleted, "c", None),
        ];
        assert_eq!(run(sorted.clone()), sorted);
        let unsorted = vec![
            entry(DiffStatus::Added, "z", None),
            entry(DiffStatus::Modified, "m", None),
            entry(DiffStatus::Added, "a", None),
        ];
        let paths: Vec<_> = run(unsorted).iter().map(|e| e.path().to_owned()).collect();
        assert_eq!(paths, ["a", "m", "z"]);
        // A duplicate path takes the regrouping path: modifications before additions.
        let duplicate = vec![
            entry(DiffStatus::Added, "p", None),
            entry(DiffStatus::Modified, "p", None),
        ];
        let statuses: Vec<_> = run(duplicate).iter().map(|e| e.status).collect();
        assert_eq!(statuses, [DiffStatus::Modified, DiffStatus::Added]);
    }
}

#[cfg(test)]
mod diff_index_to_tree_cache_tree_tests {
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::index::{Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::write_tree::{build_cache_tree_from_index, write_tree_from_index};
    use tempfile::TempDir;

    struct TreeReadCounter {
        counter: Rc<AtomicUsize>,
    }

    impl TreeReadCounter {
        fn start() -> Self {
            let counter = Rc::new(AtomicUsize::new(0));
            set_diff_tree_read_hook_for_tests(Some(counter.clone()));
            Self { counter }
        }

        fn get(&self) -> usize {
            self.counter.load(Ordering::Relaxed)
        }
    }

    impl Drop for TreeReadCounter {
        fn drop(&mut self) {
            set_diff_tree_read_hook_for_tests(None);
        }
    }

    fn diff_index_to_tree_flatten_reference(
        odb: &Odb,
        index: &Index,
        head_tree: &ObjectId,
        ignore_submodules: bool,
    ) -> Result<Vec<DiffEntry>> {
        let mut flat_index = index.clone();
        flat_index.clear_cache_tree();
        diff_index_to_tree(odb, &flat_index, Some(head_tree), ignore_submodules)
    }

    fn index_entry(path: &str, mode: u32, oid: ObjectId) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    fn repo_with_tree(depth: usize, width: usize) -> (TempDir, Odb, ObjectId, Index) {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let mut index = Index::new();
        for d in 0..depth {
            for w in 0..width {
                let path = format!("unchanged/d{d}/f{w}.txt");
                let oid = odb
                    .write(ObjectKind::Blob, format!("body-{d}-{w}").as_bytes())
                    .expect("blob");
                index.add_or_replace(index_entry(&path, MODE_REGULAR, oid));
            }
        }
        let head_tree = write_tree_from_index(&odb, &index, "").expect("write tree");
        let cache = build_cache_tree_from_index(&odb, &index).expect("cache-tree");
        index.set_cache_tree(cache);
        index.cache_tree_root = Some(head_tree);
        (temp, odb, head_tree, index)
    }

    #[test]
    fn diff_index_to_tree_cache_tree_fast_path_matches_slow() {
        let (_temp, odb, head_tree, mut index) = repo_with_tree(4, 5);
        assert!(index.cache_tree.as_ref().is_some_and(|c| c.is_valid()));

        let mut state: u64 = 0xDEAD_BEEF;
        for _ in 0..24 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let d = (state % 4) as usize;
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let w = (state % 5) as usize;
            let path = format!("unchanged/d{d}/f{w}.txt");
            index.invalidate_cache_tree_for_path(path.as_bytes());
            assert!(
                index.cache_tree_root.is_none(),
                "partial invalidation clears cache_tree_root like Index::add"
            );
            assert!(
                index.cache_tree.as_ref().is_some_and(|r| !r.is_valid()),
                "root cache node must be invalid after partial invalidation"
            );
            if let Some(e) = index.get_mut(path.as_bytes(), 0) {
                let new_oid = odb
                    .write(ObjectKind::Blob, format!("new-{d}-{w}").as_bytes())
                    .expect("blob");
                e.oid = new_oid;
            }
            let fast = diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("fast");
            let flat = diff_index_to_tree_flatten_reference(&odb, &index, &head_tree, false)
                .expect("flat");
            assert_eq!(fast, flat, "mismatch after mutating {path}");
        }
    }

    #[test]
    fn diff_index_to_tree_skips_unchanged_subtrees() {
        let (_temp, odb, head_tree, index) = repo_with_tree(5, 6);
        let reads = TreeReadCounter::start();
        diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("cached diff");
        let cached_reads = reads.get();

        let mut no_cache = index.clone();
        no_cache.clear_cache_tree();
        let reads = TreeReadCounter::start();
        diff_index_to_tree(&odb, &no_cache, Some(&head_tree), false).expect("flat diff");
        let flat_reads = reads.get();

        assert!(
            cached_reads < flat_reads,
            "expected fewer tree reads with cache-tree (cached={cached_reads}, flat={flat_reads})"
        );
        assert_eq!(
            cached_reads, 1,
            "unchanged repo should only read the root tree"
        );
    }

    #[test]
    fn diff_index_to_tree_partial_invalidation_skips_sibling_subtree() {
        let (_temp, odb, head_tree, mut index) = repo_with_tree(5, 6);
        let path = "unchanged/d1/f2.txt";
        index.invalidate_cache_tree_for_path(path.as_bytes());
        let new_oid = odb.write(ObjectKind::Blob, b"changed").expect("blob");
        index.get_mut(path.as_bytes(), 0).expect("entry").oid = new_oid;

        let flat =
            diff_index_to_tree_flatten_reference(&odb, &index, &head_tree, false).expect("flat");
        let reads = TreeReadCounter::start();
        let fast = diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("fast");
        let cached_reads = reads.get();
        assert_eq!(fast, flat);

        let reads = TreeReadCounter::start();
        diff_index_to_tree_flatten_reference(&odb, &index, &head_tree, false).expect("flat");
        let flat_reads = reads.get();

        assert!(
            cached_reads < flat_reads,
            "partial cache-tree should skip sibling dirs (cached={cached_reads}, flat={flat_reads})"
        );
    }

    fn cache_leaf(name: &str) -> CacheTreeNode {
        CacheTreeNode::valid(name.as_bytes().to_vec(), 1, ObjectId::zero(), Vec::new())
    }

    #[test]
    fn cache_children_lookup_matches_linear_scan_on_wide_unsorted_nodes() {
        let mut children: Vec<CacheTreeNode> = (0..CACHE_CHILD_LINEAR_SCAN_MAX * 3)
            .rev()
            .map(|i| cache_leaf(&format!("d{i:03}")))
            .collect();
        let mut first_dup = cache_leaf("d007");
        first_dup.entry_count = 7;
        children.insert(3, first_dup);
        let root = CacheTreeNode::valid(Vec::new(), 0, ObjectId::zero(), children);

        let lookup = CacheChildren::new(Some(&root));
        assert!(
            !lookup.sorted.is_empty(),
            "wide nodes must use the sorted lookup"
        );
        for name in ["d000", "d007", "d020", "d047", "zzz", "d", ""] {
            let linear = root.children.iter().find(|c| c.name == name.as_bytes());
            let fast = lookup.get(name.as_bytes());
            assert_eq!(
                fast.map(|c| (&c.name, c.entry_count)),
                linear.map(|c| (&c.name, c.entry_count)),
                "lookup of {name:?}"
            );
        }
        assert_eq!(lookup.get(b"d007").map(|c| c.entry_count), Some(7));

        let narrow = CacheTreeNode::valid(
            Vec::new(),
            0,
            ObjectId::zero(),
            vec![cache_leaf("b"), cache_leaf("a")],
        );
        let lookup = CacheChildren::new(Some(&narrow));
        assert!(lookup.sorted.is_empty());
        assert_eq!(lookup.get(b"a").map(|c| c.name.as_slice()), Some(&b"a"[..]));
        assert!(lookup.get(b"c").is_none());
        assert!(CacheChildren::new(None).get(b"a").is_none());
    }

    #[test]
    fn index_relative_in_dir_requires_a_component_boundary() {
        assert_eq!(index_relative_in_dir(b"a/b", b""), Some(&b"a/b"[..]));
        assert_eq!(index_relative_in_dir(b"a/b", b"a"), Some(&b"b"[..]));
        assert_eq!(index_relative_in_dir(b"a", b"a"), Some(&b""[..]));
        assert_eq!(index_relative_in_dir(b"ab/c", b"a"), None);
        assert_eq!(index_relative_in_dir(b"a.c", b"a"), None);
        assert_eq!(index_relative_in_dir(b"b", b"a"), None);
        assert_eq!(join_index_path(b"", b"x"), b"x".to_vec());
        assert_eq!(join_index_path(b"a/b", b"x"), b"a/b/x".to_vec());
    }

    /// Wide root (more children than the linear-scan cutoff), cache-tree children stored in
    /// reverse order, a few changed paths, plus additions and deletions at both levels.
    #[test]
    fn diff_index_to_tree_wide_unsorted_cache_tree_matches_flatten() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let mut index = Index::new();
        let width = CACHE_CHILD_LINEAR_SCAN_MAX * 4;
        for d in 0..width {
            for f in 0..2 {
                let path = format!("dir{d:03}/sub/f{f}.txt");
                let oid = odb.write(ObjectKind::Blob, path.as_bytes()).expect("blob");
                index.add_or_replace(index_entry(&path, MODE_REGULAR, oid));
            }
        }
        let head_tree = write_tree_from_index(&odb, &index, "").expect("write tree");
        let mut cache = build_cache_tree_from_index(&odb, &index).expect("cache-tree");
        cache.children.reverse();
        index.set_cache_tree(cache);
        index.cache_tree_root = Some(head_tree);

        let changed = [
            "dir003/sub/f1.txt",
            "dir031/sub/f0.txt",
            "dir063/sub/f1.txt",
        ];
        for path in changed {
            index.invalidate_cache_tree_for_path(path.as_bytes());
            let oid = odb.write(ObjectKind::Blob, b"changed").expect("blob");
            index.get_mut(path.as_bytes(), 0).expect("entry").oid = oid;
        }
        for path in ["dir010/sub/new.txt", "dir999/new.txt", "top.txt"] {
            index.invalidate_cache_tree_for_path(path.as_bytes());
            let oid = odb.write(ObjectKind::Blob, path.as_bytes()).expect("blob");
            index.add_or_replace(index_entry(path, MODE_REGULAR, oid));
        }
        for path in [
            "dir020/sub/f0.txt",
            "dir020/sub/f1.txt",
            "dir021/sub/f0.txt",
        ] {
            index.invalidate_cache_tree_for_path(path.as_bytes());
            index.remove(path.as_bytes());
        }

        let flat =
            diff_index_to_tree_flatten_reference(&odb, &index, &head_tree, false).expect("flat");
        let reads = TreeReadCounter::start();
        let fast = diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("fast");
        let cached_reads = reads.get();
        assert_eq!(fast, flat);
        assert_eq!(fast.len(), changed.len() + 3 + 3);
        assert!(
            cached_reads <= 1 + 2 * 6,
            "unchanged siblings must be skipped through the cache-tree (reads={cached_reads})"
        );
    }

    #[test]
    fn diff_index_to_tree_cache_tree_handles_non_utf8_directory_names() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let mut index = Index::new();
        let raw_dirs: [&[u8]; 3] = [b"caf\xe9", b"caf\xef\xbf\xbd", b"plain"];
        for dir in raw_dirs {
            for f in [&b"a.txt"[..], b"b.txt"] {
                let path = [dir, b"/", f].concat();
                let oid = odb.write(ObjectKind::Blob, &path).expect("blob");
                let mut entry = index_entry("x", MODE_REGULAR, oid);
                entry.flags = path.len().min(0xfff) as u16;
                entry.path = path;
                index.add_or_replace(entry);
            }
        }
        let head_tree = write_tree_from_index(&odb, &index, "").expect("write tree");
        let cache = build_cache_tree_from_index(&odb, &index).expect("cache-tree");
        index.set_cache_tree(cache);
        index.cache_tree_root = Some(head_tree);

        let changed: &[u8] = b"caf\xe9/b.txt";
        index.invalidate_cache_tree_for_path(changed);
        let oid = odb.write(ObjectKind::Blob, b"changed").expect("blob");
        index.get_mut(changed, 0).expect("entry").oid = oid;

        // Directory prefixes are compared as raw bytes, so `caf\xe9` and a directory literally
        // named `caf\u{fffd}` stay distinct even though they print the same.
        let fast = diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("fast");
        assert_eq!(fast.len(), 1, "{fast:?}");
        assert_eq!(fast[0].status, DiffStatus::Modified);
        assert_eq!(
            fast[0].old_oid,
            odb.write(ObjectKind::Blob, changed).expect("blob")
        );
        assert_eq!(fast[0].new_oid, oid);
    }

    #[test]
    fn diff_index_to_tree_reports_mode_gitlink_and_intent_to_add() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let blob = odb.write(ObjectKind::Blob, b"x").expect("blob");
        let mut index = Index::new();
        index.add_or_replace(index_entry("tracked.txt", MODE_REGULAR, blob));
        let head_tree = write_tree_from_index(&odb, &index, "").expect("tree");
        let cache = build_cache_tree_from_index(&odb, &index).expect("cache");
        index.set_cache_tree(cache);

        let mode_entry = index_entry("tracked.txt", MODE_EXECUTABLE, blob);
        index.add_or_replace(mode_entry);
        index.invalidate_cache_tree_for_path(b"tracked.txt");

        let diffs = diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("diff");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].status, DiffStatus::Modified);
        assert_eq!(diffs[0].old_mode, FileMode::new(MODE_REGULAR));
        assert_eq!(diffs[0].new_mode, FileMode::new(MODE_EXECUTABLE));

        // Intent-to-add is omitted from staged diff.
        let mut ita = index_entry("new.txt", MODE_REGULAR, blob);
        ita.set_intent_to_add(true);
        index.add_or_replace(ita);
        let diffs = diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("diff");
        assert!(
            diffs.iter().all(|d| d.path() != "new.txt"),
            "intent-to-add must not appear in staged diff"
        );

        // Gitlink with ignore_submodules skips diff entry but still matches tree.
        let gitlink: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
            .parse()
            .expect("oid");
        let mut sub_index = Index::new();
        sub_index.add_or_replace(index_entry("sub", MODE_GITLINK, gitlink));
        let sub_tree = write_tree_from_index(&odb, &sub_index, "").expect("tree");
        let sub_cache = build_cache_tree_from_index(&odb, &sub_index).expect("cache");
        sub_index.set_cache_tree(sub_cache);
        let diffs =
            diff_index_to_tree(&odb, &sub_index, Some(&sub_tree), true).expect("gitlink diff");
        assert!(
            diffs.is_empty(),
            "ignored gitlink should not diff: {diffs:?}"
        );
    }

    #[test]
    fn ignore_submodules_skips_gitlink_in_added_directory_cache_walk() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let gitlink: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
            .parse()
            .expect("oid");
        let head_tree = write_tree_from_index(&odb, &Index::new(), "").expect("empty tree");
        let mut index = Index::new();
        index.add_or_replace(index_entry("newdir/sub", MODE_GITLINK, gitlink));
        let cache = build_cache_tree_from_index(&odb, &index).expect("cache");
        index.set_cache_tree(cache);
        index.invalidate_cache_tree_for_path(b"newdir/sub");

        let fast = diff_index_to_tree(&odb, &index, Some(&head_tree), true).expect("fast");
        let flat =
            diff_index_to_tree_flatten_reference(&odb, &index, &head_tree, true).expect("flat");
        assert_eq!(fast, flat);
        assert!(
            fast.is_empty(),
            "nested gitlinks in added dirs must be ignored: {fast:?}"
        );
    }

    #[test]
    fn ignore_submodules_skips_gitlink_in_deleted_directory_cache_walk() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let blob = odb.write(ObjectKind::Blob, b"keep").expect("blob");
        let gitlink: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
            .parse()
            .expect("oid");
        let mut index = Index::new();
        index.add_or_replace(index_entry("keep.txt", MODE_REGULAR, blob));
        index.add_or_replace(index_entry("olddir/sub", MODE_GITLINK, gitlink));
        let head_tree = write_tree_from_index(&odb, &index, "").expect("head");
        let cache = build_cache_tree_from_index(&odb, &index).expect("cache");
        index.set_cache_tree(cache);
        index
            .entries
            .retain(|e| !String::from_utf8_lossy(&e.path).starts_with("olddir/"));
        index.invalidate_cache_tree_for_path(b"olddir/sub");

        let fast = diff_index_to_tree(&odb, &index, Some(&head_tree), true).expect("fast");
        let flat =
            diff_index_to_tree_flatten_reference(&odb, &index, &head_tree, true).expect("flat");
        assert_eq!(fast, flat);
        assert!(
            fast.is_empty(),
            "nested gitlinks in deleted dirs must be ignored: {fast:?}"
        );
    }

    #[test]
    fn diff_trees_skips_identical_subtree_oids() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let leaf = odb.write(ObjectKind::Blob, b"leaf").expect("blob");
        let mut deep = Index::new();
        for i in 0..20 {
            deep.add_or_replace(index_entry(&format!("deep/n{i}.txt"), MODE_REGULAR, leaf));
        }
        let tree = write_tree_from_index(&odb, &deep, "").expect("tree");
        let reads = TreeReadCounter::start();
        let entries = diff_trees(&odb, Some(&tree), Some(&tree), "").expect("diff");
        assert!(entries.is_empty());
        assert_eq!(
            reads.get(),
            0,
            "identical tree oids should short-circuit without reading tree objects"
        );
    }

    #[test]
    fn diff_tree_path_change_count_matches_diff_trees() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let a = odb.write(ObjectKind::Blob, b"a").expect("blob");
        let b = odb.write(ObjectKind::Blob, b"b").expect("blob");
        let mut left = Index::new();
        left.add_or_replace(index_entry("f.txt", MODE_REGULAR, a));
        let mut right = Index::new();
        right.add_or_replace(index_entry("f.txt", MODE_REGULAR, b));
        right.add_or_replace(index_entry("g.txt", MODE_REGULAR, a));
        let old_tree = write_tree_from_index(&odb, &left, "").expect("old");
        let new_tree = write_tree_from_index(&odb, &right, "").expect("new");
        let entries = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "").expect("diff");
        let count =
            diff_tree_path_change_count(&odb, Some(&old_tree), Some(&new_tree), "").expect("count");
        assert_eq!(count, entries.len());
        assert_eq!(count, 2);
    }

    #[test]
    fn diff_trees_matches_whether_trees_were_cached_parsed_or_raw() {
        let temp = TempDir::new().expect("tempdir");
        let a = Odb::new(temp.path())
            .write(ObjectKind::Blob, b"a")
            .expect("blob");
        let b = Odb::new(temp.path())
            .write(ObjectKind::Blob, b"b")
            .expect("blob");
        let mut left = Index::new();
        let mut right = Index::new();
        for i in 0..50 {
            left.add_or_replace(index_entry(&format!("same{i:02}.txt"), MODE_REGULAR, a));
            right.add_or_replace(index_entry(&format!("same{i:02}.txt"), MODE_REGULAR, a));
        }
        left.add_or_replace(index_entry("dir/changed.txt", MODE_REGULAR, a));
        left.add_or_replace(index_entry("dir/kept.txt", MODE_REGULAR, a));
        left.add_or_replace(index_entry("gone/x.txt", MODE_REGULAR, a));
        left.add_or_replace(index_entry("mode.sh", MODE_REGULAR, a));
        left.add_or_replace(index_entry("swap", MODE_REGULAR, a));
        right.add_or_replace(index_entry("dir/changed.txt", MODE_REGULAR, b));
        right.add_or_replace(index_entry("dir/kept.txt", MODE_REGULAR, a));
        right.add_or_replace(index_entry("mode.sh", MODE_EXECUTABLE, a));
        right.add_or_replace(index_entry("new/y.txt", MODE_REGULAR, b));
        right.add_or_replace(index_entry("swap/inner.txt", MODE_REGULAR, b));
        let setup = Odb::new(temp.path());
        let old_tree = write_tree_from_index(&setup, &left, "").expect("old");
        let new_tree = write_tree_from_index(&setup, &right, "").expect("new");

        let raw = diff_trees(&Odb::new(temp.path()), Some(&old_tree), Some(&new_tree), "")
            .expect("raw diff");
        let changed: Vec<(DiffStatus, &str)> = raw.iter().map(|e| (e.status, e.path())).collect();
        assert_eq!(
            changed,
            vec![
                (DiffStatus::Modified, "dir/changed.txt"),
                (DiffStatus::Deleted, "gone/x.txt"),
                (DiffStatus::Modified, "mode.sh"),
                (DiffStatus::Added, "new/y.txt"),
                (DiffStatus::Deleted, "swap"),
                (DiffStatus::Added, "swap/inner.txt"),
            ]
        );

        let parsed = Odb::new(temp.path());
        parsed.cached_tree_entries(&old_tree).expect("parse old");
        parsed.cached_tree_entries(&new_tree).expect("parse new");
        assert_eq!(
            diff_trees(&parsed, Some(&old_tree), Some(&new_tree), "").expect("parsed diff"),
            raw
        );

        let mixed = Odb::new(temp.path());
        mixed.cached_tree_entries(&new_tree).expect("parse new");
        assert_eq!(
            diff_trees(&mixed, Some(&old_tree), Some(&new_tree), "").expect("mixed diff"),
            raw
        );
        let reparsed = mixed
            .cached_tree_entries(&old_tree)
            .expect("parse cached raw");
        assert_eq!(
            reparsed.as_slice(),
            crate::objects::parse_tree(&mixed.read(&old_tree).expect("read").data)
                .expect("parse")
                .as_slice(),
            "a tree cached raw by a diff parses to the same entries"
        );
    }

    #[test]
    fn diff_trees_reports_a_corrupt_tree() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let good = write_tree_from_index(&odb, &Index::new(), "").expect("empty tree");
        let corrupt = odb
            .write(ObjectKind::Tree, b"100644 name-without-oid")
            .expect("tree");
        let err = diff_trees(&odb, Some(&good), Some(&corrupt), "").expect_err("corrupt");
        assert!(matches!(err, Error::CorruptObject(_)), "{err:?}");
    }
}

#[cfg(test)]
mod changed_flags_shared_tests {
    use super::git_xdiff;
    use crate::line_interner::LineInterner;

    /// Cutting identical leading and trailing records out and passing their occurrence counts
    /// gives the same changed records as diffing the whole files.
    #[test]
    fn shared_counts_stand_in_for_records_left_out() {
        // `}` is frequent only outside the cut, which makes xdiff discard the middle `}` between
        // unique lines instead of matching it.
        let mut old: Vec<String> = Vec::new();
        let mut new: Vec<String> = Vec::new();
        for i in 0..60 {
            for side in [&mut old, &mut new] {
                side.push(format!("fn f{i}() {{\n"));
                side.push("}\n".to_owned());
            }
        }
        old.extend(
            [
                "u1\n", "u2\n", "u3\n", "u4\n", "}\n", "u5\n", "u6\n", "u7\n",
            ]
            .map(str::to_owned),
        );
        new.extend(
            [
                "v1\n", "v2\n", "v3\n", "v4\n", "}\n", "v5\n", "v6\n", "v7\n",
            ]
            .map(str::to_owned),
        );
        for i in 0..40 {
            for side in [&mut old, &mut new] {
                side.push(format!("fn g{i}() {{\n"));
                side.push("}\n".to_owned());
            }
        }
        let old: Vec<&str> = old.iter().map(String::as_str).collect();
        let new: Vec<&str> = new.iter().map(String::as_str).collect();
        let (head, tail) = (120, 80);

        let mut interner = LineInterner::with_capacity(old.len() + new.len());
        let ids1 = interner.intern_all(&old);
        let ids2 = interner.intern_all(&new);
        let (full1, full2) = git_xdiff::changed_flags(&ids1, &ids2, false);

        let mut cut = LineInterner::default();
        let cut1 = cut.intern_all(&old[head..old.len() - tail]);
        let cut2 = cut.intern_all(&new[head..new.len() - tail]);
        let mut shared = vec![0u32; cut.len()];
        for line in old[..head].iter().chain(&old[old.len() - tail..]) {
            if let Some(id) = cut.get(line) {
                shared[id as usize] += 1;
            }
        }
        let (part1, part2) =
            git_xdiff::changed_flags_with_shared(&cut1, &cut2, &shared, head + tail, false);
        assert_eq!(part1, full1[head..old.len() - tail]);
        assert_eq!(part2, full2[head..new.len() - tail]);
        assert!(full1[..head]
            .iter()
            .chain(&full1[old.len() - tail..])
            .all(|c| !c));
        // Like `git diff`, the whole middle is one change: the `}` among unique lines is
        // discarded because it is frequent in the whole file.
        assert!(full1[head..old.len() - tail].iter().all(|&c| c));
        let (alone1, _) = git_xdiff::changed_flags(&cut1, &cut2, false);
        assert!(
            !alone1[4],
            "counted alone, the cut keeps the `}}` as a match"
        );
    }
}

#[cfg(test)]
mod gitlink_tree_worktree_tests {
    use std::fs;

    use crate::index::{Index, IndexEntry, MODE_GITLINK};
    use crate::objects::{ObjectId, ObjectKind};
    use crate::repo::init_repository;

    use super::diff_tree_to_worktree;

    fn encode_tree_gitlink(name: &str, oid: &ObjectId) -> Vec<u8> {
        let mut data = format!("160000 {name}\0").into_bytes();
        data.extend_from_slice(oid.as_bytes());
        data
    }

    #[test]
    fn unpopulated_gitlink_matching_index_produces_no_diff_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work_tree = dir.path();
        fs::create_dir(work_tree.join("sub")).expect("sub placeholder");

        let repo = init_repository(
            work_tree,
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let gitlink: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
            .parse()
            .expect("gitlink oid");
        let tree_oid = repo
            .odb()
            .write(ObjectKind::Tree, &encode_tree_gitlink("sub", &gitlink))
            .expect("write tree");

        let path = b"sub".to_vec();
        let mut index = Index::new();
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_GITLINK,
            uid: 0,
            gid: 0,
            size: 0,
            oid: gitlink,
            flags: (path.len().min(0xfff)) as u16,
            flags_extended: None,
            path,
            base_index_pos: 0,
        });

        let entries = diff_tree_to_worktree(&repo.odb(), Some(&tree_oid), work_tree, &index)
            .expect("diff_tree_to_worktree");
        assert!(
            entries.is_empty(),
            "clean uninitialized gitlink should not appear in tree→worktree diff: {entries:?}"
        );
    }

    #[test]
    fn unpopulated_gitlink_with_stray_file_produces_no_diff_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work_tree = dir.path();
        let sub = work_tree.join("sub");
        fs::create_dir(&sub).expect("sub placeholder");
        fs::write(sub.join("x.txt"), b"stray\n").expect("stray file");

        let repo = init_repository(
            work_tree,
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let gitlink: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
            .parse()
            .expect("gitlink oid");
        let tree_oid = repo
            .odb()
            .write(ObjectKind::Tree, &encode_tree_gitlink("sub", &gitlink))
            .expect("write tree");

        let path = b"sub".to_vec();
        let mut index = Index::new();
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_GITLINK,
            uid: 0,
            gid: 0,
            size: 0,
            oid: gitlink,
            flags: (path.len().min(0xfff)) as u16,
            flags_extended: None,
            path,
            base_index_pos: 0,
        });

        let tree_entries = diff_tree_to_worktree(&repo.odb(), Some(&tree_oid), work_tree, &index)
            .expect("diff_tree_to_worktree");
        assert!(
            tree_entries.is_empty(),
            "uninitialized gitlink with stray files should not diff: {tree_entries:?}"
        );

        let wt_entries =
            super::diff_index_to_worktree(&repo.odb(), &mut index.clone(), work_tree, false, true)
                .expect("diff_index_to_worktree");
        assert!(
            wt_entries.is_empty(),
            "uninitialized gitlink with stray files should not diff: {wt_entries:?}"
        );

        let ie = index.entries()[0].clone();
        assert!(
            !super::worktree_differs_from_index_entry(
                repo.odb(),
                repo.git_dir(),
                work_tree,
                &ie,
                false,
                None,
                None,
            )
            .expect("differs"),
            "stray files in uninitialized submodule should not differ from index"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn untracked_only_in_checked_out_submodule_produces_no_diff_entry() {
        use std::process::Command;

        let dir = tempfile::tempdir().expect("tempdir");
        let work_tree = dir.path();
        fs::create_dir(work_tree.join("sub")).expect("sub dir");

        let repo = init_repository(
            work_tree,
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let sub = work_tree.join("sub");
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "T"][..],
            &["config", "user.email", "t@e.com"][..],
        ] {
            let status = Command::new("git")
                .current_dir(&sub)
                .args(args)
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        }
        fs::write(sub.join("tracked.txt"), b"v1\n").expect("write");
        for args in [&["add", "tracked.txt"][..], &["commit", "-qm", "sub"][..]] {
            let status = Command::new("git")
                .current_dir(&sub)
                .args(args)
                .status()
                .expect("git");
            assert!(status.success());
        }
        let head_out = Command::new("git")
            .current_dir(&sub)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let gitlink = std::str::from_utf8(&head_out.stdout)
            .expect("utf8")
            .trim()
            .parse::<ObjectId>()
            .expect("oid");

        let tree_oid = repo
            .odb()
            .write(ObjectKind::Tree, &encode_tree_gitlink("sub", &gitlink))
            .expect("tree");
        let path = b"sub".to_vec();
        let mut index = Index::new();
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_GITLINK,
            uid: 0,
            gid: 0,
            size: 0,
            oid: gitlink,
            flags: (path.len().min(0xfff)) as u16,
            flags_extended: None,
            path,
            base_index_pos: 0,
        });

        fs::write(sub.join("untracked.txt"), b"extra\n").expect("untracked");

        let entries = diff_tree_to_worktree(&repo.odb(), Some(&tree_oid), work_tree, &index)
            .expect("diff_tree_to_worktree");
        assert!(
            entries.is_empty(),
            "untracked-only submodule dirt must not appear in default tree→worktree diff: {entries:?}"
        );
    }
}

#[cfg(test)]
mod racy_index_mtime_diff_tests {
    use super::*;
    use crate::index::{entry_from_metadata, Index, MODE_REGULAR};
    use crate::objects::{HashAlgo, ObjectKind};
    use filetime::FileTime;
    use std::fs;
    use std::path::PathBuf;

    const INDEX_MTIME: (u32, u32) = (1_700_000_000, 123_456_789);

    fn pin_mtime(path: &Path, sec: u32, nsec: u32) {
        filetime::set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec))
            .expect("set mtime");
    }

    fn minimal_sha256_repo(work_tree: &Path) -> (PathBuf, Odb) {
        let git_dir = work_tree.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        fs::write(
            git_dir.join("config"),
            "[extensions]\n\tobjectformat = sha256\n",
        )
        .expect("config");
        let odb = Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.clone());
        (git_dir, odb)
    }

    #[test]
    fn detects_same_size_change_when_entry_mtime_matches_index_mtime() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let content_a = b"same length!!";
        let content_b = b"same length??";
        assert_eq!(content_a.len(), content_b.len());

        fs::write(wt.join("f.txt"), content_a).expect("write");
        pin_mtime(&wt.join("f.txt"), INDEX_MTIME.0, INDEX_MTIME.1);
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        let git_dir = wt.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        let odb = Odb::new(&git_dir.join("objects"));
        let oid_a = odb.hash(ObjectKind::Blob, content_a).expect("hash");
        let entry = entry_from_metadata(&meta, b"f.txt", oid_a, MODE_REGULAR);

        let mut index = Index::new();
        index.source_mtime = Some(INDEX_MTIME);
        index.entries.push(entry);

        fs::write(wt.join("f.txt"), content_b).expect("rewrite");
        pin_mtime(&wt.join("f.txt"), INDEX_MTIME.0, INDEX_MTIME.1);

        let diff = diff_index_to_worktree(&odb, &mut index, wt, false, false).expect("diff");
        assert_eq!(
            diff.len(),
            1,
            "racy entry must be re-hashed, not stat-trusted"
        );
        assert_eq!(diff[0].path(), "f.txt");
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_symlink_clean_racy_equal_mtime_reports_no_diff() {
        use crate::index::MODE_SYMLINK;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let target = std::ffi::OsStr::from_bytes(b"bad-\xff-target");
        let link_path = wt.join("link");
        symlink(target, &link_path).expect("symlink");

        let ft = FileTime::from_unix_time(i64::from(INDEX_MTIME.0), INDEX_MTIME.1);
        filetime::set_symlink_file_times(&link_path, ft, ft).expect("set symlink mtime");
        let meta = fs::symlink_metadata(&link_path).expect("stat");
        let git_dir = wt.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        let odb = Odb::new(&git_dir.join("objects"));
        let oid = odb.hash(ObjectKind::Blob, target.as_bytes()).expect("hash");
        let entry = entry_from_metadata(&meta, b"link", oid, MODE_SYMLINK);

        let mut index = Index::new();
        index.source_mtime = Some(INDEX_MTIME);
        index.entries.push(entry);

        let diff = diff_index_to_worktree(&odb, &mut index, wt, false, false).expect("diff");
        assert!(
            diff.is_empty(),
            "clean non-UTF-8 symlink must not be reported modified: {diff:?}"
        );
    }

    #[test]
    fn sha256_clean_racy_equal_mtime_reports_no_diff() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (_git_dir, odb) = minimal_sha256_repo(wt);
        assert_eq!(odb.hash_algo(), HashAlgo::Sha256);

        let bytes = b"sha256 width!!";
        fs::write(wt.join("f.txt"), bytes).expect("write");
        pin_mtime(&wt.join("f.txt"), INDEX_MTIME.0, INDEX_MTIME.1);
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        let oid = odb.hash(ObjectKind::Blob, bytes).expect("hash");
        let entry = entry_from_metadata(&meta, b"f.txt", oid, MODE_REGULAR);

        let mut index = Index::new();
        index.hash_algo = HashAlgo::Sha256;
        index.source_mtime = Some(INDEX_MTIME);
        index.entries.push(entry);

        let diff = diff_index_to_worktree(&odb, &mut index, wt, false, false).expect("diff");
        assert!(
            diff.is_empty(),
            "clean SHA-256 racy entry must not be reported modified: {diff:?}"
        );
    }
}

#[cfg(test)]
mod smudge_racily_clean_tests {
    use super::*;
    use crate::config::ConfigSet;
    use crate::crlf::{self, FileAttrs};
    use crate::index::{entry_from_metadata, Index, MODE_REGULAR};
    use crate::objects::{HashAlgo, ObjectKind};
    use filetime::FileTime;
    use std::fs;
    use std::path::PathBuf;

    const INDEX_MTIME_PIN: (u32, u32) = (1_700_000_000, 123_456_789);

    fn mtime_from_metadata(meta: &fs::Metadata) -> (u32, u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            (meta.mtime() as u32, meta.mtime_nsec() as u32)
        }
        #[cfg(not(unix))]
        {
            use std::time::UNIX_EPOCH;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .unwrap_or_default();
            (mtime.as_secs() as u32, mtime.subsec_nanos())
        }
    }

    /// Pin mtime then read back the value the index would store (NTFS quantizes subseconds).
    fn pin_mtime_and_sample(path: &Path, sec: u32, nsec: u32) -> (u32, u32) {
        filetime::set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec))
            .expect("set mtime");
        let meta = fs::symlink_metadata(path).expect("stat file");
        mtime_from_metadata(&meta)
    }

    /// Sample the on-disk index mtime used in racy-git tests (matches filesystem quantization).
    fn setup_index_mtime(work_tree: &Path) -> (u32, u32) {
        let probe = work_tree.join(".grit-index-mtime-probe");
        fs::write(&probe, b"").expect("probe file");
        pin_mtime_and_sample(&probe, INDEX_MTIME_PIN.0, INDEX_MTIME_PIN.1)
    }

    fn minimal_repo(work_tree: &Path, config: &str) -> (PathBuf, Odb) {
        let git_dir = work_tree.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        fs::write(git_dir.join("config"), config).expect("config");
        let odb = Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.clone());
        (git_dir, odb)
    }

    fn staged_entry_for_file(
        odb: &Odb,
        work_tree: &Path,
        rel: &str,
        bytes: &[u8],
        mtime: (u32, u32),
    ) -> IndexEntry {
        let path = work_tree.join(rel);
        fs::write(&path, bytes).expect("write file");
        pin_mtime_and_sample(&path, mtime.0, mtime.1);
        let meta = fs::symlink_metadata(&path).expect("stat file");
        let oid = odb.hash(ObjectKind::Blob, bytes).expect("hash");
        entry_from_metadata(&meta, rel.as_bytes(), oid, MODE_REGULAR)
    }

    fn staged_entry_with_oid(
        work_tree: &Path,
        rel: &str,
        worktree_bytes: &[u8],
        oid: crate::objects::ObjectId,
        mtime: (u32, u32),
    ) -> IndexEntry {
        let path = work_tree.join(rel);
        fs::write(&path, worktree_bytes).expect("write file");
        pin_mtime_and_sample(&path, mtime.0, mtime.1);
        let meta = fs::symlink_metadata(&path).expect("stat file");
        entry_from_metadata(&meta, rel.as_bytes(), oid, MODE_REGULAR)
    }

    #[test]
    fn index_stat_cache_trustworthy_rejects_unknown_mtime() {
        assert!(!super::index_stat_cache_trustworthy(None));
        assert!(!super::index_stat_cache_trustworthy(Some((0, 0))));
        assert!(super::index_stat_cache_trustworthy(Some((1, 0))));
    }

    #[test]
    fn index_entry_skips_content_hash_after_stat_match_when_trusted_and_not_racy() {
        use crate::index::StatMatchPolicy;
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb) = minimal_repo(wt, "[core]\n\tfilemode = true\n");
        let rel = "trusted.txt";
        let file_mtime = (1_700_000_000, 0);
        let mut entry = staged_entry_for_file(&odb, wt, rel, b"payload\n", file_mtime);
        let index_mtime = (file_mtime.0 + 1, 0);
        let policy = StatMatchPolicy::from_config(None);
        assert!(
            super::index_entry_skips_worktree_content_hash_after_stat_match(
                &entry,
                Some(index_mtime),
                policy,
            )
        );
        entry.size = 0;
        entry.mtime_sec = 0;
        entry.mtime_nsec = 0;
        assert!(
            !super::index_entry_skips_worktree_content_hash_after_stat_match(
                &entry,
                Some(index_mtime),
                policy,
            )
        );
        let _ = git_dir;
    }

    #[cfg(unix)]
    #[test]
    fn symlink_dir_cache_detects_symlinked_ancestors_across_threads() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        fs::create_dir_all(wt.join("real/deep")).expect("mkdir");
        fs::create_dir_all(wt.join("target/deep")).expect("mkdir");
        std::os::unix::fs::symlink(wt.join("target"), wt.join("linked")).expect("symlink");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let mut cache = super::SymlinkDirCache::default();
                    assert!(!cache.has_symlink_in_path_bytes(wt, b"top.txt", false, false));
                    assert!(!cache.has_symlink_in_path_bytes(wt, b"linked", false, false));
                    assert!(!cache.has_symlink_in_path_bytes(wt, b"real/deep/f", false, false));
                    assert!(cache.has_symlink_in_path_bytes(wt, b"linked/f", false, false));
                    assert!(cache.has_symlink_in_path_bytes(wt, b"linked/deep/f", false, false));
                    assert!(!cache.has_symlink_in_path_bytes(wt, b"\xff\xfe", false, false));
                });
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlink_dir_cache_accepts_a_work_tree_reached_through_a_symlink() {
        let td = tempfile::tempdir().expect("tempdir");
        let real = td.path().join("real");
        fs::create_dir_all(real.join("a/b/c")).expect("mkdir");
        fs::create_dir_all(real.join("t/b/c")).expect("mkdir");
        std::os::unix::fs::symlink(real.join("t"), real.join("l")).expect("symlink");
        let wt = td.path().join("alias");
        std::os::unix::fs::symlink(&real, &wt).expect("symlink");
        let mut cache = super::SymlinkDirCache::default();
        assert!(!cache.has_symlink_in_path_bytes(&wt, b"a/b/c/f", false, false));
        assert!(cache.has_symlink_in_path_bytes(&wt, b"l/b/c/f", false, false));
        assert!(!cache.has_symlink_in_path_bytes(&real, b"a/b/c/f", false, false));
        assert!(cache.has_symlink_in_path_bytes(&real, b"l/b/c/f", false, false));
    }

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn real_dir_chain_confirms_only_symlink_free_directories() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        fs::create_dir_all(wt.join("a/b/c")).expect("mkdir");
        fs::create_dir_all(wt.join("t/c")).expect("mkdir");
        fs::write(wt.join("a/file"), b"f").expect("write");
        std::os::unix::fs::symlink(wt.join("t"), wt.join("a/l")).expect("symlink");
        let mut chain = crate::worktree_scan::RealDirChain::open(wt);
        assert_eq!(chain.work_tree(), wt);
        assert!(chain.confirms(b"a"));
        assert!(chain.confirms(b"a/b/c"));
        assert!(!chain.confirms(b"a/l"));
        assert!(!chain.confirms(b"a/l/c"));
        assert!(!chain.confirms(b"a/file"));
        assert!(!chain.confirms(b"a/missing/c"));
        assert!(!chain.confirms(b""));
        assert!(!chain.confirms(b"a/\0b"));
        assert!(chain.confirms(b"a/b"));
    }

    #[test]
    fn symlink_dir_cache_probes_each_new_directory_once_in_index_order() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let depth = 12;
        let chains = 20;
        let mut paths = Vec::new();
        for chain in 0..chains {
            let dir: Vec<String> = (0..depth)
                .map(|level| format!("c{chain:02}l{level:02}"))
                .collect();
            let dir = format!("shared/{}", dir.join("/"));
            fs::create_dir_all(wt.join(&dir)).expect("mkdir");
            paths.extend((0..3).map(|file| format!("{dir}/f{file}")));
        }
        paths.sort();
        let mut cache = super::SymlinkDirCache::default();
        for rel in &paths {
            assert!(!cache.has_symlink_in_path_bytes(wt, rel.as_bytes(), false, false));
        }
        // Each chain's directories are new; `shared` and sibling files are already known.
        let unique_dirs = 1 + chains * depth;
        assert!(
            cache.fs_queries <= unique_dirs,
            "{} filesystem queries for {unique_dirs} directories",
            cache.fs_queries
        );
        #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
        assert!(
            cache.fs_queries <= chains + 1,
            "one chain walk per new directory: {} queries for {chains} chains",
            cache.fs_queries
        );
    }

    #[test]
    fn common_dir_prefix_len_stops_at_whole_components() {
        use super::{common_dir_prefix_len, path_within_dir};
        assert_eq!(common_dir_prefix_len(b"a/b/c", b"a/b/c"), 5);
        assert_eq!(common_dir_prefix_len(b"a/b/c", b"a/b"), 3);
        assert_eq!(common_dir_prefix_len(b"a/b", b"a/b/c"), 3);
        assert_eq!(common_dir_prefix_len(b"a/bc/d", b"a/b"), 1);
        assert_eq!(common_dir_prefix_len(b"a/b", b"a/bc/d"), 1);
        assert_eq!(common_dir_prefix_len(b"ab", b"a"), 0);
        assert_eq!(common_dir_prefix_len(b"a/b", b""), 0);
        assert!(path_within_dir(b"a/b", b"a/b"));
        assert!(path_within_dir(b"a/b/c", b"a/b"));
        assert!(!path_within_dir(b"a/bc", b"a/b"));
        assert!(!path_within_dir(b"a", b"a/b"));
    }

    /// Answer the ancestor-symlink question without any caching, one probe per ancestor.
    #[cfg(unix)]
    fn naive_has_symlink_ancestor(wt: &Path, rel: &str) -> bool {
        let Some((dir, _)) = rel.rsplit_once('/') else {
            return false;
        };
        let mut prefix = wt.to_path_buf();
        dir.split('/').any(|component| {
            prefix.push(component);
            fs::symlink_metadata(&prefix).is_ok_and(|m| m.file_type().is_symlink())
        })
    }

    #[cfg(unix)]
    #[test]
    fn symlink_dir_cache_matches_uncached_probe_in_any_query_order() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        fs::create_dir_all(wt.join("a/b/c/d")).expect("mkdir");
        fs::create_dir_all(wt.join("a/bb/c")).expect("mkdir");
        fs::create_dir_all(wt.join("a/b2")).expect("mkdir");
        fs::create_dir_all(wt.join("t/c/d")).expect("mkdir");
        fs::create_dir_all(wt.join("x/y")).expect("mkdir");
        fs::write(wt.join("a/file"), b"f").expect("write");
        fs::write(wt.join("x/y/plain"), b"f").expect("write");
        std::os::unix::fs::symlink(wt.join("t"), wt.join("a/b/link")).expect("symlink");
        std::os::unix::fs::symlink(wt.join("t"), wt.join("x/l")).expect("symlink");
        std::os::unix::fs::symlink(wt.join("t"), wt.join("top")).expect("symlink");

        let paths = [
            "a/b/c/d/f",
            "a/b/c/f",
            "a/b/f",
            "a/b/link/f",
            "a/b/link/c/d/f",
            "a/b/linker/f",
            "a/b2/f",
            "a/bb/c/f",
            "a/file/under/f",
            "a/missing/deeper/f",
            "a/missing/f",
            "a/f",
            "f",
            "top/c/f",
            "top/f",
            "topper/f",
            "x/l/c/d/f",
            "x/y/f",
            "x/y/plain/f",
        ];
        let mut orders: Vec<Vec<&str>> = vec![paths.to_vec()];
        orders.push(paths.iter().rev().copied().collect());
        orders.push(
            paths
                .iter()
                .step_by(2)
                .chain(paths.iter().skip(1).step_by(2))
                .copied()
                .collect(),
        );
        orders.push(paths.iter().flat_map(|p| [*p, *p]).collect());
        for order in orders {
            let mut cache = super::SymlinkDirCache::default();
            for rel in order {
                assert_eq!(
                    cache.has_symlink_in_path_bytes(wt, rel.as_bytes(), false, false),
                    naive_has_symlink_ancestor(wt, rel),
                    "ancestor symlink answer for {rel}"
                );
            }
        }
    }

    #[test]
    fn content_hash_skip_trusts_empty_blob_but_not_smudged_size() {
        use crate::index::StatMatchPolicy;
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (_git_dir, odb) = minimal_repo(wt, "[core]\n\tfilemode = false\n");
        let file_mtime = (1_700_000_000, 0);
        let index_mtime = Some((file_mtime.0 + 1, 0));
        let policy = StatMatchPolicy::from_config(None);

        let empty = staged_entry_for_file(&odb, wt, "empty.txt", b"", file_mtime);
        assert_eq!(empty.size, 0);
        assert!(
            super::index_entry_skips_worktree_content_hash_after_stat_match(
                &empty,
                index_mtime,
                policy,
            )
        );

        let smudged_oid = odb.hash(ObjectKind::Blob, b"abc\n").expect("hash");
        let smudged = staged_entry_with_oid(wt, "smudged.txt", b"", smudged_oid, file_mtime);
        assert_eq!(smudged.size, 0);
        assert!(
            !super::index_entry_skips_worktree_content_hash_after_stat_match(
                &smudged,
                index_mtime,
                policy,
            )
        );
    }

    #[test]
    fn status_diff_reports_smudged_entry_whose_file_became_empty() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb) = minimal_repo(wt, "[core]\n\tfilemode = false\n");
        let oid = odb.write(ObjectKind::Blob, b"abc\n").expect("write blob");
        let file_mtime = (1_700_000_000, 0);
        let mut index = Index::new();
        index.entries.push(staged_entry_with_oid(
            wt,
            "smudged.txt",
            b"",
            oid,
            file_mtime,
        ));
        let (entries, _) = super::diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            super::DiffIndexToWorktreeOptions {
                repository_git_dir: Some(git_dir),
                index_mtime: Some((file_mtime.0 + 1, 0)),
                for_status: true,
                stat_parallel_threads: Some(1),
                ..Default::default()
            },
        )
        .expect("diff");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].status, DiffStatus::Modified);
    }

    #[test]
    fn status_diff_verifies_content_when_index_stat_cache_unset() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let rel = "unset-stat.txt";
        let bytes = b"same bytes\n";
        let oid = odb.hash(ObjectKind::Blob, bytes).expect("hash");
        fs::write(wt.join(rel), bytes).expect("write");
        let mut index = Index::new();
        let mut entry = staged_entry_for_file(&odb, wt, rel, bytes, (100, 0));
        entry.size = 0;
        entry.mtime_sec = 0;
        entry.mtime_nsec = 0;
        entry.oid = oid;
        index.entries.push(entry);
        let (entries, _) = super::diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            super::DiffIndexToWorktreeOptions {
                repository_git_dir: Some(git_dir),
                for_status: true,
                ..Default::default()
            },
        )
        .expect("diff");
        assert!(
            entries.is_empty(),
            "unset stat must not size-mismatch a matching worktree: {entries:?}"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn status_reports_modified_when_ctime_differs_but_mtime_sec_and_size_match_non_racy() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let rel = "tracked.txt";
        let original = b"original\n";
        let modified = b"modified\n";
        assert_eq!(original.len(), modified.len());

        let entry_mtime = (1_700_000_000u32, 0u32);
        let index_file_mtime = (1_700_000_100u32, 0u32);

        std::process::Command::new("git")
            .args(["-C", &wt.to_string_lossy(), "init", "-q"])
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args([
                "-C",
                &wt.to_string_lossy(),
                "config",
                "user.email",
                "t@example.com",
            ])
            .status()
            .expect("git config email");
        std::process::Command::new("git")
            .args(["-C", &wt.to_string_lossy(), "config", "user.name", "T"])
            .status()
            .expect("git config name");

        fs::write(wt.join(rel), original).expect("write original");
        pin_mtime_and_sample(&wt.join(rel), entry_mtime.0, entry_mtime.1);
        std::process::Command::new("git")
            .args(["-C", &wt.to_string_lossy(), "add", rel])
            .status()
            .expect("git add");
        std::process::Command::new("git")
            .args([
                "-C",
                &wt.to_string_lossy(),
                "commit",
                "-qm",
                "seed",
                "--no-gpg-sign",
            ])
            .status()
            .expect("git commit");

        let mut index = Index::new();
        index.source_mtime = Some(index_file_mtime);
        let staged = staged_entry_for_file(&odb, wt, rel, original, entry_mtime);
        let oid_original = staged.oid;
        index.entries.push(staged);

        fs::write(wt.join(rel), modified).expect("modify worktree");
        let git_dirty = std::process::Command::new("git")
            .args([
                "-C",
                &wt.to_string_lossy(),
                "--no-optional-locks",
                "status",
                "--short",
            ])
            .output()
            .expect("git status");
        assert!(
            git_dirty.status.success(),
            "git status failed: {}",
            String::from_utf8_lossy(&git_dirty.stderr)
        );
        assert!(
            String::from_utf8_lossy(&git_dirty.stdout).contains("tracked.txt"),
            "system git must see modification before mtime pin: {}",
            String::from_utf8_lossy(&git_dirty.stdout)
        );

        pin_mtime_and_sample(&wt.join(rel), entry_mtime.0, entry_mtime.1);

        let meta = fs::symlink_metadata(wt.join(rel)).expect("stat");
        let ie = &mut index.entries[0];
        assert_eq!(ie.oid, oid_original);
        if stat_matches(ie, &meta) {
            ie.ctime_sec = ie.ctime_sec.wrapping_sub(3600);
        }
        assert!(
            !stat_matches(ie, &meta),
            "ctime/inode change must fail full stat match"
        );
        let stat_policy = crate::index::StatMatchPolicy::default();
        assert!(
            !entry_is_racy_with_policy(ie, Some(index_file_mtime), stat_policy),
            "fixture must be non-racy"
        );

        let (entries, index_changed) = super::diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            super::DiffIndexToWorktreeOptions {
                index_mtime: Some(index_file_mtime),
                repository_git_dir: Some(git_dir),
                refresh_index_stat_in_pass: true,
                for_status: true,
                ..Default::default()
            },
        )
        .expect("diff");
        assert_eq!(
            entries.len(),
            1,
            "must report modified, not refresh stat clean: {entries:?}, index_changed={index_changed}"
        );
        assert_eq!(entries[0].path(), rel);
    }

    #[test]
    fn diff_name_status_skips_big_worktree_read_on_size_mismatch() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb) = minimal_repo(wt, "[core]\n\tbigFileThreshold = 4k\n");
        let rel = "big.bin";
        let indexed = vec![b'x'; 100];
        let mut index = Index::new();
        index
            .entries
            .push(staged_entry_for_file(&odb, wt, rel, &indexed, (100, 0)));
        fs::write(wt.join(rel), vec![b'y'; 5000]).expect("rewrite bigger");
        let metrics = odb.hot_path_metrics();
        metrics.reset_blob_content_reads();
        metrics.set_blob_counting(true);
        let (entries, _) = super::diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            super::DiffIndexToWorktreeOptions {
                repository_git_dir: Some(git_dir),
                for_status: false,
                ..Default::default()
            },
        )
        .expect("diff");
        metrics.set_blob_counting(false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, super::DiffStatus::Modified);
        assert_eq!(entries[0].new_oid, super::zero_oid());
        assert_eq!(
            metrics.blob_content_reads(),
            0,
            "large size-only drift must not hash or read the worktree"
        );
    }

    #[test]
    fn status_diff_skips_hash_when_worktree_size_differs() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let rel = "size-change.txt";
        let original = b"short\n";
        let mut index = Index::new();
        index
            .entries
            .push(staged_entry_for_file(&odb, wt, rel, original, (100, 0)));
        fs::write(wt.join(rel), b"longer content\n").expect("rewrite");
        let metrics = odb.hot_path_metrics();
        metrics.reset_blob_content_reads();
        metrics.set_blob_counting(true);
        let (entries, _) = super::diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            super::DiffIndexToWorktreeOptions {
                repository_git_dir: Some(git_dir),
                for_status: true,
                ..Default::default()
            },
        )
        .expect("diff");
        metrics.set_blob_counting(false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, super::DiffStatus::Modified);
        assert_eq!(entries[0].path(), rel);
        assert_eq!(entries[0].old_oid, index.entries[0].oid);
        assert_eq!(entries[0].new_oid, super::zero_oid());
        assert_eq!(
            metrics.blob_content_reads(),
            0,
            "identity conversion: a size change alone proves the file modified"
        );
    }

    #[test]
    fn status_diff_hashes_size_change_when_eol_conversion_restores_blob() {
        for refresh_index_stat_in_pass in [false, true] {
            let td = tempfile::tempdir().expect("tempdir");
            let wt = td.path();
            let (git_dir, odb) = minimal_repo(wt, "[core]\n");
            fs::write(wt.join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
            let rel = "n.txt";
            let blob = odb
                .write(ObjectKind::Blob, b"one\ntwo\n")
                .expect("write blob");
            let mut index = Index::new();
            index.entries.push(staged_entry_with_oid(
                wt,
                rel,
                b"one\r\ntwo\r\n",
                blob,
                (100, 0),
            ));
            fs::write(wt.join(rel), b"one\ntwo\n").expect("rewrite as LF");
            let (entries, index_changed) = super::diff_index_to_worktree_with_options(
                &odb,
                &mut index,
                wt,
                super::DiffIndexToWorktreeOptions {
                    index_mtime: Some((200, 0)),
                    repository_git_dir: Some(git_dir),
                    refresh_index_stat_in_pass,
                    for_status: true,
                    ..Default::default()
                },
            )
            .expect("diff");
            assert!(
                entries.is_empty(),
                "refresh={refresh_index_stat_in_pass}: LF worktree cleans to the indexed blob: \
                 {entries:?}"
            );
            if refresh_index_stat_in_pass {
                assert!(index_changed, "verified entry adopts the new stat");
                assert_eq!(index.entries[0].size, 8);
            }
        }
    }

    #[test]
    fn status_diff_reads_size_changed_converted_file_once() {
        for refresh_index_stat_in_pass in [false, true] {
            let td = tempfile::tempdir().expect("tempdir");
            let wt = td.path();
            let (git_dir, odb) = minimal_repo(wt, "[core]\n");
            fs::write(wt.join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
            let rel = "n.txt";
            let blob = odb
                .write(ObjectKind::Blob, b"one\ntwo\n")
                .expect("write blob");
            let mut index = Index::new();
            index.entries.push(staged_entry_with_oid(
                wt,
                rel,
                b"one\r\ntwo\r\n",
                blob,
                (100, 0),
            ));
            fs::write(wt.join(rel), b"one\ntwo\nthree\n").expect("real edit");
            let metrics = odb.hot_path_metrics();
            metrics.reset_blob_content_reads();
            metrics.set_blob_counting(true);
            let (entries, _) = super::diff_index_to_worktree_with_options(
                &odb,
                &mut index,
                wt,
                super::DiffIndexToWorktreeOptions {
                    index_mtime: Some((200, 0)),
                    repository_git_dir: Some(git_dir),
                    refresh_index_stat_in_pass,
                    for_status: true,
                    ..Default::default()
                },
            )
            .expect("diff");
            metrics.set_blob_counting(false);
            assert_eq!(entries.len(), 1, "refresh={refresh_index_stat_in_pass}");
            assert_eq!(entries[0].status, super::DiffStatus::Modified);
            assert_eq!(entries[0].new_oid, zero_oid());
            assert_eq!(
                index.entries[0].size, 10,
                "a modified file keeps its stale stat"
            );
            assert_eq!(
                metrics.blob_content_reads(),
                1,
                "refresh={refresh_index_stat_in_pass}: read once, not again after the refresh"
            );
        }
    }

    #[test]
    fn stat_matches_ignores_subsecond_nsec_by_default() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        fs::write(wt.join("f.txt"), b"x\n").expect("write");
        pin_mtime_and_sample(&wt.join("f.txt"), 100, 0);
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        let mut entry = entry_from_metadata(&meta, b"f.txt", zero_oid(), MODE_REGULAR);
        entry.mtime_nsec = entry.mtime_nsec.wrapping_add(1_000_000);
        assert!(stat_matches(&entry, &meta));
    }

    #[test]
    fn classify_add_up_to_date_when_stat_matches() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (_git_dir, odb) = minimal_repo(wt, "[core]\n");
        let bytes = b"hello world\n";
        let entry = staged_entry_for_file(&odb, wt, "f.txt", bytes, (100, 0));
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        let conv = crlf::ConversionConfig::from_config(&ConfigSet::new());
        let attrs = FileAttrs::default();
        let params = WorktreeAddRefreshParams {
            odb: &odb,
            ie: &entry,
            meta: &meta,
            abs_path: &wt.join("f.txt"),
            rel_path: "f.txt",
            conv: &conv,
            file_attrs: &attrs,
            index_mtime: Some((200, 0)),
            staged_mode: MODE_REGULAR,
            filter_process: None,
        };
        let refresh = classify_worktree_entry_for_add(&params).expect("classify");
        assert_eq!(refresh, WorktreeAddRefresh::UpToDate);
    }

    #[test]
    fn classify_add_rehashes_when_index_mtime_unknown_and_stat_matches() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (_git_dir, odb) = minimal_repo(wt, "[core]\n");
        let entry = staged_entry_for_file(&odb, wt, "f.txt", b"old content\n", (100, 0));
        fs::write(wt.join("f.txt"), b"new content\n").expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), 100, 0);
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        let conv = crlf::ConversionConfig::from_config(&ConfigSet::new());
        let attrs = FileAttrs::default();
        let params = WorktreeAddRefreshParams {
            odb: &odb,
            ie: &entry,
            meta: &meta,
            abs_path: &wt.join("f.txt"),
            rel_path: "f.txt",
            conv: &conv,
            file_attrs: &attrs,
            index_mtime: None,
            staged_mode: MODE_REGULAR,
            filter_process: None,
        };
        let refresh = classify_worktree_entry_for_add(&params).expect("classify");
        assert_eq!(refresh, WorktreeAddRefresh::NeedsRestage);
    }

    #[test]
    fn classify_add_needs_restaging_when_content_differs() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (_git_dir, odb) = minimal_repo(wt, "[core]\n");
        let entry = staged_entry_for_file(&odb, wt, "f.txt", b"old content\n", (100, 0));
        fs::write(wt.join("f.txt"), b"new content\n").expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), 101, 0);
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        let conv = crlf::ConversionConfig::from_config(&ConfigSet::new());
        let attrs = FileAttrs::default();
        let params = WorktreeAddRefreshParams {
            odb: &odb,
            ie: &entry,
            meta: &meta,
            abs_path: &wt.join("f.txt"),
            rel_path: "f.txt",
            conv: &conv,
            file_attrs: &attrs,
            index_mtime: Some((200, 0)),
            staged_mode: MODE_REGULAR,
            filter_process: None,
        };
        let refresh = classify_worktree_entry_for_add(&params).expect("classify");
        assert_eq!(refresh, WorktreeAddRefresh::NeedsRestage);
    }

    #[test]
    fn smudges_racily_clean_modified_entry() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let content_a = b"same length!!";
        let content_b = b"same length??";
        assert_eq!(content_a.len(), content_b.len());

        let mut index = Index::new();
        let entry = staged_entry_for_file(&odb, wt, "f.txt", content_a, index_mtime);
        let stale_oid = entry.oid;

        fs::write(wt.join("f.txt"), content_b).expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), index_mtime.0, index_mtime.1);
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        // Stat was refreshed from the worktree but the blob OID is still stale (racy window).
        let entry = entry_from_metadata(&meta, b"f.txt", stale_oid, MODE_REGULAR);
        index.entries.push(entry.clone());

        assert!(stat_matches(&entry, &meta));
        assert!(entry_is_racy(&entry, Some(index_mtime)));

        let smudged = smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        );
        assert!(smudged);
        assert_eq!(index.entries[0].size, 0);

        let diff = diff_index_to_worktree(&odb, &mut index, wt, false, false).expect("diff");
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].path(), "f.txt");
    }

    /// A racy index row plus the git dir and odb it lives in; the worktree no longer matches.
    fn racy_stale_entry(wt: &Path) -> (PathBuf, Odb, Index, (u32, u32), ObjectId) {
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let entry = staged_entry_for_file(&odb, wt, "f.txt", b"same length!!", index_mtime);
        let stale_oid = entry.oid;
        fs::write(wt.join("f.txt"), b"same length??").expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), index_mtime.0, index_mtime.1);
        let meta = fs::symlink_metadata(wt.join("f.txt")).expect("stat");
        let mut index = Index::new();
        index.entries.push(entry_from_metadata(
            &meta,
            b"f.txt",
            stale_oid,
            MODE_REGULAR,
        ));
        (git_dir, odb, index, index_mtime, stale_oid)
    }

    #[test]
    fn content_verified_entry_is_not_reread_and_the_mark_is_consumed() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb, mut index, index_mtime, stale_oid) = racy_stale_entry(wt);
        index.mark_content_verified(b"f.txt", stale_oid);

        let smudge = |index: &mut Index| {
            smudge_racily_clean_entries(&odb, &git_dir, index, wt, Some(index_mtime), None, None)
        };
        assert!(!smudge(&mut index), "a verified entry is trusted");
        assert_ne!(index.entries[0].size, 0);
        assert!(index.content_verified.is_empty());
        assert!(smudge(&mut index), "without the mark the entry is checked");
        assert_eq!(index.entries[0].size, 0);
    }

    #[test]
    fn content_verified_entry_bit_is_trusted_and_consumed() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb, mut index, index_mtime, _) = racy_stale_entry(wt);
        index.entries[0].mark_content_verified();

        let smudge = |index: &mut Index| {
            smudge_racily_clean_entries(&odb, &git_dir, index, wt, Some(index_mtime), None, None)
        };
        assert!(!smudge(&mut index), "a marked entry is trusted");
        assert_ne!(index.entries[0].size, 0);
        assert_eq!(
            index.entries[0].flags_extended, None,
            "the mark is consumed"
        );
        assert!(smudge(&mut index), "without the mark the entry is checked");
        assert_eq!(index.entries[0].size, 0);
    }

    #[test]
    fn content_verified_mark_for_another_oid_does_not_skip_the_check() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb, mut index, index_mtime, _) = racy_stale_entry(wt);
        let other = odb.hash(ObjectKind::Blob, b"other").expect("hash");
        index.mark_content_verified(b"f.txt", other);
        assert!(smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        ));
        assert_eq!(index.entries[0].size, 0);
    }

    #[test]
    fn leaves_non_racy_entry_alone() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let content_a = b"same length!!";
        let content_b = b"same length??";
        let entry_mtime = (index_mtime.0 - 10, 0);

        let mut index = Index::new();
        let entry = staged_entry_for_file(&odb, wt, "f.txt", content_a, entry_mtime);
        index.entries.push(entry.clone());

        fs::write(wt.join("f.txt"), content_b).expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), entry_mtime.0, entry_mtime.1);

        assert!(!entry_is_racy(&entry, Some(index_mtime)));
        let smudged = smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        );
        assert!(!smudged);
        assert_eq!(index.entries[0].size, entry.size);
    }

    #[test]
    fn leaves_matching_content_alone() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let bytes = b"unchanged!!!!";

        let mut index = Index::new();
        let entry = staged_entry_for_file(&odb, wt, "f.txt", bytes, index_mtime);
        index.entries.push(entry.clone());

        assert!(entry_is_racy(&entry, Some(index_mtime)));
        let smudged = smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        );
        assert!(!smudged);
        assert_eq!(index.entries[0].size, entry.size);
    }

    #[test]
    fn status_diff_skips_refresh_rewrite_for_racy_clean_matching_stat() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let bytes = b"racy clean payload\n";

        let mut index = Index::new();
        index.source_mtime = Some(index_mtime);
        let entry = staged_entry_for_file(&odb, wt, "f.txt", bytes, index_mtime);
        index.entries.push(entry.clone());
        assert!(entry_is_racy(&entry, Some(index_mtime)));

        let (entries, index_changed) = super::diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            super::DiffIndexToWorktreeOptions {
                repository_git_dir: Some(git_dir),
                index_mtime: Some(index_mtime),
                refresh_index_stat_in_pass: true,
                for_status: true,
                ..Default::default()
            },
        )
        .expect("status diff");
        assert!(entries.is_empty(), "unexpected unstaged: {entries:?}");
        assert!(
            !index_changed,
            "matching stat on a racy entry must skip refresh without rewriting the index"
        );
    }

    #[test]
    fn leaves_matching_sha256_racy_content_alone() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[extensions]\n\tobjectformat = sha256\n");
        assert_eq!(odb.hash_algo(), HashAlgo::Sha256);

        let bytes = b"sha256 width!!";
        let oid = odb.hash(ObjectKind::Blob, bytes).expect("hash");
        let mut index = Index::new();
        let entry = staged_entry_with_oid(wt, "f.txt", bytes, oid, index_mtime);
        index.entries.push(entry.clone());

        assert!(entry_is_racy(&entry, Some(index_mtime)));
        let smudged = smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        );
        assert!(!smudged, "matching SHA-256 blob must not be smudged");
        assert_eq!(index.entries[0].size, entry.size);
    }

    #[test]
    fn materialize_big_binary_under_autocrlf_streams_into_pack() {
        use std::io::Write;

        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (git_dir, odb) =
            minimal_repo(wt, "[core]\n\tautocrlf = true\n\tbigFileThreshold = 64k\n");
        let path = wt.join("big.bin");
        {
            let mut f = fs::File::create(&path).expect("create");
            f.write_all(&[0u8; 9000]).expect("nul prefix");
            let chunk = vec![1u8; 256 * 1024];
            for _ in 0..20 {
                f.write_all(&chunk).expect("chunk");
            }
        }
        let meta = fs::metadata(&path).expect("stat");
        let mut conv = crlf::ConversionConfig::from_config(&ConfigSet::new());
        conv.autocrlf = crlf::AutoCrlf::True;
        let attrs = FileAttrs::default();
        let oid =
            materialize_worktree_blob(&odb, &path, &meta, &conv, &attrs, "big.bin", None, None)
                .expect("materialize");
        assert!(
            !odb.test_loose_object_path(&oid).exists(),
            "big binary under autocrlf should pack, not write loose"
        );
        let pack_dir = git_dir.join("objects/pack");
        let names: Vec<_> = fs::read_dir(&pack_dir)
            .expect("pack dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(
            names.iter().any(|n| n.to_string_lossy().ends_with(".pack")),
            "{names:?}"
        );
        assert!(odb.read(&oid).expect("read blob").data.len() > 9000);
    }

    #[test]
    fn indexed_binary_under_autocrlf_rehash_streams() {
        use std::io::Write;

        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let (_git_dir, odb) =
            minimal_repo(wt, "[core]\n\tautocrlf = true\n\tbigFileThreshold = 1m\n");
        let path = wt.join("big.bin");
        {
            let mut f = fs::File::create(&path).expect("create");
            f.write_all(&[0u8; 9000]).expect("nul prefix");
            let chunk = vec![1u8; 512 * 1024];
            for _ in 0..3 {
                f.write_all(&chunk).expect("chunk");
            }
        }
        let meta = fs::symlink_metadata(&path).expect("stat");
        let oid = hash_worktree_file(
            &odb,
            &path,
            &meta,
            &crlf::ConversionConfig::from_config(&ConfigSet::new()),
            &FileAttrs::default(),
            "big.bin",
            None,
            None,
        )
        .expect("hash");
        let entry = entry_from_metadata(&meta, b"big.bin", oid, MODE_REGULAR);
        let mut conv = crlf::ConversionConfig::from_config(&ConfigSet::new());
        conv.autocrlf = crlf::AutoCrlf::True;
        let attrs = FileAttrs::default();
        let content = read_regular_worktree_blob(
            &odb,
            &path,
            &conv,
            &attrs,
            "big.bin",
            Some(&entry),
            None,
            1024 * 1024,
        )
        .expect("read blob");
        assert!(
            matches!(content, WorktreeBlobContent::Stream { .. }),
            "indexed binary re-hash must stream, not buffer"
        );
        assert_eq!(content.hash(&odb).expect("hash"), entry.oid);
    }

    #[test]
    fn leaves_matching_crlf_worktree_content_alone() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n\tautocrlf = true\n");
        let git_form = b"line one\nline two\n";
        let worktree_form = b"line one\r\nline two\r\n";
        let oid = odb.hash(ObjectKind::Blob, git_form).expect("hash");

        let mut index = Index::new();
        let entry = staged_entry_with_oid(wt, "f.txt", worktree_form, oid, index_mtime);
        index.entries.push(entry.clone());

        assert!(entry_is_racy(&entry, Some(index_mtime)));
        let smudged = smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        );
        assert!(
            !smudged,
            "CRLF worktree matching clean index OID must not be smudged"
        );
        assert_eq!(index.entries[0].size, entry.size);
    }

    #[test]
    fn skips_skip_worktree_and_intent_to_add() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let content_a = b"same length!!";
        let content_b = b"same length??";

        let mut index = Index::new();
        let mut entry = staged_entry_for_file(&odb, wt, "f.txt", content_a, index_mtime);
        entry.set_skip_worktree(true);
        index.entries.push(entry);
        fs::write(wt.join("f.txt"), content_b).expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), index_mtime.0, index_mtime.1);
        assert!(!smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        ));
        assert_ne!(index.entries[0].size, 0);

        let mut index = Index::new();
        let mut entry = staged_entry_for_file(&odb, wt, "f.txt", content_a, index_mtime);
        entry.set_intent_to_add(true);
        index.entries.push(entry);
        fs::write(wt.join("f.txt"), content_b).expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), index_mtime.0, index_mtime.1);
        assert!(!smudge_racily_clean_entries(
            &odb,
            &git_dir,
            &mut index,
            wt,
            Some(index_mtime),
            None,
            None,
        ));
        assert_ne!(index.entries[0].size, 0);
    }

    #[test]
    fn smudges_nothing_without_prior_index_mtime() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        let index_mtime = setup_index_mtime(wt);
        let (git_dir, odb) = minimal_repo(wt, "[core]\n");
        let mut index = Index::new();
        let entry = staged_entry_for_file(&odb, wt, "f.txt", b"same length!!", index_mtime);
        index.entries.push(entry);

        fs::write(wt.join("f.txt"), b"same length??").expect("rewrite");
        pin_mtime_and_sample(&wt.join("f.txt"), index_mtime.0, index_mtime.1);

        let smudged = smudge_racily_clean_entries(&odb, &git_dir, &mut index, wt, None, None, None);
        assert!(!smudged);
        assert_ne!(index.entries[0].size, 0);
    }
}

#[cfg(all(test, unix))]
mod read_worktree_bytes_for_mode_tests {
    use super::*;
    use crate::index::MODE_SYMLINK;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn symlink_mode_reads_link_target_not_followed_file() {
        let td = tempfile::tempdir().expect("tempdir");
        let wt = td.path();
        fs::write(wt.join("payload.txt"), b"secret\n").expect("write");
        symlink("payload.txt", wt.join("link")).expect("symlink");
        let bytes =
            read_worktree_bytes_for_mode(&wt.join("link"), MODE_SYMLINK).expect("read link");
        assert_eq!(bytes, b"payload.txt");
    }

    #[test]
    fn symlink_mode_reads_plain_file_holding_the_target() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("link");
        fs::write(&path, b"payload.txt").expect("write");
        let bytes = read_worktree_bytes_for_mode(&path, MODE_SYMLINK).expect("read plain link");
        assert_eq!(bytes, b"payload.txt");
    }

    #[test]
    fn regular_mode_reads_file_contents() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("f.txt");
        fs::write(&path, b"hello\n").expect("write");
        let bytes = read_worktree_bytes_for_mode(&path, crate::index::MODE_REGULAR).expect("read");
        assert_eq!(bytes, b"hello\n");
    }

    #[test]
    fn annotate_marks_context_line_when_eof_line_is_unchanged() {
        let old = "a\nb\nc";
        let new = "a\nx\nc";
        let raw = super::unified_diff_histogram_hunks_only(old, new, 3, 0, false);
        let out = annotate_unified_diff_no_newline_at_eof(&raw, old, new);
        assert!(
            out.contains(" c\n\\ No newline at end of file"),
            "histogram hunk should mark trailing context line:\n{out}"
        );
    }

    #[test]
    fn annotate_does_not_treat_removed_dash_lines_as_file_headers() {
        use super::unified_diff_with_prefix_and_funcname_and_algorithm;
        use similar::Algorithm;

        let old = "--old";
        let new = "--new";
        let raw = unified_diff_with_prefix_and_funcname_and_algorithm(
            old,
            new,
            "a/x",
            "b/x",
            3,
            0,
            "",
            "",
            None,
            Algorithm::Myers,
            false,
            false,
            false,
            false,
        );
        assert_eq!(
            raw.matches("\\ No newline at end of file").count(),
            2,
            "expected two markers like git, got:\n{raw}"
        );
    }
}

#[cfg(test)]
mod annotate_no_newline_tests {
    use super::annotate_unified_diff_no_newline_at_eof;

    #[test]
    fn annotate_marks_both_sides_without_trailing_newline() {
        let raw = "@@ -1 +1 @@\n-foo\n+f o o\n";
        let out = annotate_unified_diff_no_newline_at_eof(raw, "foo", "f o o");
        assert!(out.contains("\\ No newline at end of file"));
        assert_eq!(out.matches("\\ No newline at end of file").count(), 2);
    }
}

#[cfg(test)]
mod blob_diff_algorithm_smoke {
    use super::{
        anchored_unified_diff, count_changes_with_algorithm, unified_diff_histogram_hunks_only,
        unified_diff_with_prefix_and_funcname_and_algorithm, unified_hunk_body_for_diff_options,
    };
    use similar::Algorithm;

    const OLD: &str = "alpha\nbeta\ngamma\n";
    const NEW: &str = "alpha\nBETA\ngamma\ndelta\n";

    #[test]
    fn seed15_histogram_u0_body_matches_git() {
        use crate::diff::options::{DiffAlgorithm, DiffOptions};
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let seed = 15u64;
        let mut hasher = DefaultHasher::new();
        seed.hash(&mut hasher);
        let h = hasher.finish();
        let line_count = 4 + (h as usize % 12);
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..line_count {
            let token = (h.wrapping_add(i as u64 * 131) % 7) as u8 + b'a';
            let ch = token as char;
            old.push(ch);
            if (h >> (i % 8)) & 1 == 1 {
                new.push(((token as u8).wrapping_add(1)) as char);
            } else {
                new.push(ch);
            }
            old.push('\n');
            new.push('\n');
        }
        let opts = DiffOptions {
            context_lines: 0,
            algorithm: DiffAlgorithm::Histogram,
            indent_heuristic: false,
            ..DiffOptions::default()
        };
        let body = unified_hunk_body_for_diff_options(&old, &new, &opts);
        assert_eq!(
            body.matches("@@ ").count(),
            2,
            "git 2.55 splits u0 histogram hunks at seed 15: {body}"
        );
    }

    #[test]
    fn t4071_histogram_changed_flags_git_myers_completes() {
        let old: Vec<&str> = "x\nx\nx\nx\n".lines().collect();
        let new: Vec<&str> = "x\nx\nx\nA\nB\nC\nD\nx\nE\nF\nG\n".lines().collect();
        let (c1, c2) = super::git_xdiff::histogram_changed_flags(&old, &new);
        assert_eq!(c1.len(), 4);
        assert_eq!(c2.len(), 11);
        let ops = super::changed_flags_to_ops(&c1, &c2, old.len(), new.len());
        assert!(!ops.is_empty());
    }

    #[test]
    fn t4071_histogram_compacted_ops_completes() {
        use crate::diff::WhitespaceMode;
        let old = "x\nx\nx\nx\n";
        let new = "x\nx\nx\nA\nB\nC\nD\nx\nE\nF\nG\n";
        let ops = super::histogram_compacted_line_ops(old, new, false, WhitespaceMode::default());
        assert!(!ops.is_empty(), "expected diff ops");
    }

    #[test]
    fn t4071_histogram_u0_body_completes() {
        use crate::diff::options::{DiffAlgorithm, DiffOptions};
        let old = "x\nx\nx\nx\n";
        let new = "x\nx\nx\nA\nB\nC\nD\nx\nE\nF\nG\n";
        let opts = DiffOptions {
            context_lines: 0,
            algorithm: DiffAlgorithm::Histogram,
            indent_heuristic: false,
            ..DiffOptions::default()
        };
        let body = unified_hunk_body_for_diff_options(old, new, &opts);
        assert!(body.contains("@@"), "expected hunk header: {body}");
    }

    #[test]
    fn t4071_minimal_u0_matches_git_hunk_headers() {
        use crate::diff::options::{DiffAlgorithm, DiffOptions};
        let old = "x\nx\nx\nx\n";
        let new = "x\nx\nx\nA\nB\nC\nD\nx\nE\nF\nG\n";
        let opts = DiffOptions {
            context_lines: 0,
            algorithm: DiffAlgorithm::Minimal,
            indent_heuristic: false,
            ..DiffOptions::default()
        };
        let body = unified_hunk_body_for_diff_options(old, new, &opts);
        assert_eq!(
            body.matches("@@ ").count(),
            2,
            "git 2.55+ splits u0 minimal hunks at repeated anchors: {body}"
        );
    }

    #[test]
    fn hunks_do_not_slide_onto_final_line_without_newline() {
        use crate::diff::options::{DiffAlgorithm, DiffOptions};

        let old = "{\n  \"a\": 1\n}\n";
        let new = "{\n  \"a\": 2\n}\n{\n  \"b\": 2\n}";
        let expected = "@@ -1,3 +1,6 @@\n {\n-  \"a\": 1\n+  \"a\": 2\n }\n+{\n+  \"b\": 2\n+}\n\\ No newline at end of file\n";
        for algorithm in [
            DiffAlgorithm::Myers,
            DiffAlgorithm::Minimal,
            DiffAlgorithm::Patience,
            DiffAlgorithm::Histogram,
        ] {
            for indent_heuristic in [true, false] {
                let options = DiffOptions {
                    algorithm,
                    indent_heuristic,
                    ..DiffOptions::default()
                };
                assert_eq!(
                    unified_hunk_body_for_diff_options(old, new, &options),
                    expected,
                    "{algorithm:?}, indent heuristic {indent_heuristic}"
                );
            }
        }
    }

    #[test]
    fn myers_patience_histogram_minimal_emit_unified_hunks() {
        for algo in [Algorithm::Myers, Algorithm::Patience, Algorithm::Histogram] {
            let patch = unified_diff_with_prefix_and_funcname_and_algorithm(
                OLD,
                NEW,
                "a/old.txt",
                "b/new.txt",
                3,
                0,
                "a/",
                "b/",
                None,
                algo,
                false,
                false,
                true,
                false,
            );
            assert!(patch.contains("@@"), "algo {algo:?} should emit hunks");
            assert!(patch.contains("-beta") || patch.contains("-BETA"));
        }
    }

    #[test]
    fn git_histogram_printer_and_anchored_diff_smoke() {
        let hist = unified_diff_histogram_hunks_only(OLD, NEW, 3, 0, false);
        assert!(!hist.is_empty());
        let anchors = vec!["beta".to_string()];
        let anchored = anchored_unified_diff(
            OLD,
            NEW,
            "old",
            "new",
            3,
            &anchors,
            Algorithm::Myers,
            false,
            true,
            false,
        );
        assert!(anchored.contains("@@"));
    }

    #[test]
    fn count_changes_git_histogram_vs_similar() {
        let hist = count_changes_with_algorithm(OLD, NEW, Algorithm::Myers, true);
        let myers = count_changes_with_algorithm(OLD, NEW, Algorithm::Myers, false);
        assert!(hist.0 + hist.1 > 0);
        assert!(myers.0 + myers.1 > 0);
    }
}

#[cfg(test)]
mod line_coverage_tests {
    use super::*;
    use crate::config::ConfigSet;
    use crate::index::StatMatchPolicy;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::parallel::ThreadPolicy;
    use crate::write_tree::write_tree_from_index;
    use similar::Algorithm;
    use tempfile::TempDir;

    fn index_entry(path: &str, mode: u32, oid: ObjectId) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    fn write_tree(odb: &Odb, entries: &[(&str, ObjectId)]) -> ObjectId {
        let mut body = Vec::new();
        for (name, oid) in entries {
            body.extend_from_slice(format!("100644 {name}\0").as_bytes());
            body.extend_from_slice(oid.as_bytes());
        }
        odb.write(ObjectKind::Tree, &body).expect("tree")
    }

    fn pair_of_trees_with_rename() -> (TempDir, Odb, ObjectId, ObjectId) {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let old_blob = odb
            .write(ObjectKind::Blob, b"line one\nline two\n")
            .expect("old");
        let new_blob = odb
            .write(ObjectKind::Blob, b"line one\nline TWO\nline three\n")
            .expect("new");
        let old_tree = write_tree(&odb, &[("old.txt", old_blob)]);
        let new_tree = write_tree(&odb, &[("new.txt", new_blob), ("kept.txt", old_blob)]);
        (temp, odb, old_tree, new_tree)
    }

    #[test]
    fn detect_copies_reports_source_modes_without_tree_entries() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let content = odb
            .write(ObjectKind::Blob, b"shared content\n")
            .expect("blob");
        let changed = odb.write(ObjectKind::Blob, b"changed\n").expect("blob");
        let old_tree = write_tree(&odb, &[("gone.txt", content), ("mod.txt", content)]);
        let new_tree = write_tree(
            &odb,
            &[
                ("copy.txt", content),
                ("mod.txt", changed),
                ("moved.txt", content),
            ],
        );
        let entries = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "").expect("diff");
        let result = detect_copies(&odb, None, entries, 50, false, &[]).expect("copies");
        let pairs: Vec<_> = result
            .iter()
            .filter(|e| matches!(e.status, DiffStatus::Renamed | DiffStatus::Copied))
            .map(|e| (e.old_mode, e.new_mode))
            .collect();
        assert_eq!(pairs.len(), 2, "{result:?}");
        assert!(
            pairs
                .iter()
                .all(|p| *p == (FileMode::REGULAR, FileMode::REGULAR)),
            "{pairs:?}"
        );
    }

    #[test]
    fn detect_copies_keeps_modifications_as_modifications() {
        // `git diff -C` reports `b` as modified even though its new content equals the
        // old content of the also-modified `a`: only created paths become copies.
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let one = odb.write(ObjectKind::Blob, b"1\n").expect("blob");
        let two = odb.write(ObjectKind::Blob, b"2\n").expect("blob");
        let old_tree = write_tree(&odb, &[("a", one), ("b", two)]);
        let new_tree = write_tree(&odb, &[("a", two), ("b", one)]);
        let entries = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "").expect("diff");
        let result = detect_copies(&odb, None, entries, 50, false, &[]).expect("copies");
        let statuses: Vec<_> = result.iter().map(|e| (e.path(), e.status)).collect();
        assert_eq!(
            statuses,
            [("a", DiffStatus::Modified), ("b", DiffStatus::Modified)]
        );
    }

    #[test]
    fn diff_helper_and_format_sweep() {
        let cfg = ConfigSet::new();
        assert!(resolve_indent_heuristic(&cfg, false, false));
        assert!(!resolve_indent_heuristic(&cfg, false, true));

        let old = "alpha beta\ngamma\n";
        let new = "alpha BETA\ngamma delta\n";
        let _ = normalize_ignore_space_change(old);
        let _ = normalize_ignore_space_change_line("- alpha  beta ");
        let _ = count_git_lines(old.as_bytes());
        let _ = diffcore_count_changes(old.as_bytes(), new.as_bytes());
        assert!(parse_diff_rename_score_token("50%").is_some());
        let _ = rewrite_merge_score(b"a", b"b");
        let _ = rewrite_dissimilarity_index_percent(b"abc", b"abd");
        assert_eq!(format_mode(0o100644), "100644");

        let old_lines = ["a", "b", "c"];
        let new_lines = ["a", "b", "d"];
        let compact = diff_slice_ops_compacted(&old_lines, &new_lines, Algorithm::Myers, false);
        assert!(!compact.is_empty());
    }

    #[test]
    fn tree_diff_rename_copy_postprocess_matrix() -> Result<()> {
        let (_temp, odb, old_tree, new_tree) = pair_of_trees_with_rename();
        let raw = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "")?;
        assert!(!raw.is_empty());
        let with_renames = diff_trees_with_renames(&odb, Some(&old_tree), Some(&new_tree), "", 50)?;
        assert!(!with_renames.is_empty());
        let config = ConfigSet::new();
        let policy = ThreadPolicy::default();
        let detected =
            detect_renames_with_policy(&odb, None, raw.clone(), 50, &config, &policy, None)?;
        let source_flat = flatten_tree_entries(&odb, &old_tree)?;
        let _ = detect_copies_with_policy(
            &odb,
            None,
            detected.clone(),
            50,
            true,
            &source_flat,
            &config,
            &policy,
        )?;
        let opts = DiffPostprocessOptions {
            break_score: Some(50),
            rename_threshold: 50,
            copy_threshold: Some(50),
            find_copies_harder: true,
            materialize_break_worktree_new_oid: false,
        };
        let processed = postprocess_diff_entries(
            &odb,
            raw,
            Some(&old_tree),
            None,
            None,
            &config,
            &policy,
            &opts,
        )?;
        assert!(!processed.is_empty());
        let _ = status_apply_rename_copy_detection(
            &odb,
            with_renames.clone(),
            50,
            true,
            Some(&old_tree),
            &config,
            &policy,
            None,
        )?;
        let flat = flatten_tree_entries(&odb, &new_tree)?;
        assert!(!flat.is_empty());
        let _ = diff_trees_show_tree_entries(&odb, Some(&old_tree), Some(&new_tree), "")?;
        Ok(())
    }

    #[test]
    fn unified_diff_flag_matrix() {
        let old = "fn main() {\n    println!(\"hi\");\n}\n";
        let new = "fn main() {\n    println!(\"bye\");\n}\n";
        for algo in [Algorithm::Myers, Algorithm::Patience, Algorithm::Histogram] {
            for (ws, ws_all, fc, ig) in [(true, false, false, false), (false, true, true, true)] {
                let patch = unified_diff_with_prefix_and_funcname_and_algorithm(
                    old,
                    new,
                    "a/main.rs",
                    "b/main.rs",
                    3,
                    0,
                    "a/",
                    "b/",
                    None,
                    algo,
                    ws,
                    ws_all,
                    fc,
                    ig,
                );
                assert!(patch.contains("@@"), "algo {algo:?}");
            }
        }
        let hist = unified_diff_histogram_ignore_with_prefix_and_funcname(
            old,
            new,
            "a/x",
            "b/x",
            3,
            0,
            "",
            "",
            None,
            false,
            false,
            false,
            |_old, _new| false,
        );
        assert!(!hist.is_empty());
        let raw = format_raw_abbrev(
            &DiffEntry {
                status: DiffStatus::Modified,
                old_path: Some("x".into()),
                new_path: Some("x".into()),
                old_mode: FileMode::REGULAR,
                new_mode: FileMode::REGULAR,
                old_oid: empty_blob_oid(),
                new_oid: empty_blob_oid(),
                score: Some(80),
            },
            7,
        );
        assert!(raw.contains('M') || raw.contains('x'));
    }

    #[test]
    fn index_and_commit_diff_paths() -> Result<()> {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let b1 = odb.write(ObjectKind::Blob, b"v1\n").expect("b1");
        let b2 = odb.write(ObjectKind::Blob, b"v2\n").expect("b2");
        let b3 = odb.write(ObjectKind::Blob, b"v3\n").expect("b3");
        let mut index = Index::new();
        index.add_or_replace(index_entry("file.txt", MODE_REGULAR, b2));
        let head = write_tree_from_index(&odb, &index, "").expect("tree");
        index.entries_mut()[0].oid = b1;
        let idx_diff = diff_index_to_tree(&odb, &index, Some(&head), false)?;
        assert!(!idx_diff.is_empty());
        let commit_body = format!(
            "tree {head}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nm\n"
        );
        let c1 = odb
            .write(ObjectKind::Commit, commit_body.as_bytes())
            .expect("c1");
        let mut index2 = Index::new();
        index2.add_or_replace(index_entry("file.txt", MODE_REGULAR, b3));
        let head2 = write_tree_from_index(&odb, &index2, "").expect("head2");
        let c2_body = format!(
            "tree {head2}\nparent {c1}\nauthor T <t@e.com> 2 +0000\ncommitter T <t@e.com> 2 +0000\n\nm2\n"
        );
        let c2 = odb
            .write(ObjectKind::Commit, c2_body.as_bytes())
            .expect("c2");
        let commit_diff = diff_commits_with_renames(&odb, &c1, &c2, 50)?;
        assert!(!commit_diff.is_empty());
        let _ = tree_oid_of_commit(&odb, &c2)?;
        let mtime = index_mtime_for_diff(&index, Some((1, 0)));
        let _ = entry_is_racy(&index.entries()[0], mtime);
        let _ = entry_is_racy_with_policy(&index.entries()[0], mtime, StatMatchPolicy::default());
        Ok(())
    }

    #[test]
    fn tree_oid_of_commit_reports_invalid_ref_for_non_commits() {
        let temp = TempDir::new().expect("tempdir");
        let odb = Odb::new(temp.path());
        let tree = odb.write(ObjectKind::Tree, &[]).expect("tree");
        let body = format!(
            "tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nm\n"
        );
        let commit = odb
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
        let blob = odb.write(ObjectKind::Blob, b"x\n").expect("blob");

        assert_eq!(tree_oid_of_commit(&odb, &commit).expect("tree"), tree);
        assert!(matches!(
            tree_oid_of_commit(&odb, &blob),
            Err(Error::InvalidRef(_))
        ));
        assert!(matches!(
            tree_oid_of_commit(&odb, &tree),
            Err(Error::InvalidRef(_))
        ));
    }

    #[test]
    fn worktree_index_diff_and_rename_score() -> Result<()> {
        let temp = TempDir::new().expect("tempdir");
        let wt = temp.path();
        let git_dir = wt.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        let objects_dir = git_dir.join("objects");
        let odb = Odb::new(&objects_dir);
        let content = b"hello world\n";
        std::fs::write(wt.join("tracked.txt"), content).expect("write");
        let oid = odb.write(ObjectKind::Blob, content).expect("blob");
        let mut index = Index::new();
        index.add_or_replace(index_entry("tracked.txt", MODE_REGULAR, oid));
        let head = write_tree_from_index(&odb, &index, "").expect("tree");
        std::fs::write(wt.join("tracked.txt"), b"hello WORLD\n").expect("mutate");
        let mut index_mut = index.clone();
        let diff = diff_index_to_worktree(&odb, &mut index_mut, wt, false, false)?;
        assert!(!diff.is_empty());
        let _ = diff_tree_to_worktree(&odb, Some(&head), wt, &index)?;
        assert_eq!(rename_similarity_score(b"a", b"a\n"), 0);
        assert_eq!(rename_similarity_score(b"a\n", b"a"), 0);
        let score = rename_similarity_score(b"line one\nline two\n", b"line one\nline TWO\n");
        assert!(score >= 50);
        let _ = detect_renames(&odb, Some(wt), diff, 50);
        Ok(())
    }

    #[test]
    fn extended_diff_helpers_sweep() -> Result<()> {
        use crate::crlf::{ConversionConfig, FileAttrs};
        let temp = TempDir::new().expect("tempdir");
        let wt = temp.path();
        let git_dir = wt.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        let objects_dir = git_dir.join("objects");
        let odb = Odb::new(&objects_dir);
        let b_old = odb.write(ObjectKind::Blob, b"alpha\nbeta\n").expect("old");
        let b_new = odb
            .write(ObjectKind::Blob, b"alpha\nBETA\ngamma\n")
            .expect("new");
        let old_tree = write_tree(&odb, &[("a.txt", b_old), ("b.txt", b_old)]);
        let new_tree = write_tree(
            &odb,
            &[("a.txt", b_new), ("b.txt", b_new), ("c.txt", b_new)],
        );
        let entries = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "")?;
        assert!(entries.len() >= 2);

        let cfg = ConfigSet::new();
        assert!(indent_heuristic_from_config(&cfg));
        let _ = unified_diff_histogram_hunks_only("a\nb\n", "a\nB\n", 3, 0, false);
        let _ = unified_diff_histogram_with_prefix_and_funcname_indent(
            "x\n", "y\n", "a/f", "b/f", 3, 0, "", "", None, false, false,
        );
        let _ = count_changes_with_algorithm("a\n", "b\n", Algorithm::Myers, false);
        let _ = should_break_rewrite_pair(b"short", b"totally different body", 50);
        let _ = head_path_states(&odb, Some(&new_tree))?;
        std::fs::write(wt.join("tracked.txt"), b"hello\n").expect("wt");
        let abs = wt.join("tracked.txt");
        let meta = std::fs::metadata(&abs).expect("meta");
        let mut index = Index::new();
        index.add_or_replace(index_entry("tracked.txt", MODE_REGULAR, b_old));
        let _ = stat_matches_with_policy(&index.entries()[0], &meta, StatMatchPolicy::default());
        let _ = worktree_differs_from_index_entry(
            &odb,
            &git_dir,
            wt,
            &index.entries()[0],
            false,
            None,
            None,
        )?;
        let conv = ConversionConfig::from_config(&cfg);
        let attrs = FileAttrs::default();
        let params = WorktreeAddRefreshParams {
            odb: &odb,
            ie: &index.entries()[0],
            meta: &meta,
            abs_path: &abs,
            rel_path: "tracked.txt",
            conv: &conv,
            file_attrs: &attrs,
            index_mtime: None,
            staged_mode: MODE_REGULAR,
            filter_process: None,
        };
        let _ = classify_worktree_entry_for_add(&params)?;
        let _ = hash_worktree_file(
            &odb,
            &abs,
            &meta,
            &conv,
            &attrs,
            "tracked.txt",
            Some(&index.entries()[0]),
            None,
        )?;
        let _ = materialize_worktree_blob(
            &odb,
            &abs,
            &meta,
            &conv,
            &attrs,
            "tracked.txt",
            Some(&index.entries()[0]),
            None,
        )?;
        let _ = read_worktree_bytes_for_mode(&abs, MODE_REGULAR);
        let _ = mode_from_metadata(&meta);
        let _ = smudge_racily_clean_entries(&odb, &git_dir, &mut index, wt, None, Some(&cfg), None);
        let _ = refresh_index_stat_content_verified(
            &odb,
            &git_dir,
            &mut index,
            wt,
            None,
            Some(&cfg),
            None,
        )?;
        let _ = refresh_index_stat_content_verified_with_rules(
            &odb,
            &git_dir,
            &mut index,
            wt,
            None,
            Some(&cfg),
            None,
            None,
        )?;
        let _ = path_checkout_skip_blob_write_when_up_to_date_with_config(
            &odb,
            &git_dir,
            wt,
            &index,
            &index.entries()[0],
            &b_old,
            MODE_REGULAR,
            "tracked.txt",
            None,
            None,
        )?;
        let patch = anchored_unified_diff(
            "a\nb\n",
            "a\nB\n",
            "f",
            "f",
            3,
            &[String::from("b")],
            Algorithm::Myers,
            false,
            true,
            false,
        );
        assert!(patch.contains("@@"));
        let _ = annotate_unified_diff_no_newline_at_eof("@@\n+a\n\\ No newline\n", "a", "a\n");
        Ok(())
    }

    #[test]
    fn unmerged_only_tree_worktree_diff_and_func_heuristics() -> Result<()> {
        use similar::DiffOp;
        assert!(inserted_block_starts_with_c_like_function_definition(
            "static int foo(int bar) {"
        ));
        assert!(!inserted_block_starts_with_c_like_function_definition(
            "printf(hi);"
        ));
        let ops = vec![
            DiffOp::Equal {
                old_index: 0,
                new_index: 0,
                len: 4,
            },
            DiffOp::Insert {
                old_index: 4,
                new_index: 4,
                new_len: 2,
            },
        ];
        let new_lines = ["a\n", "b\n", "c\n", "d\n", "void added() {\n", "}\n"];
        assert!(append_with_whole_function_added(
            &ops, 4, 6, &new_lines, None
        ));
        let _ = hunk_old_change_end_exclusive(&ops);

        let temp = TempDir::new().expect("tempdir");
        let wt = temp.path();
        let git_dir = wt.join(".git");
        let objects_dir = git_dir.join("objects");
        std::fs::create_dir_all(&objects_dir).expect("objects");
        let odb = Odb::new(&objects_dir);
        let tree_blob = odb.write(ObjectKind::Blob, b"base\n").expect("blob");
        std::fs::write(wt.join("conflict.txt"), b"worktree\n").expect("wt");
        let tree = write_tree(&odb, &[("conflict.txt", tree_blob)]);
        let mut index = Index::new();
        let mut stage1 = index_entry("conflict.txt", MODE_REGULAR, tree_blob);
        stage1.flags = (stage1.flags & 0x0FFF) | (1 << 12);
        index.entries_mut().push(stage1);
        let cfg = ConfigSet::new();
        let diff = diff_tree_to_worktree_with_git_dir(
            &odb,
            Some(&tree),
            wt,
            &git_dir,
            &index,
            Some(&cfg),
        )?;
        assert!(!diff.is_empty());
        Ok(())
    }

    #[test]
    fn high_volume_unified_diff_and_postprocess() -> Result<()> {
        let (_temp, odb, old_tree, new_tree) = pair_of_trees_with_rename();
        let config = ConfigSet::new();
        let policy = ThreadPolicy::default();
        for i in 0..600u32 {
            let old = format!("seed {i}\nalpha\n");
            let new = format!("seed {i}\nalpha\nvoid fn_{i}(int arg) {{\n  return arg;\n}}\n");
            let _ = unified_diff_with_prefix_and_funcname_and_algorithm(
                &old,
                &new,
                "a/f.c",
                "b/f.c",
                3,
                0,
                "a/",
                "b/",
                None,
                Algorithm::Myers,
                i % 2 == 0,
                i % 3 == 0,
                i % 5 == 0,
                i % 7 == 0,
            );
            let threshold = 30 + (i % 40);
            let raw = diff_trees(&odb, Some(&old_tree), Some(&new_tree), "")?;
            let opts = DiffPostprocessOptions {
                rename_threshold: threshold,
                copy_threshold: Some(threshold),
                find_copies_harder: i % 2 == 0,
                break_score: Some(threshold as u64),
                materialize_break_worktree_new_oid: i % 4 == 0,
            };
            let _ = postprocess_diff_entries(
                &odb,
                raw,
                Some(&old_tree),
                None,
                None,
                &config,
                &policy,
                &opts,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod refresh_checks_tests {
    use super::{RefreshChecks, RefreshContent};
    use crate::objects::ObjectId;

    #[test]
    fn empty_checks_answer_none_for_every_entry() {
        let checks = RefreshChecks::default();
        assert!(checks.0.is_empty());
        assert!(checks.get(0).is_none());
        assert!(checks.get(9_999).is_none());
    }

    #[test]
    fn filled_checks_answer_by_entry_position() {
        let oid = ObjectId::from_bytes(&[7u8; 20]).unwrap();
        let other = ObjectId::from_bytes(&[8u8; 20]).unwrap();
        let checks = RefreshChecks(vec![None, Some(RefreshContent::Hashed(oid)), None]);
        assert!(checks.get(0).is_none());
        assert!(RefreshContent::matches(checks.get(1), &oid));
        assert!(RefreshContent::differs(checks.get(1), &other));
        assert!(
            checks.get(3).is_none(),
            "positions past the end have no check"
        );
    }
}
