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
pub use crate::porcelain::stash::*;
pub use crate::porcelain::stash_error::{
    CorruptStashReason, StashError, StashPushValidationError, StashResult,
};
