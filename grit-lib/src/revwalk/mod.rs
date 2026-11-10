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
mod log;
mod merge_bases;
pub(crate) mod queries;
mod resolve;

pub use crate::diff::{Pickaxe, PickaxeError, PickaxeKind};
pub use crate::rev_list::CommitExtent;
pub use describe::{DescribeOptions, DescribedCommit};
pub use filter::{CommitFilter, CommitFilterBuilder, CommitFilterError};
pub use graph::{
    GraphCharset, GraphCommitInput, GraphLayout, GraphLineKind, GraphRenderedLine,
    GraphRowTopology, LogGraphMaterialized,
};
pub use group_by::{AuthorGroup, GroupByAuthorOptions};
pub use handle::RevWalk;
pub use handle::{RevWalkCommitIter, StdinRevisionSpecs};
pub use merge_bases::{AheadBehind, AheadBehindOptions, MergeBaseMode, ReachableWalkOptions};
pub(crate) use queries::parse_rev_walk_tokens;
pub use queries::{
    committer_cutoff_from_date, rev_walk_list_supports_stream, LeftRightCounts, MergeFilter,
    RefSelection, RevWalkCommitIterOptions, RevWalkCommitStream, RevWalkCommitStreamItem,
    RevWalkListOptions, RevWalkParsedSpecs,
};
pub use resolve::{AbbrevLength, IndexDwim, Peel, ResolveOptions, ResolvePurpose};

pub mod rev_list {
    //! rev_list API (re-exported at this path).
    pub use crate::rev_list::{
        expand_object_filter_for_protocol, split_revision_token, CherryMatch, CommitExtent,
        FilterObjectKind, LogGraphLinkage, MissingAction, ObjectFilter, ObjectFilterError,
        OrderingMode, RevListOptions, RevListResult,
    };
    pub(crate) use crate::rev_list::{rev_list, topo_order_commit_subset};
}
pub mod rev_parse {
    //! rev_parse API (re-exported at this path).
    pub use crate::rev_parse::IndexColonSpec;
    pub use crate::rev_parse::{
        abbreviate_ref_name, parse_index_colon_spec, parse_peel_suffix,
        reflog_date_selector_timestamp, reflog_date_selector_timestamp_at,
        revision_spec_contains_ancestry_navigation, spec_has_parent_shorthand_suffix,
        split_double_dot_range, split_treeish_colon, split_triple_dot_range, to_relative_path,
        upstream_suffix_info, TreeishBlobAtPath,
    };
    pub(crate) use crate::rev_parse::{
        peel_to_commit_for_merge_base, resolve_revision, resolve_revision_for_range_end,
    };
}
pub mod merge_base {
    //! merge_base API (re-exported at this path).
    #[cfg(test)]
    pub(crate) use crate::merge_base::intersect_walk_test_hooks;
    pub(crate) use crate::merge_base::merge_base_for_diff_two_commits;
    pub use crate::merge_base::ReachableCountFilter;
    pub use crate::merge_base::ReachableWalkCommitterCutoffs;
    pub use crate::merge_base::{
        is_ancestor_odb_walk, rebase_fork_point_enabled, ReachableWalkLimit,
    };
}
pub mod log_walk {
    //! Log-order revision walking helpers re-exported at this path.
    pub use crate::log_walk::{LogWalkCursor, LogWalkPage, LogWalkQuery, WalkedCommits};
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
    pub use crate::object_inspect_root::{
        format_tree_cat_file_p, format_tree_entries_cat_file_p, tree_entry_cat_file_type,
        tree_entry_mode_cat_file_p, ObjectContents, ResolveError, ResolvedObject, ResolvedRefName,
        RevisionLabel,
    };
    pub use crate::oid_abbrev::OidAbbrevBatch;
}
pub mod history {
    //! In-memory history rewrite planning and apply.

    pub use crate::history_root::AbsorbExcludedRef;
    pub use crate::history_root::HistoryLastRecord;
    pub use crate::history_root::{
        format_hunk_range_header, normalize_rewrite_message, AbsorbOptions, AbsorbPlan,
        AppliedRewrite, ApplyOptions, CommitEdit, EmptyPolicy, ExecReport, ExecRunRecord, ExecSpec,
        FixupOptions, HeadEffect, History, HistoryEdits, HistoryError, HunkAssignment, HunkRange,
        NewCommit, Pathspec, Placement, PlannedRefUpdate, PlanningError, PreRebase, RebaseAbort,
        RebaseCheckPlan, RebaseOntoOptions, RebaseOutcome, RebasePlan, RebaseRequest,
        RebaseSessionError, RebaseStop, RefScope, RewordOptions, RewriteApplyRecovery,
        RewriteOptions, RewritePlan, RewrittenCommit, SplitOptions, SquashMessage, SquashOptions,
        StartRebaseOptions, UnassignedHunk, UnassignedReason, UndoMode, UndoOutcome,
    };
    pub(crate) use crate::history_root::{
        plan_rebase_check, rebase_request_after_fetch, start_rebase,
    };
}
