//! Shared conflict classification and in-progress operation vocabulary.
//!
//! Merge, cherry-pick, revert (and later rebase) use the same [`Conflict`] shapes and
//! [`ConflictKind`] names. Status porcelain uses a separate, Git-compatible
//! [`ConflictKind`](crate::porcelain::status::ConflictKind) for `--porcelain=v2` XY codes.
//!
//! ```rust
//! use grit_lib::operation::{conflict_kind_from_stages, ConflictKind};
//!
//! assert_eq!(
//!     conflict_kind_from_stages(false, true, true),
//!     ConflictKind::AddAdd
//! );
//! ```

use crate::diff::merge_trees::TreeMergeOutput;
use crate::index::Index;

pub use crate::state::{
    operation_alternatives, operation_next_steps, operation_next_steps_empty, status_operation,
    InProgressOperation, NextStep, Operation,
};

/// How a tree-merge conflict should be presented (content, add/add, modify/delete, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both sides edited the same tracked file (textual or binary).
    Content,
    /// Both sides added the same path with different content.
    AddAdd,
    /// One side deleted the path and the other modified it.
    ModifyDelete {
        /// When true, the deletion is on the current branch (HEAD); the incoming side modified the file.
        deleted_in_current: bool,
    },
}

impl ConflictKind {
    /// Stable snake_case name for JSON and scripting.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::AddAdd => "add_add",
            Self::ModifyDelete { .. } => "modify_delete",
        }
    }
}

/// One conflicted path from a tree merge or unmerged index stages 1–3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Repository-relative path.
    pub path: String,
    /// Conflict shape derived from unmerged index stages.
    pub kind: ConflictKind,
}

/// Derive [`ConflictKind`] from which unmerged index stages exist for a path.
///
/// Matches the classification used when recording merge conflicts from
/// [`TreeMergeOutput`].
#[must_use]
pub fn conflict_kind_from_stages(stage1: bool, stage2: bool, stage3: bool) -> ConflictKind {
    match (stage1, stage2, stage3) {
        (false, true, true) => ConflictKind::AddAdd,
        (true, true, true) => ConflictKind::Content,
        (true, false, true) => ConflictKind::ModifyDelete {
            deleted_in_current: true,
        },
        (true, true, false) => ConflictKind::ModifyDelete {
            deleted_in_current: false,
        },
        _ => ConflictKind::Content,
    }
}

/// Classify unmerged paths in `index` (sorted, deduplicated).
#[must_use]
pub fn conflicts_from_index(index: &Index) -> Vec<Conflict> {
    let paths = conflict_path_bytes_from_index(index);
    paths
        .into_iter()
        .map(|path_bytes| {
            let stage1 = index.get(&path_bytes, 1).is_some();
            let stage2 = index.get(&path_bytes, 2).is_some();
            let stage3 = index.get(&path_bytes, 3).is_some();
            Conflict {
                path: String::from_utf8_lossy(&path_bytes).into_owned(),
                kind: conflict_kind_from_stages(stage1, stage2, stage3),
            }
        })
        .collect()
}

/// Collect conflicted paths and kinds from a three-way tree merge result.
#[must_use]
pub fn conflicts_from_tree_merge(merged: &TreeMergeOutput) -> Vec<Conflict> {
    let paths = tree_merge_conflict_path_strings(merged);
    paths
        .into_iter()
        .map(|path| {
            let path_bytes = path.as_bytes();
            let stage1 = merged.index.get(path_bytes, 1).is_some();
            let stage2 = merged.index.get(path_bytes, 2).is_some();
            let stage3 = merged.index.get(path_bytes, 3).is_some();
            Conflict {
                path,
                kind: conflict_kind_from_stages(stage1, stage2, stage3),
            }
        })
        .collect()
}

/// Whether `merged` has textual conflict blobs or unmerged index stages.
#[must_use]
pub fn tree_merge_has_conflicts(merged: &TreeMergeOutput) -> bool {
    !merged.conflict_content.is_empty() || merged.index.entries().iter().any(|e| e.stage() != 0)
}

/// Render [`NextStep`] values as `grit` command lines (skips steps with no CLI yet).
#[must_use]
pub fn next_step_grit_commands(steps: &[NextStep]) -> Vec<String> {
    steps.iter().filter_map(NextStep::grit_command).collect()
}

fn tree_merge_conflict_path_strings(merged: &TreeMergeOutput) -> Vec<String> {
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

fn conflict_path_bytes_from_index(index: &Index) -> Vec<Vec<u8>> {
    use std::collections::BTreeSet;
    let mut paths = BTreeSet::new();
    for entry in index.entries() {
        let stage = entry.stage();
        if (1..=3).contains(&stage) {
            paths.insert(entry.path.clone());
        }
    }
    paths.into_iter().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::index::{Index, IndexEntry};
    use crate::merge_trees::TreeMergeOutput;
    use std::collections::{BTreeMap, HashSet};

    fn index_entry(path: &str, stage: u8) -> IndexEntry {
        let mut entry = IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
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
    fn conflict_kind_from_stages_content() {
        assert_eq!(
            conflict_kind_from_stages(true, true, true),
            ConflictKind::Content
        );
    }

    #[test]
    fn conflict_kind_from_stages_add_add() {
        assert_eq!(
            conflict_kind_from_stages(false, true, true),
            ConflictKind::AddAdd
        );
    }

    #[test]
    fn conflict_kind_from_stages_modify_delete_deleted_in_current() {
        assert_eq!(
            conflict_kind_from_stages(true, false, true),
            ConflictKind::ModifyDelete {
                deleted_in_current: true
            }
        );
    }

    #[test]
    fn conflict_kind_from_stages_modify_delete_deleted_incoming() {
        assert_eq!(
            conflict_kind_from_stages(true, true, false),
            ConflictKind::ModifyDelete {
                deleted_in_current: false
            }
        );
    }

    #[test]
    fn conflict_kind_as_str_is_stable_snake_case() {
        assert_eq!(ConflictKind::Content.as_str(), "content");
        assert_eq!(ConflictKind::AddAdd.as_str(), "add_add");
        assert_eq!(
            ConflictKind::ModifyDelete {
                deleted_in_current: true
            }
            .as_str(),
            "modify_delete"
        );
    }

    #[test]
    fn conflicts_from_tree_merge_unions_markers_and_unmerged_index() {
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
        let conflicts = conflicts_from_tree_merge(&merged);
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].path, "a.txt");
        assert_eq!(conflicts[0].kind, ConflictKind::Content);
        assert_eq!(conflicts[1].path, "b.txt");
        assert_eq!(
            conflicts[1].kind,
            ConflictKind::ModifyDelete {
                deleted_in_current: false
            }
        );
    }

    #[test]
    fn next_step_grit_command_for_each_operation() {
        use crate::state::InProgressOperation;

        let cases: &[(NextStep, &str)] = &[
            (
                NextStep::Abort(InProgressOperation::Merge),
                "grit merge --abort",
            ),
            (
                NextStep::Continue(InProgressOperation::Merge),
                "grit merge --continue",
            ),
            (
                NextStep::Abort(InProgressOperation::CherryPick),
                "grit pick --abort",
            ),
            (
                NextStep::Continue(InProgressOperation::CherryPick),
                "grit pick --continue",
            ),
            (
                NextStep::Skip(InProgressOperation::CherryPick),
                "grit pick --skip",
            ),
            (
                NextStep::Abort(InProgressOperation::Revert),
                "grit revert --abort",
            ),
            (
                NextStep::Continue(InProgressOperation::Revert),
                "grit revert --continue",
            ),
            (
                NextStep::Skip(InProgressOperation::Revert),
                "grit revert --skip",
            ),
        ];
        for (step, expected) in cases {
            assert_eq!(step.grit_command().as_deref(), Some(*expected), "{step:?}");
            assert_eq!(step.command().as_deref(), Some(*expected), "{step:?}");
            assert_eq!(step.git_command(), None, "{step:?}");
        }
    }

    #[test]
    fn continue_with_git_renders_only_a_git_command() {
        use crate::state::InProgressOperation;

        let cases: &[(InProgressOperation, Option<&str>)] = &[
            (InProgressOperation::Merge, Some("git merge --continue")),
            (InProgressOperation::Rebase, Some("git rebase --continue")),
            (
                InProgressOperation::RebaseInteractive,
                Some("git rebase --continue"),
            ),
            (
                InProgressOperation::CherryPick,
                Some("git cherry-pick --continue"),
            ),
            (InProgressOperation::Revert, Some("git revert --continue")),
            (InProgressOperation::Am, Some("git am --continue")),
            (InProgressOperation::Bisect, None),
        ];
        for (kind, expected) in cases {
            let step = NextStep::ContinueWithGit(*kind);
            assert_eq!(step.grit_command(), None, "{step:?}");
            assert_eq!(step.git_command().as_deref(), *expected, "{step:?}");
            assert_eq!(step.command().as_deref(), *expected, "{step:?}");
        }
        assert_eq!(
            NextStep::Abort(InProgressOperation::RebaseInteractive)
                .command()
                .as_deref(),
            Some("grit rebase --abort")
        );
    }
}
