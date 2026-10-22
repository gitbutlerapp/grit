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
//! Replacement commits preserve change-id headers and other extra fields from the source
//! (commit signatures are omitted). When change ids are enabled, headerless sources get a
//! stored derived id; newly inserted commits get fresh ids (see [`crate::change_id`]).
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
//!     preserve_committer: false,
//!     sign: true,
//! };
//! let _plan = plan_rewrite(repo, &edits, &opts)?;
//! # Ok(())
//! # }
//! ```

mod absorb;
mod apply;
mod drop;
mod error;
mod exec;
mod fixup;
mod r#move;
mod plan;
mod rebase;
mod reword;
mod split;
mod squash;
mod subject;
mod types;

pub use absorb::{
    absorb, default_absorb_base, staged_absorb_paths, AbsorbOptions, AbsorbPlan, HunkAssignment,
    HunkRange, UnassignedHunk, UnassignedReason, DEFAULT_ABSORB_STACK_LIMIT,
};
pub use apply::{apply_absorb_rewrite, apply_fixup_rewrite, apply_rewrite, verify_rewrite_plan};
pub use drop::drop;
pub use error::HistoryError;
pub use exec::{verify_rebase_with_exec, ExecReport, ExecRunRecord, ExecSpec};
pub use fixup::{fixup, staged_fixup_paths, FixupOptions};
pub use plan::plan_rewrite;
#[cfg(any(test, feature = "unstable-bench-helpers"))]
pub use plan::{test_merge_tree_call_count, test_reset_merge_tree_call_count};
pub use r#move::{move_commit, Placement};
pub use rebase::{plan_rebase, rebase_onto, RebaseOntoOptions, RebasePlan, RebaseRequest};
#[cfg(any(test, feature = "unstable-bench-helpers"))]
pub use rebase::{test_patch_id_compute_call_count, test_reset_patch_id_compute_call_count};
pub use reword::{normalize_rewrite_message, reword};
pub use split::split;
pub use squash::{squash, SquashMessage, SquashOptions};
pub use subject::{commit_message_subject_line, rewritten_row_subject};
pub use types::{
    AppliedRewrite, ApplyOptions, CommitEdit, EmptyPolicy, HeadEffect, HistoryEdits, NewCommit,
    Pathspec, PlannedRefUpdate, RefScope, RewriteOptions, RewritePlan, RewrittenCommit,
    SplitOptions,
};
