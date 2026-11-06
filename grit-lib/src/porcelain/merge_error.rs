//! Typed failures for merge, cherry-pick, revert, and sequencer state.

use thiserror::Error;

use crate::error::Error;
use crate::objects::ObjectId;
use crate::state::InProgressOperation;

/// Merge, replay, and in-progress operation failures.
///
/// Wrapped by [`Error::Merge`](crate::error::Error::Merge). Stable [`kind_name`](Self::kind_name)
/// slugs match the former umbrella variant names for scripts and the CLI error map.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MergeError {
    /// A merge was requested with fast-forward only, but the histories diverged.
    #[error("not possible to fast-forward; refusing to merge")]
    NotFastForward,

    /// Merge histories share no common ancestor.
    #[error("refusing to merge unrelated histories")]
    UnrelatedHistories {
        /// Our side commit.
        ours: ObjectId,
        /// Their side commit.
        theirs: ObjectId,
    },

    /// Pick/revert does not replay merge commits without an explicit mainline.
    #[error("merge commit {oid}")]
    MergeCommit {
        /// Object id of the merge commit that cannot be replayed yet.
        oid: ObjectId,
    },

    /// Cherry-pick source is already checked out as `HEAD`.
    #[error("commit already at HEAD")]
    ReplaySourceAtHead,

    /// Sequencer todo lists a command grit does not implement yet.
    #[error("unknown sequencer command '{command}'")]
    SequencerUnknownCommand {
        /// Verb read from `.git/sequencer/todo`.
        command: String,
    },

    /// Continue replay refuses while unmerged index entries remain.
    #[error("cannot continue replay: unmerged index entries remain")]
    ReplayContinueUnmerged,

    /// Continue replay with a dirty index after the step was already committed.
    #[error("cannot continue replay: index differs from HEAD after the step was committed")]
    ReplayContinueDirtyIndex,

    /// Skip replay when the current step was already committed on a moved branch tip.
    #[error("nothing to skip: current step already committed; run continue instead")]
    ReplaySkipAlreadyCommitted,

    /// No cherry-pick or revert sequence is in progress.
    #[error("no cherry-pick or revert in progress")]
    NoReplayInProgress,

    /// No merge is in progress (abort/continue/skip).
    #[error("there is no merge in progress")]
    NoMergeInProgress,

    /// Another paused operation blocks the requested command.
    #[error("{} in progress", .0.description())]
    OperationInProgress(InProgressOperation),

    /// Stage-1/2/3 entries remain when finishing a merge.
    #[error("you still have unmerged paths: {}", paths.join(", "))]
    ContinueUnmergedPaths {
        /// Repository-relative paths still in conflict stages.
        paths: Vec<String>,
    },

    /// Work tree resolved but index still has conflict stages.
    #[error("resolved in the work tree but not staged: {}", paths.join(", "))]
    ContinueResolvedNotStaged {
        /// Repository-relative paths cleared of conflict markers but not staged.
        paths: Vec<String>,
    },

    /// Continue merge message would be empty after cleanup.
    #[error("empty commit message")]
    ContinueEmptyMessage,

    /// A pick/revert sequence is already in progress.
    #[error("cherry-pick or revert is already in progress")]
    ReplayAlreadyInProgress,

    /// `message_override` was set while replaying more than one commit.
    #[error("message_override is only valid when replaying a single commit")]
    MessageOverrideNotForMultiCommit,

    /// Merge or replay state on disk is incomplete for the requested operation.
    #[error("merge state incomplete: missing {missing}")]
    MergeStateIncomplete {
        /// Recorded merge artifact that was expected but absent.
        missing: &'static str,
    },

    /// A `.git/sequencer/created` journal line could not be parsed.
    #[error("invalid sequencer created record: {detail}")]
    SequencerCreatedParse {
        /// What was wrong with the line.
        detail: &'static str,
    },

    /// Underlying repository, commit, or I/O failure during merge/replay.
    #[error(transparent)]
    Library(Box<Error>),
}

impl MergeError {
    /// Whether a [`crate::error::Error::Cancelled`] is carried in [`Self::Library`].
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Library(e) if e.is_cancelled())
    }

    /// Stable snake_case slug for this failure (matches former [`Error::kind_name`] values).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NotFastForward => "not_fast_forward",
            Self::UnrelatedHistories { .. } => "unrelated_histories",
            Self::MergeCommit { .. } => "merge_commit",
            Self::ReplaySourceAtHead => "replay_source_at_head",
            Self::SequencerUnknownCommand { .. } => "sequencer_unknown_command",
            Self::ReplayContinueUnmerged => "replay_continue_unmerged",
            Self::ReplayContinueDirtyIndex => "replay_continue_dirty_index",
            Self::ReplaySkipAlreadyCommitted => "replay_skip_already_committed",
            Self::NoReplayInProgress => "no_replay_in_progress",
            Self::NoMergeInProgress => "no_merge_in_progress",
            Self::OperationInProgress(..) => "operation_in_progress",
            Self::ContinueUnmergedPaths { .. } => "index_unmerged",
            Self::ContinueResolvedNotStaged { .. } => "index_unmerged",
            Self::ContinueEmptyMessage => "empty_commit_message",
            Self::ReplayAlreadyInProgress => "operation_in_progress",
            Self::MessageOverrideNotForMultiCommit => "message",
            Self::MergeStateIncomplete { .. } => "message",
            Self::SequencerCreatedParse { .. } => "message",
            Self::Library(e) => e.kind_name(),
        }
    }
}

impl From<Error> for MergeError {
    fn from(err: Error) -> Self {
        match err {
            Error::Merge(inner) => inner,
            other => Self::Library(Box::new(other)),
        }
    }
}
