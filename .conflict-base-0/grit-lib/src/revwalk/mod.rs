//! Revision walking: rev-parse, rev-list, merge-base, and commit-graph acceleration.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/revwalk)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.rev_walk();
//!
//! # Ok(())
//! # }
//! ```
pub mod filter;
pub mod handle;
pub mod queries;

pub use filter::{CommitFilter, CommitFilterBuilder, CommitFilterError, Pattern};
pub use handle::RevWalk;
pub use queries::{parse_rev_walk_tokens, RevWalkListOptions, RevWalkParsedSpecs};

pub mod rev_list {
    //! rev_list API (re-exported at this path).
    pub use crate::rev_list::*;
}
pub mod rev_parse {
    //! rev_parse API (re-exported at this path).
    pub use crate::rev_parse::*;
}
pub mod merge_base {
    //! merge_base API (re-exported at this path).
    pub use crate::merge_base::*;
}
pub mod log_walk {
    //! Log-order revision walking helpers re-exported at this path.
    pub use crate::log_walk::*;
}
pub mod commit_graph {
    //! Commit-graph read and write helpers.
    pub use crate::commit_graph_file::*;
    pub use crate::commit_graph_write::*;
}
pub mod rev_list_error {
    //! rev_list_error API (re-exported at this path).
    pub use crate::rev_list_error::*;
}
pub mod rev_parse_error {
    //! rev_parse_error API (re-exported at this path).
    pub use crate::rev_parse_error::*;
}
pub mod object_inspect {
    //! Typed revision resolution and object display payloads.
    pub use crate::object_inspect_root::*;
}
pub mod history {
    //! In-memory history rewrite planning and apply.
    pub use crate::history_root::*;
}
