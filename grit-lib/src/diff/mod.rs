//! Tree and content diffs, rename detection, merge diffs, and related helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/diff)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.diff();
//!
//! # Ok(())
//! # }
//! ```
mod changes;
mod commit_files_many;
mod core;
pub(crate) use core::myers_rediff_changed_masks_for_region;
mod detail;
mod files;
mod filter;
mod git_histogram;
pub mod handle;
mod has_changes;
pub mod hunk_id;
mod merge_commit;
mod no_index;
mod options;
mod patch_log;
pub mod pickaxe;
mod posix_ere;
mod rename_lines;
mod stat;
pub(crate) use core::is_func_line;
pub(crate) use posix_ere::PosixBre;
mod targets;
mod tree_content;
pub(crate) use tree_content::gitlink_line;
pub use tree_content::{bytes_for_diff, load_odb_blob_side_for_diff};
mod unified_emit;
mod whitespace_mode;

pub use changes::{load_object_changes, FileChange, UncommittedDiff};
pub use commit_files_many::{CommitFilesManyInput, CommitFilesManyOptions, CommitFilesResult};
pub use core::*;
pub use detail::FileDiff as ListedFileDiff;
pub use detail::{line_change_counts, DiffDetail, DiffTotals, LineStat};
pub use files::{build_file_diffs, build_file_diffs_streaming, name_status_line, DiffFilesOutput};
pub use filter::{DiffFilter, InvalidDiffFilter};
pub use handle::Diff;
pub use has_changes::{
    text_sides_differ, HAS_CHANGES_CALL_COUNT, WORKTREE_HAS_CHANGES_ENTRIES_SCANNED,
};
pub use hunk_id::{
    hunk_ids, hunk_ids_with_duplicate_peers, peer_hunks_by_primary_path, peer_scope_includes_path,
    DiffSide, HunkId, HunkIdError,
};
pub use merge_commit::{combined_structured_patch_body, commit_diff_files, DiffMerges};
pub use no_index::{diff_paths, write_paths_patch, NoIndexError};
pub use options::{
    renames_from_config, DiffAlgorithm, DiffOptions, RenameDetection, UnknownDiffAlgorithm,
    RENAME_DETECTION_DISABLED, WORD_LEVEL_INLINE_MAX_CHANGED_FILES,
};
pub use whitespace_mode::{normalize_ignore_space_change_line, WhitespaceMode};

/// Unified hunk bodies (`@@` …) for two text sides using [`DiffOptions`].
#[must_use]
pub fn line_diff_unified_hunk_body(
    old_content: &str,
    new_content: &str,
    options: &DiffOptions,
) -> String {
    core::unified_hunk_body_for_diff_options(old_content, new_content, options)
}
pub(crate) use core::{imara_unified_hunk_slices, unified_hunk_body_for_diff_options};
pub use patch_log::{
    write_commit_header_and_message, write_commit_log_patch, write_commits_log_patch,
    write_diff_entry_log_patch, write_patch, write_patch_resolved, write_patch_structured,
    LogPatchBinaryFormat, LogPatchCommitHeader, LogPatchOptions, PatchResolvedSide, PatchSide,
    PatchTextOptions,
};
pub use pickaxe::{Pickaxe, PickaxeError, PickaxeKind};
pub use stat::{DiffStat, FileStat, StatBar};
pub use targets::hunk_diff_bundle;
pub use targets::HunkDiffBundle;
pub(crate) use targets::{
    collect_files_for_target, include_file_in_diff, unstaged_file_diffs_with_peer_hunks,
    unstaged_raw_changes_with_peer_hunks, unstaged_raw_changes_with_peer_hunks_in,
};
pub use targets::{
    resolve_commit_diff_pathspecs, resolve_diff_arguments, resolve_diff_request, DiffArguments,
    DiffRequest, DiffTarget, FileDiff, ResolveDiffArgumentsError,
};
pub use tree_content::{
    for_each_tree_diff_structured, tree_diff_contents_parallel, tree_diff_stat_parallel,
    tree_diff_structured_entry, tree_diff_structured_parallel, TreeDiffContent, TreeDiffStructured,
};

pub mod bloom {
    //! bloom API (re-exported at this path).
    pub use crate::bloom::*;
}
pub mod combined_diff_patch {
    //! combined_diff_patch API (re-exported at this path).
    pub use crate::combined_diff_patch::*;
}
pub mod combined_tree_diff {
    //! combined_tree_diff API (re-exported at this path).
    pub use crate::combined_tree_diff::*;
}
pub mod patch_ids {
    //! patch_ids API (re-exported at this path).
    pub use crate::patch_ids::*;
}
pub mod userdiff {
    //! userdiff API (re-exported at this path).
    pub use crate::userdiff::*;
}
pub mod merge_diff {
    //! merge_diff API (re-exported at this path).
    pub use crate::merge_diff::*;
}
pub mod merge_file {
    //! merge_file API (re-exported at this path).
    pub use crate::merge_file::*;
}
pub mod merge_trees {
    //! merge_trees API (re-exported at this path).
    pub use crate::merge_trees::*;
}
pub mod rerere {
    //! rerere API (re-exported at this path).
    pub use crate::rerere::*;
}
pub mod fmt_merge_msg {
    //! fmt_merge_msg API (re-exported at this path).
    pub use crate::fmt_merge_msg::*;
}
pub mod ws {
    //! ASCII whitespace helpers used by the diff engine.
    pub use crate::ws::*;
}
