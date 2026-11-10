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
pub(crate) mod funcname_attrs;
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
pub(crate) use hunk_id::peer_scope_includes_path;
pub use hunk_id::HunkIdError;
pub use hunk_id::{hunk_ids, peer_hunks_by_primary_path, DiffSide, HunkId};
pub use merge_commit::DiffMerges;
pub use no_index::{diff_paths, write_paths_patch, NoIndexError};
pub use options::UnknownDiffAlgorithm;
pub use options::{
    renames_from_config, DiffAlgorithm, DiffOptions, IndentHeuristicOverride, RenameDetection,
    RENAME_DETECTION_DISABLED, WORD_LEVEL_INLINE_MAX_CHANGED_FILES,
};
pub(crate) use tree_content::tree_diff_structured_parallel;
pub use tree_options::{BreakRewrites, CopyDetection, TreeDiffOptions};
pub use whitespace_mode::WhitespaceMode;

pub(crate) use core::{imara_unified_hunk_slices, unified_hunk_body_for_diff_options};
pub use patch_log::{
    write_commit_log_patch, write_commits_log_patch, write_patch, write_patch_resolved,
    write_patch_structured, LogPatchBinaryFormat, LogPatchCommitHeader, LogPatchOptions,
    PatchResolvedSide, PatchSide, PatchTextOptions,
};
pub use pickaxe::{Pickaxe, PickaxeError, PickaxeKind};
pub use presentation::{
    annotate_structured_hunks, include_file_change_in_patch, line_stat_for_target_file,
    presentation_hunks_from_listed, presentation_hunks_from_tree,
    should_report_inexact_rename_skipped, AnnotatedStructuredHunk,
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
pub use tree_content::TreeDiffStructured;

pub(crate) mod bloom {
    pub(crate) use crate::bloom::*;
}
pub mod combined_diff_patch {
    //! combined_diff_patch API (re-exported at this path).

    pub use crate::combined_diff_patch::CombinedStructuredHunk;
    pub use crate::combined_diff_patch::CombinedStructuredLine;
    pub use crate::combined_diff_patch::CombinedStructuredTextDiff;
    pub use crate::combined_diff_patch::{
        format_combined_diff_body, format_combined_structured_body, structured_combined_diff_body,
        CombinedDiffWsOptions, CombinedStructuredLineKind,
    };
}
pub mod combined_tree_diff {
    //! Combined tree diff helpers for merge commits.
    pub use crate::combined_tree_diff::{
        combined_diff_paths_filtered, format_combined_raw_line, format_combined_raw_line_all_paths,
        format_combined_raw_line_all_paths_with_options, CombinedDiffPath, CombinedParentSide,
        CombinedParentStatus, CombinedRawLineFormatOptions, CombinedTreeDiffOptions,
    };
    pub(crate) use crate::combined_tree_diff::{
        combined_diff_paths_trees, combined_parent_status_char,
    };
}
pub mod patch_ids {
    //! patch_ids API (re-exported at this path).
    pub use crate::patch_ids::{
        compute_patch_id, compute_patch_id_for_paths, compute_patch_ids_from_text, PatchIdMode,
    };
}
pub mod userdiff {
    //! Per-path diff driver (funcname) matching.

    pub use crate::userdiff::FuncnamePatternError;
    pub use crate::userdiff::{matcher_for_driver, matcher_for_path_parsed, FuncnameMatcher};
}
pub mod merge_diff {
    //! Combined diff content for octopus merges.
    //!
    //! Canonical path: [`crate::merge::merge_diff`].
    pub use crate::merge::merge_diff::{
        all_blob_paths_in_tree_order, blob_oid_at_path, blob_text_for_diff,
        blob_text_for_diff_with_oid, combined_diff_paths, combined_merge_parent_blob_paths,
        convert_blob_to_worktree_for_path, diff_attr_external_driver, diff_attr_forces_binary,
        diff_attr_forces_text, diff_cachetextconv_enabled, diff_driver_forces_text,
        diff_forced_binary_by_driver, diff_textconv_active, diff_textconv_cmd_line,
        enrich_combined_path_renames, format_combined_binary, format_combined_binary_header,
        format_combined_binary_header_n, format_combined_textconv_patch,
        format_worktree_conflict_combined, is_binary_for_diff, read_blob_at_path, run_textconv,
        run_textconv_raw, CombinedDiffFormat, CombinedPatchOptions, CombinedPathLabels,
        ConflictStages, Textconv,
    };
}
pub mod merge_file {
    //! merge_file API (re-exported at this path).
    pub use crate::merge_file::{
        is_binary, merge, ConflictStyle, MergeFavor, MergeInput, MergeOutput,
    };
}
pub mod merge_trees {
    //! Tree-level three-way merge engine.
    //!
    //! Canonical path: [`crate::merge::merge_trees`].
    pub use crate::merge::merge_trees::{
        apply_tree_merge_output_to_index, apply_tree_merge_output_to_worktree_index,
        index_tree_oid_matches_head, materialized_merge_result_tree, merge_trees_three_way,
        refresh_tree_merge_index_stat_cache, DirectoryRenameConflict, DirectoryRenameMode,
        MergeSide, RenameRenameConflict, TheirsConflictLabel, TreeMergeConflictPresentation,
        TreeMergeOutput, WhitespaceMergeOptions,
    };
}
pub mod rerere {
    //! Recorded conflict resolution (`rerere`).
    //!
    //! Repository operations: [`crate::merge::MergeRerere`] from [`crate::repo::Repository::merge`].

    pub use crate::rerere::{
        rerere_conflict_id, rerere_enabled, rerere_gc, RerereAutoupdate, RerereEvent,
        RerereEventKind,
    };
}
/// Merge commit message formatting from FETCH_HEAD-style input.
pub mod fmt_merge_msg {
    pub use crate::fmt_merge_msg::{fmt_merge_msg, FmtMergeMsgOptions};
}
pub mod ws {
    //! ASCII whitespace helpers used by the diff engine.
    pub use crate::ws::{is_space_git, ws_blank_line};
}
