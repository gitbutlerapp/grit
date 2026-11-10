//! Merge, pick, revert, replay and rerere for one [`Repository`](crate::repo::Repository).
//!
//! Obtain [`Merge`] from [`Repository::merge`](crate::repo::Repository::merge). Branch
//! integration, conflict prediction, merge `--continue` / `--abort`, cherry-pick and revert
//! sequences, and rerere all live on that handle rather than as free functions taking
//! [`Repository`](crate::repo::Repository).
//!
//! Library guide: [embedding](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::merge::{MergeOptions, MergePredictionOptions};
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = repo.merge();
//! # Ok(())
//! # }
//! ```

mod handle;
mod rerere;

pub use crate::rerere::{RerereAutoupdate, RerereEvent, RerereEventKind};
pub use handle::{BranchIntegrateRequest, Merge};
pub use rerere::MergeRerere;

pub use crate::porcelain::merge::{
    ContinueOptions, ContinueOutcome, FastForwardPolicy, IntegrateBranchOptions,
    IntegrateBranchOutcome, MergeCommitMode, MergeDryRunPlan, MergeOptions, MergePrediction,
    MergePredictionOptions, MergeSource, MergeSourceKind, MergeTreeStage, WrittenMergeTree,
};
pub use crate::porcelain::merge_error::MergeError;
pub use crate::porcelain::replay::{ReplayDirection, ReplayOutcome, ReplayRequest};
pub use crate::porcelain::replay_sequence::{
    ReplayAbortOutcome, ReplayCreated, ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest,
    SequenceOutcome, SequencePrediction,
};
pub mod merge_diff {
    //! Combined diff content for merge commits.
    pub use crate::merge_diff::{
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
    //! Single-file three-way merge.
    pub use crate::merge_file::{
        is_binary, merge, ConflictStyle, MergeFavor, MergeInput, MergeOutput,
    };
}
pub mod merge_trees {
    //! Tree-level three-way merge engine.
    pub use crate::merge_trees::{
        apply_tree_merge_output_to_index, apply_tree_merge_output_to_worktree_index,
        index_tree_oid_matches_head, materialized_merge_result_tree, merge_trees_three_way,
        refresh_tree_merge_index_stat_cache, DirectoryRenameConflict, DirectoryRenameMode,
        MergeSide, RenameRenameConflict, TheirsConflictLabel, TreeMergeConflictPresentation,
        TreeMergeOutput, WhitespaceMergeOptions,
    };
}
