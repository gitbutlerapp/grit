//! Git index (staging area) read/write and related cache extensions.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/staging)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.index_handle().load()?;
//!
//! # Ok(())
//! # }
//! ```
mod core;
pub mod handle;
mod mode_policy;

pub use core::*;
pub use handle::IndexHandle;
pub use mode_policy::WorktreeModePolicy;

pub mod sparse_checkout {
    //! sparse_checkout API (re-exported at this path).
    pub use crate::sparse_checkout::*;
}
pub mod split_index {
    //! split_index API (re-exported at this path).
    pub use crate::split_index::*;
}
pub mod untracked_cache {
    //! untracked_cache API (re-exported at this path).
    pub use crate::untracked_cache::*;
}
pub mod resolve_undo {
    //! resolve_undo API (re-exported at this path).
    pub use crate::resolve_undo::*;
}
pub mod index_name_hash_lazy {
    //! index_name_hash_lazy API (re-exported at this path).
    pub use crate::index_name_hash_lazy::*;
}
pub mod write_tree {
    //! write_tree API (re-exported at this path).
    pub use crate::write_tree::*;
}
