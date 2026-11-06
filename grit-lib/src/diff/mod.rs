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
mod rename_blob_cache;
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
pub use core::structured;
/// Unified diff for two text sides with default `a/` / `b/` path prefixes.
#[must_use]
pub fn unified_diff(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    core::unified_diff(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        indent_heuristic,
        quote_path_fully,
    )
}

pub use core::{
    anchored_unified_diff, annotate_unified_diff_no_newline_at_eof, count_changes_with_algorithm,
    count_git_lines, detect_copies_with_policy, detect_renames_with_policy, diff_commits,
    diff_commits_with_renames, diff_index_to_tree, diff_index_to_worktree,
    diff_index_to_worktree_for_staging, diff_index_to_worktree_with_options,
    diff_index_to_worktree_with_renames, diff_tree_to_worktree, diff_tree_to_worktree_with_git_dir,
    diff_tree_to_worktree_with_git_dir_and_rules, diff_trees, diff_trees_show_tree_entries,
    diff_trees_with_renames, empty_blob_oid, format_raw, format_raw_abbrev, hash_worktree_file,
    index_mtime_for_diff, mode_from_metadata, normalize_ignore_space_change,
    parse_diff_rename_threshold_percent, postprocess_diff_entries,
    read_worktree_side_bytes_for_diff, refresh_index_stat_content_verified,
    rename_similarity_score, resolve_indent_heuristic, sort_entries_by_path, stat_matches,
    submodule_porcelain_flags, tree_oid_of_commit, unified_diff_histogram_hunks_only,
    unified_diff_histogram_ignore_with_prefix_and_funcname, unified_diff_with_prefix_and_funcname,
    unified_diff_with_prefix_and_funcname_and_algorithm, zero_oid, DetectRenamesOptions,
    DiffEntriesResult, DiffEntry, DiffIndexToWorktreeOptions, DiffPostprocessOptions,
    DiffPostprocessWorktree, DiffStatus, InexactRenameDetectionSkipped,
};
pub use detail::FileDiff as ListedFileDiff;
pub use detail::{line_change_counts, DiffDetail, DiffTotals, LineStat};
pub use files::{build_file_diffs, build_file_diffs_streaming, name_status_line, DiffFilesOutput};
pub use filter::{DiffFilter, InvalidDiffFilter};
pub use handle::Diff;
pub use has_changes::text_sides_differ;
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
/// Merge commit message formatting from FETCH_HEAD-style input.
pub mod fmt_merge_msg {
    pub use crate::fmt_merge_msg::{fmt_merge_msg, FmtMergeMsgOptions};
}
pub mod ws {
    //! ASCII whitespace helpers used by the diff engine.
    pub use crate::ws::*;
}
