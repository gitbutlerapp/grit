//! Structured diff outcomes helpers shared by the CLI (hunk ids, listing rules, rename notices).

use super::changes::FileChange;
use super::detail::LineStat;
use super::detail::{DiffDetail, FileDiff as ListedFileDiff};
use super::hunk_id::{hunk_ids, DiffSide, HunkId};
use super::options::DiffOptions;
use super::structured::StructuredHunk;
use super::{DiffStatus, InexactRenameDetectionSkipped, TreeDiffStructured};

/// One structured hunk optionally decorated with a stable [`HunkId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotatedStructuredHunk {
    /// The hunk body and line metadata.
    pub hunk: StructuredHunk,
    /// Stable id when assigned (staged/unstaged with peer hunks).
    pub id: Option<HunkId>,
    /// 0-based index within the file when ids are assigned.
    pub index: Option<usize>,
}

/// Assign stable hunk ids and indices for staged/unstaged diffs.
#[must_use]
pub fn annotate_structured_hunks(
    side: DiffSide,
    path: &str,
    hunks: Vec<StructuredHunk>,
    peer_hunks: &[StructuredHunk],
    assign_ids: bool,
) -> Vec<AnnotatedStructuredHunk> {
    let assign = assign_ids && matches!(side, DiffSide::Unstaged | DiffSide::Staged);
    let ids = if assign {
        hunk_ids(side, path, &hunks, peer_hunks)
    } else {
        Vec::new()
    };
    hunks
        .into_iter()
        .enumerate()
        .map(|(index, hunk)| AnnotatedStructuredHunk {
            hunk,
            id: if assign {
                Some(ids[index].clone())
            } else {
                None
            },
            index: if assign { Some(index) } else { None },
        })
        .collect()
}

/// Whether a working-tree [`FileChange`] should appear in a whitespace-filtered patch listing.
#[must_use]
pub fn include_file_change_in_patch(change: &FileChange, options: &DiffOptions) -> bool {
    let ws = options.whitespace;
    if !ws.is_active() {
        return true;
    }
    if matches!(
        change.status,
        DiffStatus::Renamed
            | DiffStatus::Copied
            | DiffStatus::Added
            | DiffStatus::Deleted
            | DiffStatus::TypeChanged
    ) {
        return true;
    }
    if change.old_mode != change.new_mode {
        return true;
    }
    if change.binary {
        return change.content_changed;
    }
    !change.structured(options.clone()).hunks.is_empty()
}

/// Whether callers should surface rename-limit skip metadata to the user.
#[must_use]
pub fn should_report_inexact_rename_skipped(skipped: &InexactRenameDetectionSkipped) -> bool {
    skipped.unmatched_sources > 0 || skipped.unmatched_destinations > 0
}

/// Map a listed file row from [`super::Diff::files_for_entries`].
#[must_use]
pub fn presentation_hunks_from_listed(
    f: &ListedFileDiff,
    detail: DiffDetail,
) -> Option<Vec<AnnotatedStructuredHunk>> {
    if detail != DiffDetail::Patch {
        return None;
    }
    f.structured.as_ref().map(|structured| {
        annotate_structured_hunks(
            DiffSide::Other,
            &f.path,
            structured.hunks.clone(),
            &[],
            false,
        )
    })
}

/// Map a tree structured diff row into annotated hunks.
#[must_use]
pub fn presentation_hunks_from_tree(t: &TreeDiffStructured) -> Vec<AnnotatedStructuredHunk> {
    let path = t
        .entry
        .new_path
        .clone()
        .or_else(|| t.entry.old_path.clone())
        .unwrap_or_default();
    annotate_structured_hunks(
        DiffSide::Other,
        &path,
        t.structured.hunks.clone(),
        &[],
        false,
    )
}

/// Line stat for a targets [`super::targets::FileDiff`] row under a given detail mode.
#[must_use]
pub fn line_stat_for_target_file(
    f: &super::targets::FileDiff,
    detail: DiffDetail,
) -> Option<LineStat> {
    if f.binary {
        return None;
    }
    if detail == DiffDetail::Stat {
        f.line_stat.or_else(|| Some(LineStat::from_hunks(&f.hunks)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::structured::{
        SideBytes, StructuredLine, StructuredLineKind, StructuredSegment,
    };
    use crate::diff::WhitespaceMode;
    use crate::objects::{FileMode, ObjectId};

    fn sample_hunk(body: &str) -> StructuredHunk {
        StructuredHunk {
            old_start: 1,
            new_start: 1,
            old_lines: 1,
            new_lines: 1,
            context: None,
            lines: vec![StructuredLine {
                kind: StructuredLineKind::Add,
                old: None,
                new: Some(1),
                segments: vec![StructuredSegment {
                    text: body.into(),
                    emphasis: false,
                }],
                no_newline_at_eof: false,
            }],
        }
    }

    #[test]
    fn annotate_hunks_assign_indices_on_staged_side() {
        let hunk = sample_hunk("same");
        let annotated = annotate_structured_hunks(DiffSide::Staged, "f.txt", vec![hunk], &[], true);
        assert_eq!(annotated[0].index, Some(0));
        assert!(annotated[0].id.is_some());
    }

    #[test]
    fn include_file_change_drops_whitespace_only_modification() {
        let change = FileChange {
            path: "x".into(),
            old_path: None,
            status: DiffStatus::Modified,
            old_mode: FileMode::REGULAR,
            new_mode: FileMode::REGULAR,
            old_oid: ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap(),
            new_oid: ObjectId::from_hex("0000000000000000000000000000000000000002").unwrap(),
            old_bytes: SideBytes::from_bytes(b"a\n".to_vec()),
            new_bytes: SideBytes::from_bytes(b"a\n".to_vec()),
            binary: false,
            content_changed: true,
            similarity: None,
        };
        let opts = DiffOptions::default().with_whitespace(WhitespaceMode::ignore_all_space());
        assert!(!include_file_change_in_patch(&change, &opts));
    }
}
