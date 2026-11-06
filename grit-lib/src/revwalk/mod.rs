//! Revision walking: rev-parse, rev-list, merge-base, and commit-graph acceleration.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/revwalk)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.revwalk();
//!
//! # Ok(())
//! # }
//! ```
pub(crate) mod describe;
pub mod filter;
pub(crate) mod graph;
pub mod group_by;
pub mod handle;
pub(crate) mod queries;

pub use crate::diff::{Pickaxe, PickaxeError, PickaxeKind};
pub use crate::rev_list::CommitExtent;
pub use describe::{DescribeOptions, DescribedCommit};
pub use filter::{CommitFilter, CommitFilterBuilder, CommitFilterError};
pub use graph::{
    display_parents_for_graph, list_commits_for_log_graph, log_graph_materialize,
    rewrite_display_parents, rewrite_display_parents_map, GraphCharset, GraphCommitInput,
    GraphLayout, GraphLineKind, GraphRenderedLine, GraphRowTopology,
};
pub use group_by::{
    group_commits_by_author, group_commits_by_author_from_rev, mailmap_for_grouping, AuthorGroup,
    GroupByAuthorOptions,
};
pub use handle::RevWalk;
pub use handle::RevWalkCommitIter;
pub use queries::{
    committer_cutoff_from_date, parse_rev_walk_tokens, resolve_log_walk_tips_and_hide,
    rev_list_commits_for_author_group, rev_walk_list_supports_stream, LeftRightCounts, MergeFilter,
    RefSelection, RevWalkCommitIterOptions, RevWalkCommitStream, RevWalkCommitStreamItem,
    RevWalkListOptions, RevWalkParsedSpecs,
};

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
pub mod rev_list_error {
    //! Typed `rev-list` failures surfaced on [`crate::revwalk::RevWalk`].
    pub use crate::rev_list_error::RevListError;
}
pub mod rev_parse_error {
    //! Typed revision parsing failures.
    pub use crate::rev_parse_error::{AmbiguousChangeIdHint, AmbiguousObjectHint, RevParseError};
}
pub mod object_inspect {
    //! Typed revision resolution and object display payloads.
    pub use crate::object_inspect_root::*;
    pub use crate::oid_abbrev::OidAbbrevBatch;
}
pub mod history {
    //! In-memory history rewrite planning and apply.
    pub use crate::history_root::*;
}
