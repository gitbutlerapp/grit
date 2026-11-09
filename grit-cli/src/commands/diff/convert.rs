//! Map grit-lib structured diff rows into CLI [`FileDiff`] DTOs.

use grit_lib::diff::structured::StructuredLineKind;
use grit_lib::diff::DiffOptions;
use grit_lib::diff::{
    annotate_structured_hunks, line_stat_for_target_file, presentation_hunks_from_listed,
    presentation_hunks_from_tree, AnnotatedStructuredHunk, DiffDetail, DiffSide, FileChange,
    ListedFileDiff, TreeDiffStructured,
};

use super::{FileDiff, Hunk, Line, LineKind, LineStatJson, Segment};

pub fn map_listed_file(f: ListedFileDiff, detail: DiffDetail) -> FileDiff {
    let content_changed = f
        .stat
        .map(|s| s.insertions + s.deletions > 0)
        .unwrap_or(true);
    let old_encoding_lossy = f
        .structured
        .as_ref()
        .map(|s| s.old_encoding_lossy)
        .unwrap_or(false);
    let new_encoding_lossy = f
        .structured
        .as_ref()
        .map(|s| s.new_encoding_lossy)
        .unwrap_or(false);
    let hunks = if let Some(combined) = f.combined {
        Some(
            combined
                .hunks
                .into_iter()
                .map(|h| Hunk {
                    old_start: 0,
                    new_start: 0,
                    old_lines: 0,
                    new_lines: 0,
                    context: None,
                    combined_header: Some(h.header),
                    id: None,
                    index: None,
                    lines: h
                        .lines
                        .into_iter()
                        .map(|l| Line {
                            kind: match l.kind {
                                grit_lib::diff::combined_diff_patch::CombinedStructuredLineKind::Context => {
                                    LineKind::Context
                                }
                                grit_lib::diff::combined_diff_patch::CombinedStructuredLineKind::Add => {
                                    LineKind::Add
                                }
                                grit_lib::diff::combined_diff_patch::CombinedStructuredLineKind::Del => {
                                    LineKind::Del
                                }
                            },
                            old: None,
                            new: None,
                            segments: vec![Segment {
                                text: l.text,
                                emphasis: false,
                            }],
                            no_newline_at_eof: false,
                            parents: Some(l.parents.iter().map(|c| c.to_string()).collect()),
                        })
                        .collect(),
                })
                .collect(),
        )
    } else {
        presentation_hunks_from_listed(&f, detail).map(cli_hunks_from_annotated)
    };
    FileDiff {
        path: f.path,
        old_path: f.old_path,
        status: f.status.into(),
        old_mode: f.old_mode,
        new_mode: f.new_mode,
        old_encoding_lossy,
        new_encoding_lossy,
        binary: f.binary,
        old_size: f.binary.then_some(f.old_size),
        new_size: f.binary.then_some(f.new_size),
        content_changed,
        similarity: f.similarity,
        stat: f.stat.map(LineStatJson::from),
        combined_status: f.combined_status,
        hunks: if detail == DiffDetail::Patch {
            Some(hunks.unwrap_or_default())
        } else {
            None
        },
    }
}

pub fn cli_file_from_tree(t: TreeDiffStructured) -> FileDiff {
    let hunks = cli_hunks_from_annotated(presentation_hunks_from_tree(&t));
    let e = t.entry;
    let path = e
        .new_path
        .clone()
        .or_else(|| e.old_path.clone())
        .unwrap_or_default();
    let similarity = e.rename_similarity();
    let old_path = e
        .old_path
        .as_ref()
        .filter(|op| op.as_str() != path)
        .cloned();
    let structured = t.structured;
    FileDiff {
        path,
        old_path,
        status: e.status.into(),
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        binary: t.binary,
        old_size: t.binary.then_some(t.old_size),
        new_size: t.binary.then_some(t.new_size),
        content_changed: e.old_oid != e.new_oid,
        similarity,
        stat: None,
        combined_status: None,
        hunks: Some(hunks),
    }
}

pub fn cli_file_from_file_change(
    fc: &FileChange,
    diff_options: DiffOptions,
    side: DiffSide,
    peer_hunks: &[grit_lib::diff::structured::StructuredHunk],
    assign_hunk_ids: bool,
) -> FileDiff {
    let structured = fc.structured(diff_options);
    let hunks = cli_hunks_from_annotated(annotate_structured_hunks(
        side,
        &fc.path,
        structured.hunks,
        peer_hunks,
        assign_hunk_ids,
    ));
    FileDiff {
        path: fc.path.clone(),
        old_path: fc.old_path.clone(),
        status: fc.status.into(),
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        binary: fc.binary,
        old_size: fc.binary.then_some(fc.old_bytes.byte_len()),
        new_size: fc.binary.then_some(fc.new_bytes.byte_len()),
        content_changed: fc.content_changed,
        similarity: fc.similarity,
        stat: None,
        combined_status: None,
        hunks: Some(hunks),
    }
}

pub fn cli_file_from_lib(f: grit_lib::diff::FileDiff, detail: DiffDetail) -> FileDiff {
    let stat = line_stat_for_target_file(&f, detail).map(LineStatJson::from);
    let hunks = if detail == DiffDetail::Patch {
        Some(cli_hunks_from_annotated(annotate_structured_hunks(
            DiffSide::Other,
            &f.path,
            f.hunks,
            &[],
            false,
        )))
    } else {
        None
    };
    FileDiff {
        path: f.path,
        old_path: f.old_path,
        status: f.status.into(),
        old_mode: f.old_mode,
        new_mode: f.new_mode,
        old_encoding_lossy: f.old_encoding_lossy,
        new_encoding_lossy: f.new_encoding_lossy,
        binary: f.binary,
        old_size: f.binary.then_some(f.old_size),
        new_size: f.binary.then_some(f.new_size),
        content_changed: f.content_changed,
        similarity: None,
        stat,
        combined_status: None,
        hunks,
    }
}

pub(crate) fn cli_hunks_from_annotated(annotated: Vec<AnnotatedStructuredHunk>) -> Vec<Hunk> {
    annotated
        .into_iter()
        .map(|row| {
            let mut out = hunk_from_structured(row.hunk);
            if let Some(id) = row.id {
                out.id = Some(id.to_string());
            }
            out.index = row.index;
            out
        })
        .collect()
}

fn hunk_from_structured(h: grit_lib::diff::structured::StructuredHunk) -> Hunk {
    Hunk {
        old_start: h.old_start,
        new_start: h.new_start,
        old_lines: h.old_lines,
        new_lines: h.new_lines,
        context: h.context,
        combined_header: None,
        id: None,
        index: None,
        lines: h
            .lines
            .into_iter()
            .map(|l| Line {
                kind: match l.kind {
                    StructuredLineKind::Context => LineKind::Context,
                    StructuredLineKind::Add => LineKind::Add,
                    StructuredLineKind::Del => LineKind::Del,
                },
                old: l.old,
                new: l.new,
                segments: l
                    .segments
                    .into_iter()
                    .map(|s| Segment {
                        text: s.text,
                        emphasis: s.emphasis,
                    })
                    .collect(),
                no_newline_at_eof: l.no_newline_at_eof,
                parents: None,
            })
            .collect(),
    }
}
