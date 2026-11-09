//! Tree and content diffs, rename detection, merge diffs, and related helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/diff)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.diff();
//!
//! # Ok(())
//! # }
//! ```
mod basename_rename;
mod changes;
mod commit_files_many;
mod core;
pub(crate) use core::myers_rediff_changed_masks_for_region;
pub(crate) use core::*;
pub(crate) use core::{
    intern_merge_compare_three_way, merge_file_compacted_line_ops, merge_myops_from_preinterned_ids,
};
mod detail;
mod files;
mod filter;
mod git_histogram;
pub(crate) mod handle;
mod has_changes;
pub(crate) mod hunk_id;
mod merge_commit;
mod no_index;
mod options;
mod patch_log;
pub(crate) mod pickaxe;
mod posix_ere;
mod presentation;
mod rename_blob_cache;
mod rename_lines;
mod stat;
pub(crate) use core::is_func_line;
pub(crate) use posix_ere::PosixBre;
pub(crate) mod targets;
mod text;
mod tree_content;
mod tree_options;
pub(crate) use tree_content::gitlink_line;
pub use tree_content::{bytes_for_diff, load_odb_blob_side_for_diff};
mod unified_emit;
mod whitespace_mode;
#[cfg(all(test, feature = "full-tests"))]
mod worktree_scan_git_compat_tests;

pub use crate::submodule_ignore::SubmoduleIgnoreMode;
pub use changes::{FileChange, UncommittedDiff};
pub use commit_files_many::{CommitFilesManyInput, CommitFilesManyOptions, CommitFilesResult};
pub use core::structured;

pub use core::{
    count_git_lines, empty_blob_oid, format_raw, format_raw_abbrev, index_mtime_for_diff,
    mode_from_metadata, normalize_ignore_space_change, parse_diff_rename_threshold_percent,
    read_worktree_side_bytes_for_diff, refresh_index_stat_content_verified,
    rename_similarity_score, sort_entries_by_path, stat_matches, submodule_porcelain_flags,
    tree_oid_of_commit, zero_oid, DiffEntriesResult, DiffEntry, DiffStatus,
    InexactRenameDetectionSkipped,
};
pub use detail::FileDiff as ListedFileDiff;
pub use detail::{DiffDetail, DiffTotals, LineStat};
pub(crate) use files::build_file_diffs;
pub use files::{name_status_line, DiffFilesOutput};
pub use filter::{DiffFilter, InvalidDiffFilter};
pub use handle::Diff;
pub use has_changes::text_sides_differ;
pub use hunk_id::{
    hunk_ids, peer_hunks_by_primary_path, peer_scope_includes_path, DiffSide, HunkId, HunkIdError,
};
pub use merge_commit::{combined_structured_patch_body, DiffMerges};
pub use no_index::{diff_paths, write_paths_patch, NoIndexError};
pub use options::{
    renames_from_config, DiffAlgorithm, DiffOptions, IndentHeuristicOverride, RenameDetection,
    UnknownDiffAlgorithm, RENAME_DETECTION_DISABLED, WORD_LEVEL_INLINE_MAX_CHANGED_FILES,
};
pub(crate) use tree_content::tree_diff_structured_parallel;
pub use tree_options::{BreakRewrites, CopyDetection, TreeDiffOptions};
pub use whitespace_mode::{normalize_ignore_space_change_line, WhitespaceMode};

pub(crate) use core::{imara_unified_hunk_slices, unified_hunk_body_for_diff_options};
pub use patch_log::{
    write_commit_header_and_message, write_commit_log_patch, write_commits_log_patch,
    write_diff_entry_log_patch, write_patch, write_patch_resolved, write_patch_structured,
    LogPatchBinaryFormat, LogPatchCommitHeader, LogPatchOptions, PatchResolvedSide, PatchSide,
    PatchTextOptions,
};
pub use pickaxe::{Pickaxe, PickaxeError, PickaxeKind};
pub use presentation::{
    annotate_structured_hunks, include_file_change_in_patch, line_stat_for_target_file,
    presentation_hunks_from_file_change, presentation_hunks_from_listed,
    presentation_hunks_from_tree, should_report_inexact_rename_skipped, AnnotatedStructuredHunk,
};
pub use stat::{DiffStat, FileStat, StatBar};
pub use targets::HunkDiffBundle;
pub(crate) use targets::{
    collect_files_for_target, include_file_in_diff, unstaged_file_diffs_with_peer_hunks,
    unstaged_raw_changes_with_peer_hunks, unstaged_raw_changes_with_peer_hunks_in,
};
pub use targets::{DiffArguments, DiffRequest, DiffTarget, FileDiff, ResolveDiffArgumentsError};
pub use text::{
    line_change_counts, unified_diff, DiffLabel, HunkExtent, IndentHeuristic, NoNewlineMarker,
    PathQuoting, UnifiedDiffOptions,
};
pub use tree_content::{tree_diff_structured_entry, TreeDiffStructured};

pub(crate) mod bloom {
    pub(crate) use crate::bloom::*;
}
pub mod combined_diff_patch {
    //! combined_diff_patch API (re-exported at this path).
    pub use crate::combined_diff_patch::*;
}
pub mod combined_tree_diff {
    //! Combined tree diff helpers for merge commits.
    pub use crate::combined_tree_diff::*;
}
pub mod patch_ids {
    //! patch_ids API (re-exported at this path).
    pub use crate::patch_ids::*;
}
pub mod userdiff {
    //! Per-path diff driver (funcname) matching.
    pub use crate::userdiff::*;
}
pub mod merge_diff {
    //! Combined diff content for octopus merges.
    //!
    //! Canonical path: [`crate::merge::merge_diff`].
    pub use crate::merge::merge_diff::*;
}
pub mod merge_file {
    //! merge_file API (re-exported at this path).
    pub use crate::merge_file::*;
}
pub mod merge_trees {
    //! Tree-level three-way merge engine.
    //!
    //! Canonical path: [`crate::merge::merge_trees`].
    pub use crate::merge::merge_trees::*;
}
pub mod rerere {
    //! Recorded conflict resolution (`rerere`).
    //!
    //! Repository operations: [`crate::merge::MergeRerere`] from [`crate::repo::Repository::merge`].
    pub use crate::rerere::*;
}
/// Merge commit message formatting from FETCH_HEAD-style input.
pub mod fmt_merge_msg {
    pub use crate::fmt_merge_msg::{fmt_merge_msg, FmtMergeMsgOptions};
}
pub mod ws {
    //! ASCII whitespace helpers used by the diff engine.
    pub use crate::ws::*;
}
