//! Revision walking: rev-parse, rev-list, merge-base, and commit-graph acceleration.
pub mod handle;

pub use handle::RevWalk;

pub mod rev_list {
    //! Canonical re-export of [`crate::rev_list`].
    pub use crate::rev_list::*;
}
pub mod rev_parse {
    //! Canonical re-export of [`crate::rev_parse`].
    pub use crate::rev_parse::*;
}
pub mod merge_base {
    //! Canonical re-export of [`crate::merge_base`].
    pub use crate::merge_base::*;
}
pub mod commit_graph {
    //! Commit-graph read/write ([`crate::commit_graph_file`], [`crate::commit_graph_write`]).
    pub use crate::commit_graph_file::*;
    pub use crate::commit_graph_write::*;
}
