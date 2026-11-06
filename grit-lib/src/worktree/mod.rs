//! Working tree, ignore/attributes, and checkout path helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/worktree)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _wt = _repo.work_tree();
//!
//! # Ok(())
//! # }
//! ```
mod core;
mod error;
pub(crate) mod io_error;
pub(crate) mod linked_worktree_error;
mod move_repair;
pub(crate) mod symlink;

pub use core::*;
pub use error::{LinkedWorktreeError, LinkedWorktreeOperation, WorktreeError};
pub(crate) use io_error::{io_with_path, map_worktree_io, WorktreePathOperation};
pub use move_repair::{
    move_linked_worktree, repair_worktrees, resolve_move_destination, MoveWorktreeOptions,
    WorktreeRepair, WorktreeRepairKind,
};
pub(crate) use symlink::{
    create_worktree_symlink, create_worktree_symlink_from_blob, symlink_target_path_from_bytes,
    write_index_symlink_to_worktree,
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
    pub use crate::error::{PathspecFromFileParseError, PathspecResolveError};
    pub use crate::pathspec::{
        bloom_lookup_prefix, bloom_lookup_prefix_with_cwd, has_glob_chars,
        is_long_magic_pathspec_token, parse_pathspecs_from_source, path_allowed_by_pathspec_list,
        pathdiff, pathspec_contributes_match, pathspec_has_top, pathspec_is_exclude,
        pathspec_wants_descent_into_tree, pathspecs_allow_bloom, precompose_argv_pathspec,
        resolve_pathspec_with_argv_precompose, simple_length, validate_attr_pathspecs,
        validate_global_pathspec_flags, Pathspec, PathspecFile, PathspecGlobalFlags,
        PathspecMatchContext, PathspecOptions, PathspecParseError,
    };
}
pub mod wildmatch {
    //! wildmatch API (re-exported at this path).
    pub use crate::wildmatch::*;
}
