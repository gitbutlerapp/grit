//! Single line-hunk engine honoring [`DiffOptions`] for structured and unified output.

use similar::{ChangeTag, DiffOp, TextDiff};

use crate::diff::options::{DiffAlgorithm, DiffOptions};
use crate::diff::{git_xdiff_compacted_line_ops, group_diff_ops_gap, histogram_compacted_line_ops};
use crate::diff_indent_heuristic;

use super::{StructuredHunk, StructuredLine, StructuredLineKind, StructuredSegment};

/// Line-level hunks for two text sides, honoring algorithm, context, and indent heuristic.
#[must_use]
pub(crate) fn line_hunks(
    old_content: &str,
    new_content: &str,
    options: &DiffOptions,
) -> Vec<StructuredHunk> {
    if old_content == new_content {
        return Vec::new();
    }
    match options.algorithm {
        DiffAlgorithm::Histogram => {
            let old_lines: Vec<&str> = split_lines_git(old_content);
            let new_lines: Vec<&str> = split_lines_git(new_content);
            let ops =
                histogram_compacted_line_ops(old_content, new_content, options.indent_heuristic);
            structured_hunks_from_ops(
                old_content,
                new_content,
                &old_lines,
                &new_lines,
                ops,
                options,
            )
        }
        DiffAlgorithm::Myers => {
            let old_lines: Vec<&str> = split_lines_git(old_content);
            let new_lines: Vec<&str> = split_lines_git(new_content);
            let ops =
                git_xdiff_compacted_line_ops(&old_lines, &new_lines, options.indent_heuristic);
            structured_hunks_from_ops(
                old_content,
                new_content,
                &old_lines,
                &new_lines,
                ops,
                options,
            )
        }
        DiffAlgorithm::Patience | DiffAlgorithm::Minimal => {
            let ops = diff_indent_heuristic::diff_lines_ops_compacted(
                old_content,
                new_content,
                options.algorithm.similar_algorithm(),
                options.indent_heuristic,
            );
            let old_lines: Vec<&str> = split_lines_git(old_content);
            let new_lines: Vec<&str> = split_lines_git(new_content);
            structured_hunks_from_ops(
                old_content,
                new_content,
                &old_lines,
                &new_lines,
                ops,
                options,
            )
        }
    }
}

fn split_lines_git(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let ends_nl = text.ends_with('\n');
    let mut parts: Vec<&str> = text.split('\n').collect();
    if ends_nl && parts.last() == Some(&"") {
        parts.pop();
    }
    parts
}

fn ops_substantively_change(ops: &[DiffOp]) -> bool {
    ops.iter()
        .any(|op| !matches!(op, DiffOp::Equal { .. }))
}

fn structured_hunks_from_ops(
    old_content: &str,
    new_content: &str,
    old_lines: &[&str],
    new_lines: &[&str],
    ops: Vec<DiffOp>,
    options: &DiffOptions,
) -> Vec<StructuredHunk> {
    if !ops_substantively_change(&ops) {
        if old_content != new_content {
            return line_ending_only_structured_hunks(old_content, new_content, options);
        }
        return Vec::new();
    }
    let max_gap = options
        .context_lines
        .saturating_mul(2)
        .saturating_add(options.inter_hunk_context);
    let groups = group_diff_ops_gap(ops, options.context_lines, max_gap);
    let before_ends_nl = old_content.ends_with('\n');
    let after_ends_nl = new_content.ends_with('\n');
    let before_count = old_lines.len();
    let after_count = new_lines.len();

    groups
        .into_iter()
        .filter_map(|group| {
            let first = group.first()?;
            let (old_lines_count, new_lines_count) = hunk_line_counts(&group);
            let old_start = unified_side_start(first.old_range().start, old_lines_count);
            let new_start = unified_side_start(first.new_range().start, new_lines_count);

            let mut lines = Vec::new();
            for op in &group {
                for change in op_changes(op, old_lines, new_lines) {
                    lines.push(change);
                }
            }

            Some(StructuredHunk {
                old_start,
                new_start,
                old_lines: old_lines_count,
                new_lines: new_lines_count,
                context: None,
                lines: lines
                    .into_iter()
                    .map(|(kind, old_idx, new_idx, text)| {
                        structured_line_from_indices(
                            kind,
                            old_idx,
                            new_idx,
                            text,
                            before_count,
                            after_count,
                            before_ends_nl,
                            after_ends_nl,
                        )
                    })
                    .collect(),
            })
        })
        .collect()
}

fn op_changes(
    op: &DiffOp,
    old_lines: &[&str],
    new_lines: &[&str],
) -> Vec<(StructuredLineKind, Option<usize>, Option<usize>, String)> {
    let mut out = Vec::new();
    match *op {
        DiffOp::Equal {
            old_index,
            new_index,
            len,
        } => {
            for i in 0..len {
                let text = old_lines
                    .get(old_index + i)
                    .copied()
                    .unwrap_or("")
                    .to_owned();
                out.push((
                    StructuredLineKind::Context,
                    Some(old_index + i),
                    Some(new_index + i),
                    text,
                ));
            }
        }
        DiffOp::Delete {
            old_index, old_len, ..
        } => {
            for i in 0..old_len {
                let text = old_lines
                    .get(old_index + i)
                    .copied()
                    .unwrap_or("")
                    .to_owned();
                out.push((StructuredLineKind::Del, Some(old_index + i), None, text));
            }
        }
        DiffOp::Insert {
            new_index, new_len, ..
        } => {
            for i in 0..new_len {
                let text = new_lines
                    .get(new_index + i)
                    .copied()
                    .unwrap_or("")
                    .to_owned();
                out.push((StructuredLineKind::Add, None, Some(new_index + i), text));
            }
        }
        DiffOp::Replace {
            old_index,
            old_len,
            new_index,
            new_len,
        } => {
            for i in 0..old_len {
                let text = old_lines
                    .get(old_index + i)
                    .copied()
                    .unwrap_or("")
                    .to_owned();
                out.push((StructuredLineKind::Del, Some(old_index + i), None, text));
            }
            for i in 0..new_len {
                let text = new_lines
                    .get(new_index + i)
                    .copied()
                    .unwrap_or("")
                    .to_owned();
                out.push((StructuredLineKind::Add, None, Some(new_index + i), text));
            }
        }
    }
    out
}

fn line_ending_only_structured_hunks(
    old_content: &str,
    new_content: &str,
    options: &DiffOptions,
) -> Vec<StructuredHunk> {
    let diff = TextDiff::from_lines(old_content, new_content);
    let old_lines: Vec<String> = split_lines_git(old_content)
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let new_lines: Vec<String> = split_lines_git(new_content)
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    diff.grouped_ops(options.context_lines)
        .into_iter()
        .filter_map(|group| {
            let first = group.first()?;
            let (old_lines_count, new_lines_count) = hunk_line_counts(&group);
            let old_start = unified_side_start(first.old_range().start, old_lines_count);
            let new_start = unified_side_start(first.new_range().start, new_lines_count);
            let before_ends_nl = old_content.ends_with('\n');
            let after_ends_nl = new_content.ends_with('\n');
            let mut lines = Vec::new();
            for op in &group {
                for change in diff.iter_changes(op) {
                    let kind = match change.tag() {
                        ChangeTag::Equal => StructuredLineKind::Context,
                        ChangeTag::Delete => StructuredLineKind::Del,
                        ChangeTag::Insert => StructuredLineKind::Add,
                    };
                    let text = match kind {
                        StructuredLineKind::Del => line_body_idx(&old_lines, change.old_index()),
                        StructuredLineKind::Add => line_body_idx(&new_lines, change.new_index()),
                        StructuredLineKind::Context => {
                            line_body_idx(&old_lines, change.old_index())
                        }
                    };
                    lines.push(structured_line_from_indices(
                        kind,
                        change.old_index(),
                        change.new_index(),
                        text,
                        old_lines.len(),
                        new_lines.len(),
                        before_ends_nl,
                        after_ends_nl,
                    ));
                }
            }
            Some(StructuredHunk {
                old_start,
                new_start,
                old_lines: old_lines_count,
                new_lines: new_lines_count,
                context: None,
                lines,
            })
        })
        .collect()
}

fn line_body_idx(lines: &[String], index: Option<usize>) -> String {
    index
        .and_then(|i| lines.get(i))
        .cloned()
        .unwrap_or_default()
}

fn hunk_line_counts(group: &[DiffOp]) -> (usize, usize) {
    let mut old = 0usize;
    let mut new = 0usize;
    for op in group {
        match *op {
            DiffOp::Equal { len, .. } => {
                old += len;
                new += len;
            }
            DiffOp::Delete { old_len, .. } => old += old_len,
            DiffOp::Insert { new_len, .. } => new += new_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                old += old_len;
                new += new_len;
            }
        }
    }
    (old, new)
}

fn unified_side_start(anchor_index_0based: usize, line_count: usize) -> usize {
    if line_count == 0 {
        anchor_index_0based
    } else {
        anchor_index_0based + 1
    }
}

#[allow(clippy::too_many_arguments)]
fn structured_line_from_indices(
    kind: StructuredLineKind,
    old_index: Option<usize>,
    new_index: Option<usize>,
    text: String,
    before_count: usize,
    after_count: usize,
    before_ends_nl: bool,
    after_ends_nl: bool,
) -> StructuredLine {
    StructuredLine {
        kind,
        old: old_index.map(|i| i + 1),
        new: new_index.map(|i| i + 1),
        segments: vec![StructuredSegment {
            text,
            emphasis: false,
        }],
        no_newline_at_eof: match kind {
            StructuredLineKind::Del => {
                line_missing_eof_newline(old_index, before_count, before_ends_nl)
            }
            StructuredLineKind::Add => {
                line_missing_eof_newline(new_index, after_count, after_ends_nl)
            }
            StructuredLineKind::Context => {
                line_missing_eof_newline(old_index, before_count, before_ends_nl)
                    || line_missing_eof_newline(new_index, after_count, after_ends_nl)
            }
        },
    }
}

fn line_missing_eof_newline(
    index: Option<usize>,
    line_count: usize,
    file_ends_with_newline: bool,
) -> bool {
    if file_ends_with_newline {
        return false;
    }
    index == Some(line_count.saturating_sub(1))
}
