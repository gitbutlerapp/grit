//! Single line-hunk engine honoring [`DiffOptions`] for structured and unified output.

use super::super::{
    imara_unified_hunk_slices, unified_hunk_body_for_diff_options, unified_hunk_header_counts,
};
use super::{StructuredHunk, StructuredLine, StructuredLineKind, StructuredSegment};
use crate::diff::options::DiffOptions;
use crate::hash::ChecksumHasher;
use crate::objects::HashAlgo;
/// Structured hunk metadata for stable id hashing from one unified hunk slice (`@@` …).
///
/// Skips per-line old/new coordinates; [`super::hunk_id`] only hashes kind, text, and
/// `no_newline_at_eof`.
#[cfg(test)]
#[must_use]
pub(crate) fn structured_hunk_for_id_from_unified_slice(
    hunk_str: &str,
    before_count: usize,
    after_count: usize,
    before_ends_nl: bool,
    after_ends_nl: bool,
) -> Option<StructuredHunk> {
    let patch_lines = split_unified_hunk_on_lf(hunk_str);
    let header = patch_lines.first()?;
    let (old_start, old_lines, new_start, new_lines) = unified_hunk_header_counts(header)?;

    let mut old_line = old_start;
    let mut new_line = new_start;
    let mut structured_lines: Vec<StructuredLine> = Vec::new();
    for line in patch_lines.iter().skip(1) {
        if *line == "\\ No newline at end of file" {
            if let Some(last) = structured_lines.last_mut() {
                last.no_newline_at_eof = true;
            }
            continue;
        }
        let (kind, prefix_len) = match line.as_bytes().first() {
            Some(b' ') => (StructuredLineKind::Context, 1),
            Some(b'-') => (StructuredLineKind::Del, 1),
            Some(b'+') => (StructuredLineKind::Add, 1),
            _ => continue,
        };
        let text = line[prefix_len..].to_owned();
        let (old_idx, new_idx) = match kind {
            StructuredLineKind::Context => {
                let o = old_line;
                let n = new_line;
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                (Some(o), Some(n))
            }
            StructuredLineKind::Del => {
                let o = old_line;
                old_line = old_line.saturating_add(1);
                (Some(o), None)
            }
            StructuredLineKind::Add => {
                let n = new_line;
                new_line = new_line.saturating_add(1);
                (None, Some(n))
            }
        };
        structured_lines.push(StructuredLine {
            kind,
            old: None,
            new: None,
            segments: vec![StructuredSegment {
                text,
                emphasis: false,
            }],
            no_newline_at_eof: match kind {
                StructuredLineKind::Del => line_missing_eof_newline(
                    old_idx.map(|n| n.saturating_sub(1)),
                    before_count,
                    before_ends_nl,
                ),
                StructuredLineKind::Add => line_missing_eof_newline(
                    new_idx.map(|n| n.saturating_sub(1)),
                    after_count,
                    after_ends_nl,
                ),
                StructuredLineKind::Context => {
                    line_missing_eof_newline(
                        old_idx.map(|n| n.saturating_sub(1)),
                        before_count,
                        before_ends_nl,
                    ) || line_missing_eof_newline(
                        new_idx.map(|n| n.saturating_sub(1)),
                        after_count,
                        after_ends_nl,
                    )
                }
            },
        });
    }

    Some(StructuredHunk {
        old_start,
        new_start,
        old_lines,
        new_lines,
        context: None,
        lines: structured_lines,
    })
}

fn hash_unified_hunk_line(
    hasher: &mut ChecksumHasher,
    kind: StructuredLineKind,
    text: &str,
    no_newline_at_eof: bool,
) {
    let kind_byte = match kind {
        StructuredLineKind::Context => b' ',
        StructuredLineKind::Add => b'+',
        StructuredLineKind::Del => b'-',
    };
    hasher.update(&[kind_byte]);
    hasher.update(text.as_bytes());
    hasher.update(&[if no_newline_at_eof { 1 } else { 0 }]);
}

/// Append hunk line-body bytes to `hasher` (same bytes as [`super::hunk_id::hash_hunk_body`]).
pub(crate) fn hash_hunk_body_from_unified_slice(
    hasher: &mut ChecksumHasher,
    hunk_str: &str,
    before_count: usize,
    after_count: usize,
    before_ends_nl: bool,
    after_ends_nl: bool,
) -> bool {
    let patch_lines = split_unified_hunk_on_lf(hunk_str);
    let Some(header) = patch_lines.first() else {
        return false;
    };
    let Some((old_start, _, new_start, _)) = unified_hunk_header_counts(header) else {
        return false;
    };

    let mut old_line = old_start;
    let mut new_line = new_start;
    let mut lines: Vec<(StructuredLineKind, &str, bool)> = Vec::new();
    for line in patch_lines.iter().skip(1) {
        if *line == "\\ No newline at end of file" {
            if let Some(last) = lines.last_mut() {
                last.2 = true;
            }
            continue;
        }
        let (kind, prefix_len) = match line.as_bytes().first() {
            Some(b' ') => (StructuredLineKind::Context, 1),
            Some(b'-') => (StructuredLineKind::Del, 1),
            Some(b'+') => (StructuredLineKind::Add, 1),
            _ => continue,
        };
        let text = &line[prefix_len..];
        let (old_idx, new_idx) = match kind {
            StructuredLineKind::Context => {
                let o = old_line;
                let n = new_line;
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                (Some(o), Some(n))
            }
            StructuredLineKind::Del => {
                let o = old_line;
                old_line = old_line.saturating_add(1);
                (Some(o), None)
            }
            StructuredLineKind::Add => {
                let n = new_line;
                new_line = new_line.saturating_add(1);
                (None, Some(n))
            }
        };
        let no_newline = match kind {
            StructuredLineKind::Del => line_missing_eof_newline(
                old_idx.map(|n| n.saturating_sub(1)),
                before_count,
                before_ends_nl,
            ),
            StructuredLineKind::Add => line_missing_eof_newline(
                new_idx.map(|n| n.saturating_sub(1)),
                after_count,
                after_ends_nl,
            ),
            StructuredLineKind::Context => {
                line_missing_eof_newline(
                    old_idx.map(|n| n.saturating_sub(1)),
                    before_count,
                    before_ends_nl,
                ) || line_missing_eof_newline(
                    new_idx.map(|n| n.saturating_sub(1)),
                    after_count,
                    after_ends_nl,
                )
            }
        };
        lines.push((kind, text, no_newline));
    }
    for (kind, text, no_newline) in lines {
        hash_unified_hunk_line(hasher, kind, text, no_newline);
    }
    true
}

/// Body digest for one unified hunk slice, matching [`super::hunk_id::hunk_body_digest`].
#[must_use]
pub(crate) fn hunk_body_digest_from_unified_slice(
    hunk_str: &str,
    before_count: usize,
    after_count: usize,
    before_ends_nl: bool,
    after_ends_nl: bool,
) -> Option<Vec<u8>> {
    let mut hasher = HashAlgo::Sha1.checksum_hasher();
    hash_hunk_body_from_unified_slice(
        &mut hasher,
        hunk_str,
        before_count,
        after_count,
        before_ends_nl,
        after_ends_nl,
    )
    .then(|| hasher.finalize())
}

/// Line-level hunks for two text sides, honoring algorithm, context, and indent heuristic.
#[must_use]
pub(crate) fn line_hunks(
    old_content: &str,
    new_content: &str,
    options: &DiffOptions,
    enclosing_function_context: bool,
) -> Vec<StructuredHunk> {
    if !options.whitespace.is_active() && old_content == new_content {
        return Vec::new();
    }
    let body = unified_hunk_body_for_diff_options(old_content, new_content, options);
    structured_hunks_from_unified_body(&body, old_content, new_content, enclosing_function_context)
}

fn structured_hunks_from_unified_body(
    body: &str,
    old_content: &str,
    new_content: &str,
    enclosing_function_context: bool,
) -> Vec<StructuredHunk> {
    if body.is_empty() {
        return Vec::new();
    }
    let before_ends_nl = old_content.ends_with('\n');
    let after_ends_nl = new_content.ends_with('\n');
    let before_count = line_count(old_content);
    let after_count = line_count(new_content);
    let old_line_bodies = if enclosing_function_context {
        super::old_line_bodies_for_context(old_content)
    } else {
        Vec::new()
    };

    imara_unified_hunk_slices(body)
        .into_iter()
        .filter_map(|hunk_str| {
            let patch_lines = split_unified_hunk_on_lf(hunk_str);
            let header = patch_lines.first()?;
            let (old_start, old_lines, new_start, new_lines) = unified_hunk_header_counts(header)?;

            let mut old_line = old_start;
            let mut new_line = new_start;
            let mut structured_lines: Vec<StructuredLine> = Vec::new();

            for line in patch_lines.iter().skip(1) {
                if *line == "\\ No newline at end of file" {
                    if let Some(last) = structured_lines.last_mut() {
                        last.no_newline_at_eof = true;
                    }
                    continue;
                }
                let (kind, prefix_len) = match line.as_bytes().first() {
                    Some(b' ') => (StructuredLineKind::Context, 1),
                    Some(b'-') => (StructuredLineKind::Del, 1),
                    Some(b'+') => (StructuredLineKind::Add, 1),
                    _ => continue,
                };
                let text = line[prefix_len..].to_owned();
                let (old_idx, new_idx) = match kind {
                    StructuredLineKind::Context => {
                        let o = old_line;
                        let n = new_line;
                        old_line = old_line.saturating_add(1);
                        new_line = new_line.saturating_add(1);
                        (Some(o), Some(n))
                    }
                    StructuredLineKind::Del => {
                        let o = old_line;
                        old_line = old_line.saturating_add(1);
                        (Some(o), None)
                    }
                    StructuredLineKind::Add => {
                        let n = new_line;
                        new_line = new_line.saturating_add(1);
                        (None, Some(n))
                    }
                };
                structured_lines.push(StructuredLine {
                    kind,
                    old: old_idx,
                    new: new_idx,
                    segments: vec![StructuredSegment {
                        text,
                        emphasis: false,
                    }],
                    no_newline_at_eof: match kind {
                        StructuredLineKind::Del => line_missing_eof_newline(
                            old_idx.map(|n| n.saturating_sub(1)),
                            before_count,
                            before_ends_nl,
                        ),
                        StructuredLineKind::Add => line_missing_eof_newline(
                            new_idx.map(|n| n.saturating_sub(1)),
                            after_count,
                            after_ends_nl,
                        ),
                        StructuredLineKind::Context => {
                            line_missing_eof_newline(
                                old_idx.map(|n| n.saturating_sub(1)),
                                before_count,
                                before_ends_nl,
                            ) || line_missing_eof_newline(
                                new_idx.map(|n| n.saturating_sub(1)),
                                after_count,
                                after_ends_nl,
                            )
                        }
                    },
                });
            }

            let context = if enclosing_function_context {
                structured_lines
                    .iter()
                    .filter(|l| l.kind == StructuredLineKind::Del)
                    .filter_map(|l| l.old)
                    .min()
                    .or_else(|| structured_lines.iter().filter_map(|l| l.old).min())
                    .and_then(|old_line_no| {
                        super::enclosing_function_context(&old_line_bodies, old_line_no)
                    })
            } else {
                None
            };

            Some(StructuredHunk {
                old_start,
                new_start,
                old_lines,
                new_lines,
                context,
                lines: structured_lines,
            })
        })
        .collect()
}

/// Split unified hunk text on `\n` only so `\r` stays in line bodies (Git CRLF parity).
fn split_unified_hunk_on_lf(hunk: &str) -> Vec<&str> {
    if hunk.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = hunk.split('\n').collect();
    if hunk.ends_with('\n') && parts.last() == Some(&"") {
        parts.pop();
    }
    parts
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let ends_nl = text.ends_with('\n');
    let mut parts: usize = text.split('\n').count();
    if ends_nl && text.ends_with("\n") {
        parts = parts.saturating_sub(1);
    }
    parts
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
