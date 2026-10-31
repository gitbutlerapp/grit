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
//!     committer: "Rewriter <r@example.com> 1 +0000".parse()?,
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
mod last;
mod message;
mod r#move;
mod plan;
mod rebase;
mod rebase_session;
mod replay_merge_session;
mod reword;
mod split;
mod squash;
mod subject;
mod types;
mod undo;
mod undo_work;

pub use absorb::{
    absorb, default_absorb_base, format_hunk_range_header, staged_absorb_paths, AbsorbExcludedRef,
    AbsorbOptions, AbsorbPlan, HunkAssignment, HunkRange, UnassignedHunk, UnassignedReason,
    DEFAULT_ABSORB_STACK_LIMIT,
};
pub use apply::{
    abort_interrupted_rewrite_apply, apply_absorb_rewrite, apply_fixup_rewrite, apply_rewrite,
    ensure_rewrite_apply_ready, recover_interrupted_rewrite_apply, rewrite_apply_journal_active,
    verify_rewrite_plan, RewriteApplyRecovery,
};
pub use drop::drop;
pub use error::HistoryError;
pub use exec::{verify_rebase_with_exec, ExecReport, ExecRunRecord, ExecSpec};
pub use fixup::{
    apply_fixup_hunk_selection, fixup, fixup_selection_paths, staged_fixup_paths, FixupOptions,
};
pub use last::{read_history_last, HistoryLastRecord};
pub use plan::plan_rewrite;
pub use r#move::{move_commit, Placement};
pub use rebase::{
    plan_rebase, plan_rebase_check, plan_rebase_with_hook, rebase_onto, PreRebase, RebaseCheckPlan,
    RebaseOntoOptions, RebasePlan, RebaseRequest,
};
#[cfg(any(test, feature = "unstable-bench-helpers"))]
pub use rebase::{test_patch_id_compute_call_count, test_reset_patch_id_compute_call_count};
pub(crate) use rebase_session::persist_rebase_stopped_head_if_missing;
pub use rebase_session::{
    abort_rebase, continue_rebase, skip_rebase, start_rebase, start_rebase_with_exec, RebaseAbort,
    RebaseOutcome, RebaseSessionError, RebaseStop, StartRebaseOptions,
};
pub use reword::{normalize_rewrite_message, reword, reword_with, RewordOptions};
pub use split::split;
pub use squash::{squash, SquashMessage, SquashOptions};
pub use subject::{commit_message_subject_line, rewritten_row_subject};
pub use types::{
    AppliedRewrite, ApplyOptions, CommitEdit, EmptyPolicy, HeadEffect, HistoryEdits, NewCommit,
    Pathspec, PlannedRefUpdate, RefScope, RewriteOptions, RewritePlan, RewrittenCommit,
    SplitOptions, UndoOutcome,
};
pub use undo::undo_last_rewrite;

/// Read and parse commit `oid`; failures stay typed as [`HistoryError::Library`].
fn load_commit(
    repo: &crate::repo::Repository,
    oid: crate::objects::ObjectId,
) -> Result<crate::objects::CommitData, HistoryError> {
    Ok(repo.odb().read_commit(&oid)?)
}

/// Root tree of commit `commit`; failures stay typed as [`HistoryError::Library`].
fn commit_tree(
    repo: &crate::repo::Repository,
    commit: crate::objects::ObjectId,
) -> Result<crate::objects::ObjectId, HistoryError> {
    Ok(repo.odb().commit_tree(&commit)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::change_id::ChangeIdError;
    use crate::error::Error;
    use crate::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
    use crate::repo::{init_repository, Repository};
    use crate::RefStorageFormat;

    fn rewrite_opts() -> RewriteOptions {
        RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: "Committer <c@example.com> 3 +0000"
                .parse()
                .expect("signature"),
            preserve_committer: false,
            sign: false,
        }
    }

    /// A repository with `main` at a two-commit history; returns the tip and a blob.
    fn repo_with_history() -> (tempfile::TempDir, Repository, ObjectId, ObjectId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let tree = repo.odb().write(ObjectKind::Tree, &[]).expect("tree");
        let mut parent = Vec::new();
        let mut tip = tree;
        for message in ["first\n", "second\n"] {
            let bytes = serialize_commit(&CommitData {
                tree,
                parents: parent.clone(),
                author: "Author <a@example.com> 1 +0000".into(),
                committer: "Committer <c@example.com> 2 +0000".into(),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: message.into(),
                raw_message: None,
                extra_headers: vec![],
            });
            tip = repo
                .odb()
                .write(ObjectKind::Commit, &bytes)
                .expect("commit");
            parent = vec![tip];
        }
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");
        let blob = repo
            .odb()
            .write(ObjectKind::Blob, b"not a commit")
            .expect("blob");
        (dir, repo, tip, blob)
    }

    #[test]
    fn load_commit_and_commit_tree_read_commits() {
        let (_dir, repo, tip, _blob) = repo_with_history();
        let commit = load_commit(&repo, tip).expect("load");
        assert_eq!(commit.message, "second\n");
        assert_eq!(commit_tree(&repo, tip).expect("tree"), commit.tree);
    }

    #[test]
    fn load_commit_keeps_object_errors_typed() {
        let (_dir, repo, _tip, blob) = repo_with_history();
        let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("hex");
        let err = load_commit(&repo, missing).unwrap_err();
        assert!(
            matches!(&err, HistoryError::Library(inner) if matches!(**inner, Error::ObjectNotFound(_))),
            "{err:?}"
        );
        let err = commit_tree(&repo, blob).unwrap_err();
        assert!(
            matches!(
                &err,
                HistoryError::Library(inner) if matches!(
                    **inner,
                    Error::UnexpectedObjectKind { oid, expected: ObjectKind::Commit, actual: ObjectKind::Blob }
                        if oid == blob
                )
            ),
            "{err:?}"
        );
    }

    #[test]
    fn drop_of_a_non_commit_is_a_typed_library_error() {
        let (_dir, repo, _tip, blob) = repo_with_history();
        let err = drop(&repo, blob, &rewrite_opts()).unwrap_err();
        assert!(
            matches!(
                &err,
                HistoryError::Library(inner) if matches!(**inner, Error::UnexpectedObjectKind { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn squash_with_an_unknown_revision_is_a_typed_library_error() {
        let (_dir, repo, _tip, _blob) = repo_with_history();
        let opts = SquashOptions {
            message: SquashMessage::Oldest,
            rewrite: rewrite_opts(),
        };
        let err = squash(&repo, &["no-such-branch..main"], &opts).unwrap_err();
        assert!(matches!(err, HistoryError::Library(_)), "{err:?}");
    }

    #[test]
    fn change_id_errors_convert_to_their_own_variant() {
        let err = HistoryError::from(ChangeIdError::HashCollision);
        assert!(matches!(
            err,
            HistoryError::ChangeId(ChangeIdError::HashCollision)
        ));
        assert_eq!(err.to_string(), ChangeIdError::HashCollision.to_string());
    }
}
