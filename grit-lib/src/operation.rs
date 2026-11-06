//! Shared conflict classification and in-progress operation vocabulary.
//!
//! Status, merge, pick, revert, rebase, pull and stash all report conflicts with the same
//! [`Conflict`] and [`ConflictKind`]. The kind is read from which unmerged index stages a
//! path has (the same classification as the `u` lines of `git status --porcelain=v2`), so a
//! conflict looks the same whether it comes from an operation's outcome or from
//! [`status`](crate::porcelain::status::status) afterwards. Only a tree-merge result can add
//! [`ConflictKind::RenameRename`] and the directory rename kinds
//! ([`ConflictKind::DirectoryRenameMoved`], [`ConflictKind::DirectoryRenameSplit`] and
//! [`ConflictKind::DirectoryRenameBlocked`]), which need information the index does not keep.
//!
//! ```rust
//! use grit_lib::operation::{conflict_kind_from_stagemask, ConflictKind};
//!
//! // Stage 1 (base) and stage 3 (theirs) only: we deleted the path, they kept it.
//! assert_eq!(
//!     conflict_kind_from_stagemask(0b101),
//!     Some(ConflictKind::DeletedByUs)
//! );
//! assert_eq!(ConflictKind::DeletedByUs.as_str(), "deleted_by_us");
//! ```

use crate::diff::merge_trees::{DirectoryRenameConflict, TreeMergeOutput};
use crate::index::Index;
use crate::state::NextStep;
use std::collections::{BTreeMap, HashMap};

/// How the two sides of a merge collided on one path.
///
/// "Us" is the side checked out when the operation started (index stage 2, `HEAD`); "them"
/// is the side being brought in (stage 3): the merged branch, the picked or reverted commit,
/// or the commit a rebase is replaying. Variants are ordered as `grit status` lists them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(
    feature = "serde-models",
    serde(tag = "kind", rename_all = "snake_case")
)]
pub enum ConflictKind {
    /// Both sides changed the file (stages 1, 2 and 3; porcelain `UU`).
    BothModified,
    /// Both sides added the path with different content (stages 2 and 3; `AA`).
    BothAdded,
    /// Both sides removed the path (stage 1 only; `DD`).
    BothDeleted,
    /// Only our side has the path (stage 2 only; `AU`).
    AddedByUs,
    /// Only their side has the path (stage 3 only; `UA`).
    AddedByThem,
    /// We deleted the path and they changed it (stages 1 and 3; `DU`).
    DeletedByUs,
    /// We changed the path and they deleted it (stages 1 and 2; `UD`).
    DeletedByThem,
    /// Both sides renamed the same base file, to different paths.
    ///
    /// Reported once for each of the three paths involved; [`Conflict::path`] says which one.
    /// Only [`conflicts_from_tree_merge`] produces it: the index alone shows the three paths
    /// as [`Self::BothDeleted`], [`Self::AddedByUs`] and [`Self::AddedByThem`]. The paths are
    /// boxed so every other kind stays small in the long conflict lists `status` builds.
    RenameRename(Box<RenamedPaths>),
    /// A file one side added inside a directory the other side renamed was moved into the new
    /// directory, and left unmerged at the adding side's stage so the user confirms the move.
    ///
    /// [`Conflict::path`] is the new location. Only [`conflicts_from_tree_merge`] produces it;
    /// the index shows the path as [`Self::AddedByUs`] or [`Self::AddedByThem`] (or by its
    /// stages, when the file also has content conflicts).
    DirectoryRenameMoved(Box<MovedPath>),
    /// One side spread a directory's files over several directories with no clear winner, so
    /// files the other side added to it were left in place. [`Conflict::path`] is the directory;
    /// no index entry is unmerged.
    DirectoryRenameSplit,
    /// Files were not moved into a renamed directory because their destination is taken.
    /// [`Conflict::path`] is the destination; no index entry is unmerged.
    DirectoryRenameBlocked(Box<BlockedMove>),
}

/// Where a [`ConflictKind::DirectoryRenameMoved`] file came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct MovedPath {
    /// Path the adding side gave the file, inside the renamed directory's old name.
    pub original_path: String,
}

/// The files a [`ConflictKind::DirectoryRenameBlocked`] directory rename did not move.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct BlockedMove {
    /// New files that would have moved to the destination; they stay where they were added.
    pub sources: Vec<String>,
    /// `true` when the adding side already has a file or directory at the destination; `false`
    /// when several new files would land on it.
    pub existing: bool,
}

/// The three names in a [`ConflictKind::RenameRename`] conflict.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct RenamedPaths {
    /// Path of the file in the merge base (left at stage 1).
    pub base_path: String,
    /// Name our side gave it (stage 2).
    pub ours_path: String,
    /// Name their side gave it (stage 3).
    pub theirs_path: String,
}

impl ConflictKind {
    /// Stable snake_case name for JSON and scripting.
    ///
    /// One of `both_modified`, `both_added`, `both_deleted`, `added_by_us`, `added_by_them`,
    /// `deleted_by_us`, `deleted_by_them`, `rename_rename`, `directory_rename_moved`,
    /// `directory_rename_split` or `directory_rename_blocked`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BothModified => "both_modified",
            Self::BothAdded => "both_added",
            Self::BothDeleted => "both_deleted",
            Self::AddedByUs => "added_by_us",
            Self::AddedByThem => "added_by_them",
            Self::DeletedByUs => "deleted_by_us",
            Self::DeletedByThem => "deleted_by_them",
            Self::RenameRename(_) => "rename_rename",
            Self::DirectoryRenameMoved(_) => "directory_rename_moved",
            Self::DirectoryRenameSplit => "directory_rename_split",
            Self::DirectoryRenameBlocked(_) => "directory_rename_blocked",
        }
    }
}

/// One conflicted path, as reported by status and by every operation that can stop on
/// conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct Conflict {
    /// Repository-relative path.
    pub path: String,
    /// How the two sides collided on [`Self::path`].
    #[cfg_attr(feature = "serde-models", serde(flatten))]
    pub kind: ConflictKind,
}

/// Classify a path from the set of unmerged index stages it has.
///
/// `stagemask` has bit 0 set for stage 1 (base), bit 1 for stage 2 (ours) and bit 2 for
/// stage 3 (theirs).
///
/// # Returns
///
/// The [`ConflictKind`] for that combination, or `None` when `stagemask` has no stage bit
/// set or has bits above stage 3 (such a path is not unmerged).
#[must_use]
pub fn conflict_kind_from_stagemask(stagemask: u8) -> Option<ConflictKind> {
    Some(match stagemask {
        0b001 => ConflictKind::BothDeleted,
        0b010 => ConflictKind::AddedByUs,
        0b011 => ConflictKind::DeletedByThem,
        0b100 => ConflictKind::AddedByThem,
        0b101 => ConflictKind::DeletedByUs,
        0b110 => ConflictKind::BothAdded,
        0b111 => ConflictKind::BothModified,
        _ => return None,
    })
}

/// Classify every unmerged path in `index`.
///
/// # Returns
///
/// One [`Conflict`] per path with an entry at stage 1, 2 or 3, in index (byte) order. Paths
/// use [`crate::repo_relpath::storage_string`], so non-UTF-8 paths round-trip.
#[must_use]
pub fn conflicts_from_index(index: &Index) -> Vec<Conflict> {
    let mut masks: BTreeMap<&[u8], u8> = BTreeMap::new();
    for entry in index.entries() {
        let stage = entry.stage();
        if (1..=3).contains(&stage) {
            *masks.entry(entry.path.as_slice()).or_default() |= 1 << (stage - 1);
        }
    }
    masks
        .into_iter()
        .filter_map(|(path, mask)| {
            Some(Conflict {
                path: crate::repo_relpath::storage_string(path),
                kind: conflict_kind_from_stagemask(mask)?,
            })
        })
        .collect()
}

/// Collect conflicted paths and kinds from a three-way tree merge result.
///
/// Paths of a [`TreeMergeOutput::rename_conflicts`] entry are reported as
/// [`ConflictKind::RenameRename`], and each [`TreeMergeOutput::directory_rename_conflicts`]
/// entry as the matching directory rename kind at its [`DirectoryRenameConflict::path`]; every
/// other path is classified by its unmerged stages, like [`conflicts_from_index`]. A path that
/// only has conflict-marker content and no unmerged stages is [`ConflictKind::BothModified`].
/// The result is sorted by path bytes, and paths use [`crate::repo_relpath::storage_string`]
/// (as [`conflicts_from_index`] does), so non-UTF-8 paths round-trip.
#[must_use]
pub fn conflicts_from_tree_merge(merged: &TreeMergeOutput) -> Vec<Conflict> {
    use crate::repo_relpath::storage_string;

    let mut known_kinds: HashMap<&[u8], ConflictKind> = HashMap::new();
    let mut conflicts: Vec<(&[u8], ConflictKind)> = Vec::new();
    for conflict in &merged.directory_rename_conflicts {
        let path = conflict.path();
        match conflict {
            DirectoryRenameConflict::Moved { from, .. } => {
                known_kinds.insert(
                    path,
                    ConflictKind::DirectoryRenameMoved(Box::new(MovedPath {
                        original_path: storage_string(from),
                    })),
                );
            }
            DirectoryRenameConflict::Split { .. } => {
                conflicts.push((path, ConflictKind::DirectoryRenameSplit));
            }
            DirectoryRenameConflict::Blocked {
                sources, existing, ..
            } => conflicts.push((
                path,
                ConflictKind::DirectoryRenameBlocked(Box::new(BlockedMove {
                    sources: sources.iter().map(|s| storage_string(s)).collect(),
                    existing: *existing,
                })),
            )),
        }
    }
    for rr in &merged.rename_conflicts {
        let kind = ConflictKind::RenameRename(Box::new(RenamedPaths {
            base_path: storage_string(&rr.base_path),
            ours_path: storage_string(&rr.ours_path),
            theirs_path: storage_string(&rr.theirs_path),
        }));
        for path in [&rr.base_path, &rr.ours_path, &rr.theirs_path] {
            known_kinds.insert(path.as_slice(), kind.clone());
        }
    }
    for path in tree_merge_conflict_paths(merged) {
        let kind = known_kinds.get(path).cloned().unwrap_or_else(|| {
            let mask = (1..=3u8)
                .filter(|&stage| merged.index.get(path, stage).is_some())
                .fold(0u8, |mask, stage| mask | 1 << (stage - 1));
            conflict_kind_from_stagemask(mask).unwrap_or(ConflictKind::BothModified)
        });
        conflicts.push((path, kind));
    }
    conflicts.sort_by(|a, b| a.0.cmp(b.0));
    conflicts
        .into_iter()
        .map(|(path, kind)| Conflict {
            path: storage_string(path),
            kind,
        })
        .collect()
}

/// Whether `merged` has textual conflict blobs, unmerged index stages or directory renames
/// that need the user's attention.
#[must_use]
pub fn tree_merge_has_conflicts(merged: &TreeMergeOutput) -> bool {
    !merged.conflict_content.is_empty()
        || !merged.directory_rename_conflicts.is_empty()
        || merged.index.entries().iter().any(|e| e.stage() != 0)
}

/// Render [`NextStep`] values as `grit` command lines (skips steps with no CLI yet).
#[must_use]
pub fn next_step_grit_commands(steps: &[NextStep]) -> Vec<String> {
    steps.iter().filter_map(NextStep::grit_command).collect()
}

/// Conflicted paths of a tree merge (marker blobs and unmerged stages), sorted and deduplicated.
fn tree_merge_conflict_paths(merged: &TreeMergeOutput) -> Vec<&[u8]> {
    let mut paths: Vec<&[u8]> = merged
        .conflict_content
        .keys()
        .map(Vec::as_slice)
        .chain(
            merged
                .index
                .entries()
                .iter()
                .filter(|e| e.stage() != 0)
                .map(|e| e.path.as_slice()),
        )
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
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
    fn conflict_kind_from_stagemask_covers_every_unmerged_combination() {
        let cases = [
            (0b001, ConflictKind::BothDeleted),
            (0b010, ConflictKind::AddedByUs),
            (0b011, ConflictKind::DeletedByThem),
            (0b100, ConflictKind::AddedByThem),
            (0b101, ConflictKind::DeletedByUs),
            (0b110, ConflictKind::BothAdded),
            (0b111, ConflictKind::BothModified),
        ];
        for (mask, kind) in cases {
            assert_eq!(conflict_kind_from_stagemask(mask), Some(kind), "{mask:#b}");
        }
    }

    #[test]
    fn conflict_kind_from_stagemask_rejects_masks_without_unmerged_stages() {
        assert_eq!(conflict_kind_from_stagemask(0), None);
        assert_eq!(conflict_kind_from_stagemask(0b1000), None);
        assert_eq!(conflict_kind_from_stagemask(0xff), None);
    }

    #[test]
    fn conflict_kind_as_str_is_stable_snake_case() {
        let cases = [
            (ConflictKind::BothModified, "both_modified"),
            (ConflictKind::BothAdded, "both_added"),
            (ConflictKind::BothDeleted, "both_deleted"),
            (ConflictKind::AddedByUs, "added_by_us"),
            (ConflictKind::AddedByThem, "added_by_them"),
            (ConflictKind::DeletedByUs, "deleted_by_us"),
            (ConflictKind::DeletedByThem, "deleted_by_them"),
            (
                ConflictKind::RenameRename(Box::new(RenamedPaths {
                    base_path: "a".into(),
                    ours_path: "b".into(),
                    theirs_path: "c".into(),
                })),
                "rename_rename",
            ),
        ];
        for (kind, name) in cases {
            assert_eq!(kind.as_str(), name);
        }
    }

    #[test]
    fn conflict_kind_stays_two_words() {
        assert!(std::mem::size_of::<ConflictKind>() <= 2 * std::mem::size_of::<usize>());
    }

    #[test]
    fn conflicts_from_index_groups_stages_per_path_in_byte_order() {
        let mut index = Index::new();
        index.add_or_replace(index_entry("z", 1));
        index.add_or_replace(index_entry("z", 3));
        index.add_or_replace(index_entry("clean", 0));
        index.add_or_replace(index_entry("a", 2));
        index.add_or_replace(index_entry("a", 3));
        let conflicts = conflicts_from_index(&index);
        assert_eq!(
            conflicts,
            vec![
                Conflict {
                    path: "a".into(),
                    kind: ConflictKind::BothAdded,
                },
                Conflict {
                    path: "z".into(),
                    kind: ConflictKind::DeletedByUs,
                },
            ]
        );
    }

    #[test]
    fn conflicts_from_index_non_utf8_path_uses_storage_string() {
        let mut index = Index::new();
        for stage in 1..=3 {
            let mut entry = index_entry("", stage);
            entry.path = vec![0xff, b'a'];
            index.add_or_replace(entry);
        }
        let conflicts = conflicts_from_index(&index);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::BothModified);
        assert_eq!(
            crate::repo_relpath::storage_bytes(&conflicts[0].path).as_ref(),
            [0xff, b'a']
        );
    }

    #[test]
    fn conflicts_from_tree_merge_reports_rename_rename_on_all_three_paths() {
        let mut index = Index::new();
        index.add_or_replace(index_entry("a", 1));
        index.add_or_replace(index_entry("b", 2));
        index.add_or_replace(index_entry("c", 3));
        index.add_or_replace(index_entry("d", 1));
        index.add_or_replace(index_entry("d", 2));
        let merged = TreeMergeOutput {
            index,
            conflict_content: BTreeMap::new(),
            result_tree: crate::diff::zero_oid(),
            touched_paths: HashSet::new(),
            rename_conflicts: vec![crate::merge_trees::RenameRenameConflict {
                base_path: b"a".to_vec(),
                ours_path: b"b".to_vec(),
                theirs_path: b"c".to_vec(),
            }],
            directory_rename_conflicts: Vec::new(),
        };
        let kind = ConflictKind::RenameRename(Box::new(RenamedPaths {
            base_path: "a".into(),
            ours_path: "b".into(),
            theirs_path: "c".into(),
        }));
        let conflicts = conflicts_from_tree_merge(&merged);
        let got: Vec<(&str, &ConflictKind)> = conflicts
            .iter()
            .map(|c| (c.path.as_str(), &c.kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a", &kind),
                ("b", &kind),
                ("c", &kind),
                ("d", &ConflictKind::DeletedByThem),
            ]
        );
    }

    #[test]
    fn conflicts_from_tree_merge_keeps_non_utf8_directory_rename_paths() {
        use crate::merge_trees::{DirectoryRenameConflict, MergeSide};
        use crate::repo_relpath::storage_string;

        let from = b"a/n\xff".to_vec();
        let to = b"b/n\xff".to_vec();
        let mut row = index_entry("x", 3);
        row.path = to.clone();
        let mut index = Index::new();
        index.add_or_replace(row);
        let merged = TreeMergeOutput {
            index,
            conflict_content: BTreeMap::new(),
            result_tree: crate::diff::zero_oid(),
            touched_paths: HashSet::new(),
            rename_conflicts: Vec::new(),
            directory_rename_conflicts: vec![
                DirectoryRenameConflict::Moved {
                    from: from.clone(),
                    to: to.clone(),
                    added_by: MergeSide::Theirs,
                },
                DirectoryRenameConflict::Split {
                    dir: b"c\xfe".to_vec(),
                    renamed_by: MergeSide::Ours,
                },
            ],
        };
        assert_eq!(
            conflicts_from_tree_merge(&merged),
            vec![
                Conflict {
                    path: storage_string(&to),
                    kind: ConflictKind::DirectoryRenameMoved(Box::new(MovedPath {
                        original_path: storage_string(&from),
                    })),
                },
                Conflict {
                    path: storage_string(b"c\xfe"),
                    kind: ConflictKind::DirectoryRenameSplit,
                },
            ]
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
            rename_conflicts: Vec::new(),
            directory_rename_conflicts: Vec::new(),
        };
        let conflicts = conflicts_from_tree_merge(&merged);
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].path, "a.txt");
        assert_eq!(conflicts[0].kind, ConflictKind::BothModified);
        assert_eq!(conflicts[1].path, "b.txt");
        assert_eq!(conflicts[1].kind, ConflictKind::DeletedByThem);
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
