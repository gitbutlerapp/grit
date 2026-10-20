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
mod core;
mod filter;
pub mod handle;
mod options;
mod patch_log;
mod targets;
mod tree_content;
mod unified_emit;

pub use core::*;
pub use filter::{DiffFilter, InvalidDiffFilter};
pub use handle::Diff;
pub use options::{
    renames_from_config, DiffAlgorithm, DiffOptions, RenameDetection, UnknownDiffAlgorithm,
    WORD_LEVEL_INLINE_MAX_CHANGED_FILES,
};
pub use patch_log::{
    write_commit_header_and_message, write_commit_log_patch, write_commits_log_patch,
    write_diff_entry_log_patch, LogPatchCommitHeader, LogPatchOptions,
};
pub use targets::{
    resolve_diff_arguments, DiffArguments, DiffTarget, FileDiff, ResolveDiffArgumentsError,
};
pub use tree_content::{
    tree_diff_contents_parallel, tree_diff_structured_parallel, TreeDiffContent, TreeDiffStructured,
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
pub mod diffstat {
    //! diffstat API (re-exported at this path).
    pub use crate::diffstat::*;
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
    //! Whitespace helpers for diff and apply.
    pub use crate::ws::*;
}
