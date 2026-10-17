//! In-memory history rewrite engine shared by `grit history` subcommands.
//!
//! This module plans how to rewrite commits: it walks the affected range, applies
//! [`CommitEdit`] instructions, replays descendants with three-way tree merges, writes
//! replacement commit objects into the object database, and returns a [`RewritePlan`]
//! describing new OIDs and ref movements. It is intentionally **not** a stateful sequencer:
//! conflicts abort planning, and **refs, the index, and the worktree are never modified**
//! here (even when the caller will dry-run apply later, new commit objects are still
//! persisted).
//!
//! Library guide: history rewrite planning (see [`plan_rewrite`]).
//!
//! ```rust
//! use grit_lib::revwalk::history::{
//!     plan_rewrite, CommitEdit, HistoryEdits, RefScope, RewriteOptions, EmptyPolicy,
//! };
//! use grit_lib::objects::ObjectId;
//! use grit_lib::repo::Repository;
//!
//! # fn example(repo: &Repository, oldest: ObjectId) -> grit_lib::error::Result<()> {
//! let mut edits = HistoryEdits::new();
//! edits.insert(
//!     oldest,
//!     CommitEdit::Rewrite {
//!         tree: None,
//!         message: Some("Reworded subject\n".into()),
//!         author: None,
//!         parents: None,
//!     },
//! );
//! let opts = RewriteOptions {
//!     scope: RefScope::Branches,
//!     empty: EmptyPolicy::Drop,
//!     committer: "Rewriter <r@example.com> 1 +0000".into(),
//! };
//! let _plan = plan_rewrite(repo, &edits, &opts)?;
//! # Ok(())
//! # }
//! ```

mod apply;
mod error;
mod plan;
mod reword;
mod types;

pub use apply::apply_rewrite;
pub use error::HistoryError;
pub use plan::plan_rewrite;
#[cfg(any(test, feature = "unstable-bench-helpers"))]
pub use plan::{test_merge_tree_call_count, test_reset_merge_tree_call_count};
pub use reword::{normalize_rewrite_message, reword};
pub use types::{
    AppliedRewrite, ApplyOptions, CommitEdit, EmptyPolicy, HeadEffect, HistoryEdits, NewCommit,
    PlannedRefUpdate, RefScope, RewriteOptions, RewritePlan, RewrittenCommit,
};
