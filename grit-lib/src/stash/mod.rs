//! Stash stack read/write — stable types for [`RepositoryStash`](crate::repo_handles::RepositoryStash).
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/stash)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.stash();
//!
//! # Ok(())
//! # }
//! ```
pub use crate::porcelain::stash::FlatTreeEntry;
pub use crate::porcelain::stash::{
    check_stash_apply_would_overwrite_local_changes, flatten_tree_full, parse_stash_reference,
    stash_index_for_oid, worktree_bytes_for_index_mode, StashApplyOptions, StashApplyOutcome,
    StashBranchOutcome, StashDiffOptions, StashEntry, StashMode, StashPreview, StashPushOptions,
    StashPushResult, StashReference,
};
pub use crate::porcelain::stash_error::{CorruptStashReason, StashError, StashPushValidationError};
