//! `grit pick` — cherry-pick one or more commits onto the current branch.

use crate::commands::replay_merge_args::{mainline_parent, EmptyStepArg, ReplayMergeArgs};
use crate::context;
use crate::markdown;
use crate::message_input::{read_message_input, MessageInputArgs};
use crate::operation_output::{
    render_conflicts_human, render_pick_check_conflicts_human, render_pick_conflict_footer_human,
    ConflictJson, OperationConflictBlock,
};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::revision_errors::map_commit_spec_error;
use anyhow::{bail, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{
    next_step_grit_commands, operation_alternatives, operation_next_steps_empty,
};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    abort_replay_sequence, commits_to_replay, continue_replay_sequence, predict_replay_sequence,
    skip_replay_step, start_replay_sequence, ReplayAbortOutcome, ReplayCreated, ReplayOptions,
    ReplaySequenceRequest, SequenceOutcome, SequencePrediction,
};
use grit_lib::state::InProgressOperation;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// Integration result for `grit pick` (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PickResult {
    Picked,
    /// Every step would apply cleanly (`grit pick --check` only).
    Clean,
    Conflicts,
    Empty,
    Aborted,
    Skipped,
}

/// One commit recorded in pick `--json` output.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PickCommitRecord {
    /// Source commit that was replayed (full hex oid).
    pub source: String,
    /// New commit on the current branch, or JSON `null` when `--no-commit` or stopped early.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// Stored `change-id` on `oid` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Subject line from the source commit (or the new commit when available).
    pub subject: String,
}

/// Result of `grit pick`.
#[derive(Serialize, JsonSchema)]
pub struct PickOutcome {
    pub result: PickResult,
    /// Commits picked or attempted in this invocation.
    pub commits: Vec<PickCommitRecord>,
    /// Source commits skipped as empty/redundant (`--empty drop` or already applied).
    pub dropped: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Predicted result tree (full hex) when `result` is `clean` from `--check`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// True when this outcome is from `grit pick --check` (nothing was picked).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
}

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

impl HumanRender for PickOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.result {
            PickResult::Picked | PickResult::Skipped => {
                if self.dry_run == Some(true) {
                    outln!(w, "Would pick {} commit(s)", self.commits.len());
                    for row in &self.commits {
                        let src = row.source.get(..7).unwrap_or(&row.source);
                        outln!(w, "  {src}");
                    }
                    return Ok(());
                }
                for row in &self.commits {
                    if let Some(oid) = &row.oid {
                        let src = row.source.get(..7).unwrap_or(&row.source);
                        let new = oid.get(..7).unwrap_or(oid.as_str());
                        outln!(w, "Picked {src} → {new} {}", row.subject);
                    } else {
                        let src = row.source.get(..7).unwrap_or(&row.source);
                        outln!(w, "Applied {src} {} (no commit)", row.subject);
                    }
                }
                if !self.dropped.is_empty() {
                    outln!(w, "Dropped {} empty commit(s)", self.dropped.len());
                }
            }
            PickResult::Clean => {
                if let Some(oid) = &self.oid {
                    outln!(w, "Would pick cleanly (tree {oid})");
                } else {
                    outln!(w, "Would pick cleanly");
                }
            }
            PickResult::Conflicts => {
                if self.check {
                    if let Some(conflicts) = &self.conflicts {
                        render_pick_check_conflicts_human(conflicts, w)?;
                    }
                } else if let Some(conflicts) = &self.conflicts {
                    render_conflicts_human(conflicts, w)?;
                    render_pick_conflict_footer_human();
                }
                if !self.check {
                    if let Some(steps) = &self.next_steps {
                        eprintln!();
                        for step in steps {
                            eprintln!("→ {step}");
                        }
                    }
                    if let Some(alt) = &self.alternatives {
                        let joined = alt.join(" or ");
                        eprintln!("→ or {joined}");
                    }
                }
            }
            PickResult::Empty => {
                if self.check {
                    if let Some(row) = self.commits.last() {
                        let src = row.source.get(..7).unwrap_or(&row.source);
                        outln!(w, "Would stop on an empty pick step ({src})");
                    } else {
                        outln!(w, "Would stop on an empty pick step");
                    }
                } else if let Some(row) = self.commits.last() {
                    let src = row.source.get(..7).unwrap_or(&row.source);
                    outln!(w, "Stopped on empty commit {src} — {}", row.subject);
                }
                if let Some(steps) = &self.next_steps {
                    for step in steps {
                        eprintln!("→ {step}");
                    }
                }
            }
            PickResult::Aborted => outln!(w, "Pick aborted."),
        }
        Ok(())
    }
}

impl MarkdownRender for PickOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_pick_outcome(w, self)?;
        Ok(())
    }
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
        match abort_replay_sequence(&repo).map_err(map_replay_error)? {
            ReplayAbortOutcome::Rewound => {}
            ReplayAbortOutcome::ClearedInPlace => {
                eprintln!(
                    "warning: branch tip moved during the pick; replay state cleared without moving the branch"
                );
            }
        }
        return Ok(PickOutcome::new(PickResult::Aborted));
    }

    if opts.continue_pick {
        let outcome = continue_replay_sequence(&repo, &committer).map_err(map_replay_error)?;
        return Ok(map_sequence_outcome(outcome, PickResult::Picked));
    }

    if opts.skip {
        let outcome = skip_replay_step(&repo, &committer).map_err(map_replay_error)?;
        return Ok(map_sequence_outcome(outcome, PickResult::Skipped));
    }

    if opts.commits.is_empty() {
        bail!("at least one commit or `--continue` / `--abort` / `--skip` is required");
    }

    let sources = commits_to_replay(&repo, &opts.commits).map_err(map_commit_spec_error)?;
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
        let prediction = predict_replay_sequence(&repo, &req).map_err(map_replay_error)?;
        return Ok(map_prediction_outcome(prediction));
    }
    let outcome = start_replay_sequence(&repo, &req).map_err(map_replay_error)?;
    Ok(map_sequence_outcome(outcome, PickResult::Picked))
}

impl PickOutcome {
    fn new(result: PickResult) -> Self {
        Self {
            result,
            commits: Vec::new(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            oid: None,
            check: false,
        }
    }
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

fn map_sequence_outcome(outcome: SequenceOutcome, ok_result: PickResult) -> PickOutcome {
    match outcome {
        SequenceOutcome::Completed { created, dropped } => PickOutcome {
            commits: created.iter().map(PickCommitRecord::from).collect(),
            dropped: dropped.iter().map(ObjectId::to_hex).collect(),
            ..PickOutcome::new(ok_result)
        },
        SequenceOutcome::Conflicts {
            conflicts,
            created_so_far,
            ..
        } => {
            let block = OperationConflictBlock::for_pick(&conflicts);
            PickOutcome {
                commits: created_so_far.iter().map(PickCommitRecord::from).collect(),
                conflicts: Some(block.conflicts),
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
    }
}

fn map_prediction_outcome(prediction: SequencePrediction) -> PickOutcome {
    let outcome = match prediction {
        SequencePrediction::Clean { tree, dropped } => PickOutcome {
            dropped: dropped.iter().map(ObjectId::to_hex).collect(),
            oid: Some(tree.to_hex()),
            ..PickOutcome::new(PickResult::Clean)
        },
        SequencePrediction::Conflicts {
            commit,
            subject,
            conflicts,
        } => PickOutcome {
            commits: vec![source_record(commit, &subject)],
            conflicts: Some(OperationConflictBlock::for_pick(&conflicts).conflicts),
            ..PickOutcome::new(PickResult::Conflicts)
        },
        SequencePrediction::Empty { commit, subject } => PickOutcome {
            commits: vec![source_record(commit, &subject)],
            ..PickOutcome::new(PickResult::Empty)
        },
    };
    PickOutcome {
        check: true,
        ..outcome
    }
}

fn map_replay_error(err: Error) -> anyhow::Error {
    match err {
        Error::DetachedHead => anyhow::anyhow!("HEAD is detached; grit pick needs a branch"),
        Error::UnbornHead => anyhow::anyhow!("no commits yet on this branch to pick onto"),
        Error::MergeCommit { oid } => {
            let s = &oid.to_hex()[..7];
            anyhow::anyhow!(
                "{s} is a merge commit — pass `--mainline <n>` to pick a merge commit (not yet supported on the CLI)"
            )
        }
        Error::ReplaySourceAtHead => {
            anyhow::anyhow!("nothing to pick — that commit is already the current HEAD")
        }
        Error::NoReplayInProgress => {
            anyhow::anyhow!("no cherry-pick in progress")
        }
        Error::ReplayContinueUnmerged => anyhow::anyhow!(
            "cannot continue: resolve conflicts and stage the result (`grit add`), then run `grit pick --continue`"
        ),
        Error::ReplaySkipAlreadyCommitted => anyhow::anyhow!(
            "nothing to skip: current step already committed; run `grit pick --continue` instead"
        ),
        Error::ReplayContinueDirtyIndex => anyhow::anyhow!(
            "cannot continue: index differs from HEAD after the step was committed"
        ),
        Error::Message(msg) => anyhow::anyhow!(msg),
        other => anyhow::Error::new(other),
    }
}
