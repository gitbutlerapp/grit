//! `grit revert` — undo one or more commits with new inverse commits.

use crate::commands::replay_merge_args::{mainline_parent, ReplayMergeArgs};
use crate::context;
use crate::markdown;
use crate::message_input::{read_message_input, MessageInputArgs};
use crate::operation_output::{
    render_conflicts_human, render_revert_conflict_footer_human, ConflictJson,
    OperationConflictBlock,
};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::revision_errors::map_commit_spec_error;
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    abort_replay_sequence, continue_replay_sequence, predict_replay_sequence, skip_replay_step,
    start_replay_sequence, ReplayCreated, ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest,
    SequenceOutcome, SequencePrediction,
};
use grit_lib::porcelain::revert::commits_to_revert;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// How to handle a revert step whose patch would not change `HEAD`.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum ReplayEmptyPolicyArg {
    /// Stop and leave sequencer state for `--continue` / `--skip`.
    #[default]
    Stop,
    /// Record an empty revert commit for the step.
    Keep,
    /// Skip the commit and continue with the next one.
    Drop,
}

impl From<ReplayEmptyPolicyArg> for ReplayEmptyPolicy {
    fn from(value: ReplayEmptyPolicyArg) -> Self {
        match value {
            ReplayEmptyPolicyArg::Stop => ReplayEmptyPolicy::Stop,
            ReplayEmptyPolicyArg::Keep => ReplayEmptyPolicy::Keep,
            ReplayEmptyPolicyArg::Drop => ReplayEmptyPolicy::Drop,
        }
    }
}

/// One revert commit created on the current branch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevertCommitEntry {
    /// Source commit that was reverted (full hex oid).
    pub source: String,
    /// New revert commit on the current branch (full hex oid).
    pub oid: String,
    /// Subject line of the revert commit.
    pub subject: String,
    /// Stored `change-id` on the revert commit, or JSON `null` when absent.
    pub change_id: Option<String>,
}

/// High-level result of `grit revert`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevertResult {
    /// Every requested step finished (or `--dry-run` printed the plan).
    Completed,
    /// Every step would apply cleanly (`grit revert --check` only).
    Clean,
    /// The sequence stopped on merge conflicts.
    Conflicts,
    /// `--abort` restored the pre-revert state.
    Aborted,
    /// A step would be empty and `--empty stop` halted the sequence.
    Empty,
}

/// Result of `grit revert`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevertOutcome {
    pub result: RevertResult,
    /// The last source commit reverted in this invocation (full hex), when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The last revert commit created (full hex), when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// Subject of the last revert commit, when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// `change-id` on the last revert commit, when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Revert commits created in this invocation (newest first).
    pub commits: Vec<RevertCommitEntry>,
    /// Source commits skipped as empty per `--empty drop`.
    pub dropped: Vec<String>,
    /// Conflicted paths when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    /// Suggested follow-up commands when `result` is `conflicts` or `empty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    /// Mutually exclusive options (`--abort`, `--skip`) when paused.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// True when `--dry-run` was set (no refs or objects were updated).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// True when this outcome is from `grit revert --check` (nothing was reverted).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
}

impl RevertOutcome {
    fn new(result: RevertResult) -> Self {
        Self {
            result,
            source: None,
            oid: None,
            subject: None,
            change_id: None,
            commits: Vec::new(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            check: false,
        }
    }

    fn dry_run(sources: Vec<ObjectId>) -> Self {
        Self {
            commits: sources
                .into_iter()
                .map(|oid| RevertCommitEntry {
                    source: oid.to_hex(),
                    oid: String::new(),
                    subject: String::new(),
                    change_id: None,
                })
                .collect(),
            dry_run: Some(true),
            ..Self::new(RevertResult::Completed)
        }
    }
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

impl HumanRender for RevertOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.result {
            RevertResult::Aborted => {
                outln!(w, "Revert aborted.");
            }
            RevertResult::Conflicts => {
                if let Some(details) = &self.conflicts {
                    if self.check {
                        crate::operation_output::render_revert_check_conflicts_human(details, w)?;
                    } else {
                        render_conflicts_human(details, w)?;
                        render_revert_conflict_footer_human();
                    }
                }
            }
            RevertResult::Clean => {
                if let Some(oid) = &self.oid {
                    outln!(w, "Would revert cleanly (tree {oid})");
                } else {
                    outln!(w, "Would revert cleanly");
                }
            }
            RevertResult::Empty => {
                if self.check {
                    outln!(w, "Would stop on an empty revert step.");
                } else {
                    outln!(w, "Revert stopped on an empty commit.");
                    if let Some(steps) = &self.next_steps {
                        eprintln!("→ {}, or `--skip` / `--abort`.", steps.join(", then "));
                    }
                }
            }
            RevertResult::Completed => {
                if self.dry_run == Some(true) {
                    outln!(w, "Would revert {} commit(s):", self.commits.len());
                    for entry in &self.commits {
                        let short = entry.source.get(..7).unwrap_or(&entry.source);
                        outln!(w, "  {short}");
                    }
                    return Ok(());
                }
                for entry in &self.commits {
                    let new_short = entry.oid.get(..7).unwrap_or(&entry.oid);
                    let src_short = entry.source.get(..7).unwrap_or(&entry.source);
                    outln!(w, "Reverted {src_short} → {new_short} {}", entry.subject);
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RevertOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_revert_outcome(w, self)?;
        Ok(())
    }
}

/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct RevertRunOptions {
    pub commits: Vec<String>,
    pub mainline: Option<u32>,
    pub continue_revert: bool,
    pub abort: bool,
    pub skip: bool,
    pub no_commit: bool,
    pub empty: ReplayEmptyPolicyArg,
    pub message: MessageInputArgs,
    pub dry_run: bool,
    pub check: bool,
    #[allow(clippy::struct_field_names)]
    pub replay_merge: ReplayMergeArgs,
}

/// Revert commits onto the current branch, or continue/skip/abort an in-progress sequence.
pub fn run(opts: &RevertRunOptions) -> Result<RevertOutcome> {
    let repo = context::discover()?;
    if opts.abort {
        abort_replay_sequence(&repo).map_err(map_replay_error)?;
        return Ok(RevertOutcome::new(RevertResult::Aborted));
    }

    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        context::wall_clock_now(env),
    )?;

    if opts.continue_revert {
        let outcome = continue_replay_sequence(&repo, &committer).map_err(map_replay_error)?;
        return Ok(map_sequence_outcome(outcome));
    }
    if opts.skip {
        let outcome = skip_replay_step(&repo, &committer).map_err(map_replay_error)?;
        return Ok(map_sequence_outcome(outcome));
    }

    if opts.commits.is_empty() {
        bail!("at least one commit is required unless using --continue, --abort, or --skip");
    }
    let sources = commits_to_revert(&repo, &opts.commits).map_err(map_commit_spec_error)?;
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
        let prediction = predict_replay_sequence(&repo, &req).map_err(map_replay_error)?;
        return Ok(map_prediction_outcome(prediction));
    }
    let outcome = start_replay_sequence(&repo, &req).map_err(map_replay_error)?;
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
        SequenceOutcome::Completed { created, dropped } => {
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
                ..RevertOutcome::new(RevertResult::Completed)
            }
        }
        SequenceOutcome::Conflicts {
            conflicts,
            created_so_far,
            ..
        } => {
            let block = OperationConflictBlock::for_revert(&conflicts);
            RevertOutcome {
                commits: created_so_far.iter().map(RevertCommitEntry::from).collect(),
                conflicts: Some(block.conflicts),
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

fn map_replay_error(err: Error) -> anyhow::Error {
    match err {
        Error::DetachedHead => anyhow::anyhow!("HEAD is detached; grit revert needs a branch"),
        Error::UnbornHead => anyhow::anyhow!("no commits yet on this branch to revert onto"),
        Error::MergeCommit { oid } => {
            let s = &oid.to_hex()[..7];
            anyhow::anyhow!(
                "{s} is a merge commit — pass `--mainline 1` (or another parent number) to revert it"
            )
        }
        Error::ReplaySourceAtHead => {
            anyhow::anyhow!("nothing to revert — that commit is already the current HEAD")
        }
        Error::ReplayContinueUnmerged => anyhow::anyhow!(
            "cannot continue revert: resolve conflicts and stage the result first (`grit add`)"
        ),
        Error::NothingToCommit => {
            anyhow::anyhow!("nothing to commit — the revert would not change the tree")
        }
        Error::NoReplayInProgress => {
            anyhow::anyhow!("no revert in progress")
        }
        Error::ReplayAbortHeadMoved => anyhow::anyhow!(
            "cannot abort revert: the current branch moved since the revert started"
        ),
        Error::Message(msg) => anyhow::anyhow!(msg),
        other => anyhow::Error::new(other),
    }
}
