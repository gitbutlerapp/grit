//! `grit pick` — cherry-pick one or more commits onto the current branch.

use crate::commands::replay_merge_args::{mainline_parent, EmptyStepArg, ReplayMergeArgs};
use crate::context;
use crate::message_input::{read_message_input, MessageInputArgs};
use crate::render::operation::OperationConflictBlock;
pub use crate::render::pick::{PickCommitRecord, PickOutcome, PickResult};
use anyhow::{bail, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::error::RefError;
use grit_lib::merge::{
    ReplayAbortOutcome, ReplayCreated, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
    SequencePrediction,
};
use grit_lib::objects::ObjectId;
use grit_lib::operation::next_step_grit_commands;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::state::InProgressOperation;
use grit_lib::state::{operation_alternatives, operation_next_steps_empty};

/// Options for [`run`].
pub struct PickRunOptions {
    pub commits: Vec<String>,
    pub continue_pick: bool,
    pub abort: bool,
    pub skip: bool,
    pub record_origin: bool,
    pub no_commit: bool,
    pub empty: EmptyStepArg,
    pub message: MessageInputArgs,
    pub mainline: Option<u32>,
    pub dry_run: bool,
    pub check: bool,
    pub replay_merge: ReplayMergeArgs,
}

/// Cherry-pick commits onto the current branch, or resume/cancel an in-progress pick.
pub fn run(opts: PickRunOptions) -> Result<PickOutcome> {
    let repo = context::discover()?;
    let env = repo.environment();
    let now = context::wall_clock_now(env);
    let committer = context::signature(&repo, IdentRole::Committer, now)?;

    if (opts.continue_pick || opts.abort || opts.skip) && !opts.commits.is_empty() {
        bail!("`--continue`, `--abort`, and `--skip` cannot be used with commit arguments");
    }

    if opts.abort {
        match repo
            .merge()
            .abort_sequence()
            .map_err(|e| map_replay_error(&repo, e))?
        {
            ReplayAbortOutcome::Rewound => {}
            ReplayAbortOutcome::ClearedInPlace => {
                eprintln!(
                    "warning: branch tip moved during the pick; replay state cleared without moving the branch"
                );
            }
            _ => {}
        }
        return Ok(PickOutcome::new(PickResult::Aborted));
    }

    let _interrupt = crate::interrupt::InterruptCancel::install();
    let mut progress = grit_lib::progress::NullProgress;

    if opts.continue_pick {
        let outcome = repo
            .merge()
            .continue_sequence(
                &committer,
                &mut progress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(|e| map_replay_error(&repo, e))?;
        return map_sequence_outcome(outcome, PickResult::Picked);
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
        return map_sequence_outcome(outcome, PickResult::Skipped);
    }

    if opts.commits.is_empty() {
        bail!("at least one commit or `--continue` / `--abort` / `--skip` is required");
    }

    let sources = repo
        .merge()
        .commits_to_pick(&opts.commits)
        .map_err(crate::error_map::map_commit_spec_error)?;
    if opts.dry_run {
        return Ok(PickOutcome {
            commits: sources
                .iter()
                .map(|oid| PickCommitRecord {
                    source: oid.to_hex(),
                    oid: None,
                    change_id: None,
                    subject: String::new(),
                })
                .collect(),
            dry_run: Some(true),
            ..PickOutcome::new(PickResult::Picked)
        });
    }
    let favor = opts.replay_merge.favor();
    let conflict_style = opts.replay_merge.conflict_style(&repo);
    let message_override = if opts.check {
        None
    } else {
        read_message_input(&opts.message)?
    };
    if message_override.is_some() && opts.commits.len() != 1 {
        bail!("--message and --message-file are only allowed when picking exactly one commit");
    }
    let req = ReplaySequenceRequest {
        commits: sources,
        direction: ReplayDirection::Pick,
        committer,
        options: ReplayOptions {
            record_origin: opts.record_origin,
            no_commit: opts.no_commit,
            empty: opts.empty.into(),
            message_override,
            mainline: mainline_parent(opts.mainline)?,
            favor,
            conflict_style,
        },
    };
    if opts.check {
        let prediction = repo
            .merge()
            .predict_sequence(&req)
            .map_err(|e| map_replay_error(&repo, e))?;
        return map_prediction_outcome(prediction);
    }
    let outcome = repo
        .merge()
        .start_sequence(
            &req,
            &mut progress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(|e| map_replay_error(&repo, e))?;
    map_sequence_outcome(outcome, PickResult::Picked)
}

impl From<&ReplayCreated> for PickCommitRecord {
    fn from(record: &ReplayCreated) -> Self {
        Self {
            source: record.source.to_hex(),
            oid: Some(record.new_oid.to_hex()),
            change_id: record.change_id.as_ref().map(|id| id.as_str().to_owned()),
            subject: context::subject_line(&record.subject),
        }
    }
}

fn source_record(source: ObjectId, subject: &str) -> PickCommitRecord {
    PickCommitRecord {
        source: source.to_hex(),
        oid: None,
        change_id: None,
        subject: context::subject_line(subject),
    }
}

fn map_sequence_outcome(outcome: SequenceOutcome, ok_result: PickResult) -> Result<PickOutcome> {
    Ok(match outcome {
        SequenceOutcome::Completed {
            created,
            dropped,
            rerere,
            ..
        } => PickOutcome {
            commits: created.iter().map(PickCommitRecord::from).collect(),
            dropped: dropped.iter().map(ObjectId::to_hex).collect(),
            rerere: crate::render::operation::rerere_events_json(&rerere),
            ..PickOutcome::new(ok_result)
        },
        SequenceOutcome::Conflicts {
            conflicts,
            created_so_far,
            rerere,
            ..
        } => {
            let block = OperationConflictBlock::for_pick(&conflicts);
            PickOutcome {
                commits: created_so_far.iter().map(PickCommitRecord::from).collect(),
                conflicts: Some(block.conflicts),
                conflict_sources: Some(conflicts),
                rerere: crate::render::operation::rerere_events_json(&rerere),
                next_steps: Some(block.next_steps),
                alternatives: block.alternatives,
                ..PickOutcome::new(PickResult::Conflicts)
            }
        }
        SequenceOutcome::Empty {
            current,
            current_subject,
            created_so_far,
            ..
        } => {
            let mut commits: Vec<PickCommitRecord> =
                created_so_far.iter().map(PickCommitRecord::from).collect();
            commits.push(source_record(current, &current_subject));
            let next_steps = next_step_grit_commands(&operation_next_steps_empty(
                InProgressOperation::CherryPick,
            ));
            let alternatives =
                next_step_grit_commands(&operation_alternatives(InProgressOperation::CherryPick));
            PickOutcome {
                commits,
                next_steps: Some(next_steps),
                alternatives: (!alternatives.is_empty()).then_some(alternatives),
                ..PickOutcome::new(PickResult::Empty)
            }
        }
        _ => bail!("pick finished with an outcome this version of grit cannot display"),
    })
}

fn map_prediction_outcome(prediction: SequencePrediction) -> Result<PickOutcome> {
    let outcome = match prediction {
        SequencePrediction::Clean { tree, dropped, .. } => PickOutcome {
            dropped: dropped.iter().map(ObjectId::to_hex).collect(),
            oid: Some(tree.to_hex()),
            ..PickOutcome::new(PickResult::Clean)
        },
        SequencePrediction::Conflicts {
            commit,
            subject,
            conflicts,
            ..
        } => PickOutcome {
            commits: vec![source_record(commit, &subject)],
            conflicts: Some(OperationConflictBlock::for_pick(&conflicts).conflicts),
            conflict_sources: Some(conflicts),
            ..PickOutcome::new(PickResult::Conflicts)
        },
        SequencePrediction::Empty {
            commit, subject, ..
        } => PickOutcome {
            commits: vec![source_record(commit, &subject)],
            ..PickOutcome::new(PickResult::Empty)
        },
        _ => bail!("pick --check returned a prediction this version of grit cannot display"),
    };
    Ok(PickOutcome {
        check: true,
        ..outcome
    })
}

fn map_replay_error(repo: &grit_lib::repo::Repository, err: Error) -> anyhow::Error {
    match err {
        Error::Merge(merge_err) => map_merge_replay_error(repo, "pick", merge_err),
        Error::Ref(RefError::UnbornHead) => {
            anyhow::anyhow!("no commits yet on this branch to pick onto")
        }
        other => anyhow::Error::new(other),
    }
}

fn map_merge_replay_error(
    repo: &grit_lib::repo::Repository,
    verb: &str,
    err: grit_lib::porcelain::MergeError,
) -> anyhow::Error {
    use grit_lib::porcelain::MergeError;
    match err {
        MergeError::OperationInProgress(kind) => {
            crate::json_error::operation_in_progress_for_repo(repo, verb, kind)
        }
        MergeError::MergeCommit { oid } => {
            let s = oid.to_short_hex();
            anyhow::anyhow!(
                "{s} is a merge commit — pass `--mainline <n>` to pick a merge commit (not yet supported on the CLI)"
            )
        }
        MergeError::ReplaySourceAtHead => {
            anyhow::anyhow!("nothing to pick — that commit is already the current HEAD")
        }
        MergeError::NoReplayInProgress => {
            crate::json_error::no_operation_in_progress("no cherry-pick in progress")
        }
        MergeError::ReplayContinueUnmerged => anyhow::anyhow!(
            "cannot continue: resolve conflicts and stage the result (`grit add`), then run `grit pick --continue`"
        ),
        MergeError::ReplaySkipAlreadyCommitted => anyhow::anyhow!(
            "nothing to skip: current step already committed; run `grit pick --continue` instead"
        ),
        MergeError::ReplayContinueDirtyIndex => anyhow::anyhow!(
            "cannot continue: index differs from HEAD after the step was committed"
        ),
        other => anyhow::Error::new(other),
    }
}

/// Dispatch parsed CLI args for `Pick`.
pub fn dispatch(
    args: crate::args::PickArgs,
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
    let PickArgs {
        commits,
        mainline,
        continue_pick,
        abort,
        skip,
        record_origin,
        no_commit,
        empty,
        message,
        check,
        dry_run,
        replay_merge,
    } = args;
    {
        let outcome = commands::pick::run(commands::pick::PickRunOptions {
            commits,
            continue_pick,
            abort,
            skip,
            record_origin,
            no_commit,
            empty,
            message,
            mainline,
            check,
            dry_run,
            replay_merge,
        })?;
        let result = outcome.result;
        emit_success(outcome, opts, ctx)?;
        Ok(ExitStatus::from_pick_result(result))
    }
}
