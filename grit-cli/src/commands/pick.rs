//! `grit pick` — cherry-pick one or more commits onto the current branch.

use crate::commands::replay_merge_args::ReplayMergeArgs;
use crate::context;
use crate::markdown;
use crate::message_input::{read_message_input, MessageInputArgs};
use crate::operation_output::{
    render_conflicts_human, render_pick_check_conflicts_human, render_pick_conflict_footer_human,
    ConflictJson, OperationConflictBlock,
};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use grit_lib::change_id::resolved_change_id;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{
    next_step_grit_commands, operation_alternatives, operation_next_steps_empty,
};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    abort_replay_sequence, commits_to_replay, continue_replay_sequence, predict_replay_sequence,
    skip_replay_step, start_replay_sequence, ReplayEmptyPolicy, ReplayOptions,
    ReplaySequenceRequest, SequenceOutcome, SequencePrediction,
};
use grit_lib::state::InProgressOperation;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;
use std::num::NonZeroU32;

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

/// How to handle an empty replay step (`--empty`).
#[derive(Clone, Copy, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum PickEmptyArg {
    #[default]
    Stop,
    Keep,
    Drop,
}

impl From<PickEmptyArg> for ReplayEmptyPolicy {
    fn from(value: PickEmptyArg) -> Self {
        match value {
            PickEmptyArg::Stop => ReplayEmptyPolicy::Stop,
            PickEmptyArg::Keep => ReplayEmptyPolicy::Keep,
            PickEmptyArg::Drop => ReplayEmptyPolicy::Drop,
        }
    }
}

/// Options for [`run`].
pub struct PickRunOptions {
    pub commits: Vec<String>,
    pub continue_pick: bool,
    pub abort: bool,
    pub skip: bool,
    pub record_origin: bool,
    pub no_commit: bool,
    pub empty: PickEmptyArg,
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
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        context::wall_clock_now(env),
    )?;

    if (opts.continue_pick || opts.abort || opts.skip) && !opts.commits.is_empty() {
        bail!("`--continue`, `--abort`, and `--skip` cannot be used with commit arguments");
    }

    if opts.abort {
        abort_replay_sequence(&repo).map_err(map_replay_error)?;
        return Ok(PickOutcome {
            result: PickResult::Aborted,
            commits: Vec::new(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            oid: None,
            check: false,
        });
    }

    if opts.continue_pick {
        let outcome = continue_replay_sequence(&repo, &committer).map_err(map_replay_error)?;
        return Ok(map_sequence_outcome(
            &repo,
            &[],
            outcome,
            PickResult::Picked,
        ));
    }

    if opts.skip {
        let outcome = skip_replay_step(&repo, &committer).map_err(map_replay_error)?;
        return Ok(map_sequence_outcome(
            &repo,
            &[],
            outcome,
            PickResult::Skipped,
        ));
    }

    if opts.commits.is_empty() {
        bail!("at least one commit or `--continue` / `--abort` / `--skip` is required");
    }

    let sources = commits_to_replay(&repo, &opts.commits).map_err(map_pick_committish_error)?;
    if opts.dry_run {
        return Ok(PickOutcome {
            result: PickResult::Picked,
            commits: sources
                .iter()
                .map(|oid| PickCommitRecord {
                    source: oid.to_hex(),
                    oid: None,
                    change_id: None,
                    subject: String::new(),
                })
                .collect(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: Some(true),
            oid: None,
            check: false,
        });
    }
    let favor = opts.replay_merge.favor();
    let conflict_style = opts.replay_merge.conflict_style(&repo);
    if opts.check {
        return run_check(&repo, sources, &opts, favor, conflict_style);
    }

    let message_override = read_message_input(&opts.message)?;
    if message_override.is_some() && opts.commits.len() != 1 {
        bail!("--message and --message-file are only allowed when picking exactly one commit");
    }

    let mainline = opts
        .mainline
        .map(|n| {
            NonZeroU32::new(n).ok_or_else(|| {
                anyhow::anyhow!("--mainline requires a positive parent number (1, 2, …)")
            })
        })
        .transpose()?;
    let replay_opts = ReplayOptions {
        record_origin: opts.record_origin,
        no_commit: opts.no_commit,
        empty: opts.empty.into(),
        message_override,
        mainline,
        favor,
        conflict_style,
    };
    let req = ReplaySequenceRequest {
        commits: sources.clone(),
        direction: ReplayDirection::Pick,
        committer,
        options: replay_opts,
    };
    let outcome = start_replay_sequence(&repo, &req).map_err(map_replay_error)?;
    Ok(map_sequence_outcome(
        &repo,
        &sources,
        outcome,
        PickResult::Picked,
    ))
}

fn map_sequence_outcome(
    repo: &grit_lib::repo::Repository,
    sources: &[ObjectId],
    outcome: SequenceOutcome,
    ok_result: PickResult,
) -> PickOutcome {
    match outcome {
        SequenceOutcome::Completed { created, dropped } => {
            let (commits, dropped_hex) = if sources.is_empty() {
                (
                    records_from_replay_created(repo, &created),
                    dropped.iter().map(ObjectId::to_hex).collect(),
                )
            } else {
                records_for_completed_from_replay(repo, sources, &created, &dropped)
            };
            PickOutcome {
                result: ok_result,
                commits,
                dropped: dropped_hex,
                conflicts: None,
                next_steps: None,
                alternatives: None,
                dry_run: None,
                oid: None,
                check: false,
            }
        }
        SequenceOutcome::Conflicts {
            conflicts,
            created_so_far,
            ..
        } => {
            let commits = records_from_replay_created(repo, &created_so_far);
            let block = OperationConflictBlock::for_pick(&conflicts);
            PickOutcome {
                result: PickResult::Conflicts,
                commits,
                dropped: Vec::new(),
                conflicts: Some(block.conflicts),
                next_steps: Some(block.next_steps),
                alternatives: block.alternatives,
                dry_run: None,
                oid: None,
                check: false,
            }
        }
        SequenceOutcome::Empty {
            current,
            created_so_far,
            ..
        } => {
            let mut commits = records_from_replay_created(repo, &created_so_far);
            commits.push(source_record(repo, current, None));
            let next_steps = next_step_grit_commands(&operation_next_steps_empty(
                InProgressOperation::CherryPick,
            ));
            let alternatives =
                next_step_grit_commands(&operation_alternatives(InProgressOperation::CherryPick));
            PickOutcome {
                result: PickResult::Empty,
                commits,
                dropped: Vec::new(),
                conflicts: None,
                next_steps: Some(next_steps),
                alternatives: if alternatives.is_empty() {
                    None
                } else {
                    Some(alternatives)
                },
                dry_run: None,
                oid: None,
                check: false,
            }
        }
    }
}

fn run_check(
    repo: &grit_lib::repo::Repository,
    sources: Vec<ObjectId>,
    opts: &PickRunOptions,
    favor: grit_lib::diff::merge_file::MergeFavor,
    conflict_style: grit_lib::diff::merge_file::ConflictStyle,
) -> Result<PickOutcome> {
    let mainline = opts
        .mainline
        .map(|n| {
            NonZeroU32::new(n).ok_or_else(|| {
                anyhow::anyhow!("--mainline requires a positive parent number (1, 2, …)")
            })
        })
        .transpose()?;
    let replay_opts = ReplayOptions {
        record_origin: opts.record_origin,
        no_commit: opts.no_commit,
        empty: opts.empty.into(),
        message_override: None,
        mainline,
        favor,
        conflict_style,
    };
    let prediction = predict_replay_sequence(
        repo,
        &ReplaySequenceRequest {
            commits: sources,
            direction: ReplayDirection::Pick,
            committer: String::new(),
            options: replay_opts,
        },
    )
    .map_err(map_replay_error)?;
    map_prediction_outcome(repo, prediction)
}

fn map_prediction_outcome(
    repo: &grit_lib::repo::Repository,
    prediction: SequencePrediction,
) -> Result<PickOutcome> {
    match prediction {
        SequencePrediction::Clean { tree, dropped } => Ok(PickOutcome {
            result: PickResult::Clean,
            commits: Vec::new(),
            dropped: dropped.iter().map(ObjectId::to_hex).collect(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            oid: Some(tree.to_hex()),
            check: true,
        }),
        SequencePrediction::Conflicts { commit, conflicts } => {
            let block = OperationConflictBlock::for_pick(&conflicts);
            Ok(PickOutcome {
                result: PickResult::Conflicts,
                commits: vec![source_record(repo, commit, None)],
                dropped: Vec::new(),
                conflicts: Some(block.conflicts),
                next_steps: None,
                alternatives: None,
                dry_run: None,
                oid: None,
                check: true,
            })
        }
        SequencePrediction::Empty { commit } => Ok(PickOutcome {
            result: PickResult::Empty,
            commits: vec![source_record(repo, commit, None)],
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            oid: None,
            check: true,
        }),
    }
}

fn records_for_completed_from_replay(
    repo: &grit_lib::repo::Repository,
    _sources: &[ObjectId],
    created: &[grit_lib::porcelain::replay_sequence::ReplayCreated],
    dropped: &[ObjectId],
) -> (Vec<PickCommitRecord>, Vec<String>) {
    (
        records_from_replay_created(repo, created),
        dropped.iter().map(ObjectId::to_hex).collect(),
    )
}

fn records_from_replay_created(
    repo: &grit_lib::repo::Repository,
    created: &[grit_lib::porcelain::replay_sequence::ReplayCreated],
) -> Vec<PickCommitRecord> {
    created
        .iter()
        .map(|record| source_record(repo, record.source, Some(record.new_oid)))
        .collect()
}

fn source_record(
    repo: &grit_lib::repo::Repository,
    source: ObjectId,
    new_oid: Option<ObjectId>,
) -> PickCommitRecord {
    let source_commit = context::read_commit(repo, &source).ok();
    let subject = source_commit
        .as_ref()
        .map(|c| context::subject_line(&c.message))
        .unwrap_or_default();
    let (oid, change_id, subject) = if let Some(new_oid) = new_oid {
        let new_commit = context::read_commit(repo, &new_oid).ok();
        let subject = new_commit
            .as_ref()
            .map(|c| context::subject_line(&c.message))
            .unwrap_or(subject);
        let change_id = new_commit.and_then(|c| {
            let resolved = resolved_change_id(&new_oid, &c);
            resolved.stored.then(|| resolved.id.as_str().to_owned())
        });
        (Some(new_oid.to_hex()), change_id, subject)
    } else {
        (None, None, subject)
    };
    PickCommitRecord {
        source: source.to_hex(),
        oid,
        change_id,
        subject,
    }
}

fn map_pick_committish_error(err: Error) -> anyhow::Error {
    if let Some(msg) = crate::revision_errors::map_lib_revision_error(&err) {
        return anyhow::anyhow!(msg);
    }
    match err {
        Error::InvalidRef(msg) => anyhow::anyhow!(msg),
        Error::ObjectNotFound(msg) => anyhow::anyhow!("could not resolve commit: {msg}"),
        other => anyhow::Error::new(other),
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
        Error::ReplayAbortHeadMoved => {
            anyhow::anyhow!("cannot abort: branch tip moved since the pick started")
        }
        Error::Message(msg) => anyhow::anyhow!(msg),
        other => anyhow::Error::new(other),
    }
}
