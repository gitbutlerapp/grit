//! Working tree, ignore/attributes, and checkout path helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/worktree)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _wt = _repo.work_tree();
//!
//! # Ok(())
//! # }
//! ```
mod core;
mod error;
pub mod io_error;
mod move_repair;

pub use core::*;
pub use error::{WorktreeError, WorktreeOperation};
pub use io_error::{
    format_worktree_path_io_message, io_with_path, is_file_in_use, map_worktree_io,
    WorktreePathOperation,
};
pub use move_repair::{
    move_linked_worktree, repair_worktrees, resolve_move_destination, MoveWorktreeOptions,
    WorktreeRepair, WorktreeRepairKind,
};

pub mod attributes {
    //! attributes API (re-exported at this path).
    pub use crate::attributes::*;
}
pub mod crlf {
    //! crlf API (re-exported at this path).
    pub use crate::crlf::*;
}
pub mod ignore {
    //! ignore API (re-exported at this path).
    pub use crate::ignore::*;
}
pub mod fsmonitor {
    //! fsmonitor API (re-exported at this path).
    pub use crate::fsmonitor_root::*;
}
pub mod worktree_cwd {
    //! worktree_cwd API (re-exported at this path).
    pub use crate::worktree_cwd::*;
}
pub mod worktree_ref {
    //! worktree_ref API (re-exported at this path).
    pub use crate::worktree_ref::*;
}
pub mod worktree_rules {
    //! worktree_rules API (re-exported at this path).
    pub use crate::worktree_rules::*;
}
pub mod pathspec {
    //! pathspec API (re-exported at this path).
    pub use crate::pathspec::*;
}
pub mod wildmatch {
    //! wildmatch API (re-exported at this path).
    pub use crate::wildmatch::*;
}
