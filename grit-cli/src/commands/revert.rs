//! `grit revert` — undo one or more commits with new inverse commits.

use grit_lib::progress::{CancelToken, NullProgress};

use crate::commands::replay_merge_args::{mainline_parent, EmptyStepArg, ReplayMergeArgs};
use crate::context;
use crate::message_input::{read_message_input, MessageInputArgs};
use crate::render::operation::OperationConflictBlock;
pub use crate::render::revert::{RevertCommitEntry, RevertOutcome, RevertResult};
use anyhow::{bail, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::error::RefError;
use grit_lib::merge::{
    ReplayAbortOutcome, ReplayCreated, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
    SequencePrediction,
};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::ReplayDirection;
/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct RevertRunOptions {
    pub commits: Vec<String>,
    pub mainline: Option<u32>,
    pub continue_revert: bool,
    pub abort: bool,
    pub skip: bool,
    pub no_commit: bool,
    pub empty: EmptyStepArg,
    pub message: MessageInputArgs,
    pub dry_run: bool,
    pub check: bool,
    #[allow(clippy::struct_field_names)]
    pub replay_merge: ReplayMergeArgs,
}

impl From<&ReplayCreated> for RevertCommitEntry {
    fn from(record: &ReplayCreated) -> Self {
        Self {
            source: record.source.to_hex(),
            oid: record.new_oid.to_hex(),
            subject: context::subject_line(&record.subject),
            change_id: record.change_id.as_ref().map(|id| id.as_str().to_owned()),
        }
    }
}

/// Revert commits onto the current branch, or continue/skip/abort an in-progress sequence.
pub fn run(opts: &RevertRunOptions) -> Result<RevertOutcome> {
    let repo = context::discover()?;
    if opts.abort {
        match repo
            .merge()
            .abort_sequence()
            .map_err(|e| map_replay_error(&repo, e))?
        {
            ReplayAbortOutcome::Rewound => {}
            ReplayAbortOutcome::ClearedInPlace => {
                eprintln!(
                    "warning: branch tip moved during the revert; replay state cleared without moving the branch"
                );
            }
        }
        return Ok(RevertOutcome::new(RevertResult::Aborted));
    }

    let env = repo.environment();
    let now = context::wall_clock_now(env);
    let committer = context::signature(&repo, IdentRole::Committer, now)?;

    let _interrupt = crate::interrupt::InterruptCancel::install();
    let mut progress = grit_lib::progress::NullProgress;

    if opts.continue_revert {
        let outcome = repo
            .merge()
            .continue_sequence(
                &committer,
                &mut progress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(|e| map_replay_error(&repo, e))?;
        return Ok(map_sequence_outcome(outcome));
    }
    if opts.skip {
        let outcome = repo
            .merge()
            .skip_sequence_step(
                &committer,
                &mut progress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(|e| map_replay_error(&repo, e))?;
        return Ok(map_sequence_outcome(outcome));
    }

    if opts.commits.is_empty() {
        bail!("at least one commit is required unless using --continue, --abort, or --skip");
    }
    let sources = repo
        .merge()
        .commits_to_revert(&opts.commits)
        .map_err(crate::error_map::map_commit_spec_error)?;
    if opts.dry_run {
        return Ok(RevertOutcome::dry_run(sources));
    }

    let favor = opts.replay_merge.favor();
    let conflict_style = opts.replay_merge.conflict_style(&repo);
    let message_override = if opts.check {
        None
    } else {
        read_message_input(&opts.message)?
    };
    let req = ReplaySequenceRequest {
        commits: sources,
        direction: ReplayDirection::Revert,
        committer,
        options: ReplayOptions {
            no_commit: opts.no_commit,
            empty: opts.empty.into(),
            message_override,
            mainline: mainline_parent(opts.mainline)?,
            favor,
            conflict_style,
            ..ReplayOptions::default()
        },
    };
    if opts.check {
        let prediction = repo
            .merge()
            .predict_sequence(&req)
            .map_err(|e| map_replay_error(&repo, e))?;
        return Ok(map_prediction_outcome(prediction));
    }
    let outcome = repo
        .merge()
        .start_sequence(
            &req,
            &mut progress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(|e| map_replay_error(&repo, e))?;
    Ok(map_sequence_outcome(outcome))
}

fn map_prediction_outcome(prediction: SequencePrediction) -> RevertOutcome {
    let outcome = match prediction {
        SequencePrediction::Clean { tree, dropped } => RevertOutcome {
            oid: Some(tree.to_hex()),
            dropped: dropped.iter().map(ObjectId::to_hex).collect(),
            ..RevertOutcome::new(RevertResult::Clean)
        },
        SequencePrediction::Conflicts {
            commit, conflicts, ..
        } => RevertOutcome {
            source: Some(commit.to_hex()),
            conflicts: Some(OperationConflictBlock::for_revert(&conflicts).conflicts),
            conflict_sources: Some(conflicts),
            ..RevertOutcome::new(RevertResult::Conflicts)
        },
        SequencePrediction::Empty { commit, .. } => RevertOutcome {
            source: Some(commit.to_hex()),
            ..RevertOutcome::new(RevertResult::Empty)
        },
    };
    RevertOutcome {
        check: true,
        ..outcome
    }
}

fn map_sequence_outcome(outcome: SequenceOutcome) -> RevertOutcome {
    match outcome {
        SequenceOutcome::Completed {
            created,
            dropped,
            rerere,
        } => {
            let commits: Vec<RevertCommitEntry> =
                created.iter().map(RevertCommitEntry::from).collect();
            let last = commits.last().cloned();
            RevertOutcome {
                source: last.as_ref().map(|e| e.source.clone()),
                oid: last.as_ref().map(|e| e.oid.clone()),
                subject: last.as_ref().map(|e| e.subject.clone()),
                change_id: last.and_then(|e| e.change_id),
                commits,
                dropped: dropped.iter().map(ObjectId::to_hex).collect(),
                rerere: crate::render::operation::rerere_events_json(&rerere),
                ..RevertOutcome::new(RevertResult::Completed)
            }
        }
        SequenceOutcome::Conflicts {
            conflicts,
            created_so_far,
            rerere,
            ..
        } => {
            let block = OperationConflictBlock::for_revert(&conflicts);
            RevertOutcome {
                commits: created_so_far.iter().map(RevertCommitEntry::from).collect(),
                conflicts: Some(block.conflicts),
                conflict_sources: Some(conflicts),
                rerere: crate::render::operation::rerere_events_json(&rerere),
                next_steps: Some(block.next_steps),
                alternatives: block.alternatives,
                ..RevertOutcome::new(RevertResult::Conflicts)
            }
        }
        SequenceOutcome::Empty { created_so_far, .. } => {
            let block = OperationConflictBlock::for_revert(&[]);
            RevertOutcome {
                commits: created_so_far.iter().map(RevertCommitEntry::from).collect(),
                next_steps: Some(block.next_steps),
                alternatives: block.alternatives,
                ..RevertOutcome::new(RevertResult::Empty)
            }
        }
    }
}

fn map_replay_error(repo: &grit_lib::repo::Repository, err: Error) -> anyhow::Error {
    match err {
        Error::Merge(merge_err) => map_merge_replay_error(repo, merge_err),
        Error::Commit(grit_lib::porcelain::CommitError::NothingToCommit) => {
            anyhow::anyhow!("nothing to commit — the revert would not change the tree")
        }
        Error::Ref(RefError::UnbornHead) => {
            anyhow::anyhow!("no commits yet on this branch to revert onto")
        }
        other => anyhow::Error::new(other),
    }
}

fn map_merge_replay_error(
    repo: &grit_lib::repo::Repository,
    err: grit_lib::porcelain::MergeError,
) -> anyhow::Error {
    use grit_lib::porcelain::MergeError;
    match err {
        MergeError::OperationInProgress(kind) => {
            crate::json_error::operation_in_progress_for_repo(repo, "revert", kind)
        }
        MergeError::MergeCommit { oid } => {
            let s = oid.to_short_hex();
            anyhow::anyhow!(
                "{s} is a merge commit — pass `--mainline 1` (or another parent number) to revert it"
            )
        }
        MergeError::ReplaySourceAtHead => {
            anyhow::anyhow!("nothing to revert — that commit is already the current HEAD")
        }
        MergeError::ReplayContinueUnmerged => anyhow::anyhow!(
            "cannot continue revert: resolve conflicts and stage the result first (`grit add`)"
        ),
        MergeError::NoReplayInProgress => {
            crate::json_error::no_operation_in_progress("no revert in progress")
        }
        MergeError::ReplaySkipAlreadyCommitted => anyhow::anyhow!(
            "nothing to skip: current step already committed; run `grit revert --continue` instead"
        ),
        MergeError::ReplayContinueDirtyIndex => anyhow::anyhow!(
            "cannot continue revert: index differs from HEAD after the step was committed"
        ),
        other => anyhow::Error::new(other),
    }
}

/// Dispatch parsed CLI args for `Revert`.
pub fn dispatch(
    args: crate::args::RevertArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let RevertArgs {
        commits,
        mainline,
        continue_revert,
        abort,
        skip,
        no_commit,
        empty,
        message,
        check,
        dry_run,
        replay_merge,
    } = args;
    {
        let outcome = commands::revert::run(&commands::revert::RevertRunOptions {
            commits,
            mainline,
            continue_revert,
            abort,
            skip,
            no_commit,
            empty,
            message,
            check,
            dry_run,
            replay_merge,
        })?;
        let result = outcome.result;
        emit_success(outcome, opts, ctx)?;
        Ok(ExitStatus::from_revert_result(result))
    }
}
