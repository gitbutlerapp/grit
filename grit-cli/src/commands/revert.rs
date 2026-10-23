//! `grit revert` — undo one or more commits with new inverse commits.

use crate::commands::replay_merge_args::ReplayMergeArgs;
use crate::context;
use crate::markdown;
use crate::operation_output::{
    render_conflicts_human, render_revert_conflict_footer_human, ConflictJson,
    OperationConflictBlock,
};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
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
use grit_lib::porcelain::revert::{committer_timestamp, revision_range_apply_order};
use grit_lib::revwalk::parse_rev_walk_tokens;
use grit_lib::revwalk::rev_list::resolve_revision_specs_to_commits;
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;
use std::num::NonZeroU32;

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
    fn aborted() -> Self {
        Self {
            result: RevertResult::Aborted,
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
            result: RevertResult::Completed,
            source: None,
            oid: None,
            subject: None,
            change_id: None,
            commits: sources
                .into_iter()
                .map(|oid| RevertCommitEntry {
                    source: oid.to_hex(),
                    oid: String::new(),
                    subject: String::new(),
                    change_id: None,
                })
                .collect(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: Some(true),
            check: false,
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
    pub message: Option<String>,
    pub message_file: Option<String>,
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
        return Ok(RevertOutcome::aborted());
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
        return map_sequence_outcome(&repo, outcome);
    }
    if opts.skip {
        let outcome = skip_replay_step(&repo, &committer).map_err(map_replay_error)?;
        return map_sequence_outcome(&repo, outcome);
    }

    let sources = resolve_revert_commits(&repo, &opts.commits)?;
    if opts.dry_run {
        return Ok(RevertOutcome::dry_run(sources));
    }

    let favor = opts.replay_merge.favor();
    let conflict_style = opts.replay_merge.conflict_style(&repo);

    if opts.check {
        return run_check(&repo, sources, opts, favor, conflict_style);
    }

    let message_override =
        read_message_override(opts.message.as_deref(), opts.message_file.as_deref())?;
    let mainline = opts
        .mainline
        .map(|n| {
            NonZeroU32::new(n).ok_or_else(|| {
                anyhow::anyhow!("--mainline requires a positive parent number (1, 2, …)")
            })
        })
        .transpose()?;
    let replay_opts = ReplayOptions {
        no_commit: opts.no_commit,
        empty: opts.empty.into(),
        message_override,
        mainline,
        favor,
        conflict_style,
        ..ReplayOptions::default()
    };

    let outcome = start_replay_sequence(
        &repo,
        &ReplaySequenceRequest {
            commits: sources,
            direction: ReplayDirection::Revert,
            committer,
            options: replay_opts,
        },
    )
    .map_err(map_replay_error)?;
    map_sequence_outcome(&repo, outcome)
}

fn run_check(
    repo: &grit_lib::repo::Repository,
    sources: Vec<ObjectId>,
    opts: &RevertRunOptions,
    favor: grit_lib::diff::merge_file::MergeFavor,
    conflict_style: grit_lib::diff::merge_file::ConflictStyle,
) -> Result<RevertOutcome> {
    let mainline = opts
        .mainline
        .map(|n| {
            NonZeroU32::new(n).ok_or_else(|| {
                anyhow::anyhow!("--mainline requires a positive parent number (1, 2, …)")
            })
        })
        .transpose()?;
    let replay_opts = ReplayOptions {
        no_commit: opts.no_commit,
        empty: opts.empty.into(),
        message_override: None,
        mainline,
        favor,
        conflict_style,
        ..ReplayOptions::default()
    };
    let prediction = predict_replay_sequence(
        repo,
        &ReplaySequenceRequest {
            commits: sources,
            direction: ReplayDirection::Revert,
            committer: String::new(),
            options: replay_opts,
        },
    )
    .map_err(map_replay_error)?;
    map_prediction_outcome(prediction)
}

fn map_prediction_outcome(prediction: SequencePrediction) -> Result<RevertOutcome> {
    match prediction {
        SequencePrediction::Clean { tree, dropped } => Ok(RevertOutcome {
            result: RevertResult::Clean,
            source: None,
            oid: Some(tree.to_hex()),
            subject: None,
            change_id: None,
            commits: Vec::new(),
            dropped: dropped.iter().map(ObjectId::to_hex).collect(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            check: true,
        }),
        SequencePrediction::Conflicts { commit, conflicts } => {
            let block = OperationConflictBlock::for_revert(&conflicts);
            Ok(RevertOutcome {
                result: RevertResult::Conflicts,
                source: Some(commit.to_hex()),
                oid: None,
                subject: None,
                change_id: None,
                commits: Vec::new(),
                dropped: Vec::new(),
                conflicts: Some(block.conflicts),
                next_steps: None,
                alternatives: None,
                dry_run: None,
                check: true,
            })
        }
        SequencePrediction::Empty { commit } => Ok(RevertOutcome {
            result: RevertResult::Empty,
            source: Some(commit.to_hex()),
            oid: None,
            subject: None,
            change_id: None,
            commits: Vec::new(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            check: true,
        }),
    }
}

fn resolve_revert_commits(
    repo: &grit_lib::repo::Repository,
    tokens: &[String],
) -> Result<Vec<ObjectId>> {
    if tokens.is_empty() {
        bail!("at least one commit is required unless using --continue, --abort, or --skip");
    }
    let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let parsed = parse_rev_walk_tokens(repo, &refs).map_err(anyhow::Error::new)?;
    let include = resolve_revision_specs_to_commits(repo, &parsed.positive)
        .map_err(map_revert_committish_error)?;
    let exclude = resolve_revision_specs_to_commits(repo, &parsed.negative)
        .map_err(map_revert_committish_error)?;
    let range_syntax = tokens
        .iter()
        .any(|t| t.contains("..") || t.contains("...") || t.starts_with('^'))
        || !exclude.is_empty()
        || parsed.symmetric_left.is_some();
    if range_syntax {
        return revision_range_apply_order(repo, &include, &exclude).map_err(anyhow::Error::new);
    }
    if tokens.len() > 1 {
        return tokens
            .iter()
            .map(|token| {
                resolve_revision_as_commit(repo, token).map_err(map_revert_committish_error)
            })
            .collect::<Result<Vec<_>>>();
    }
    let commits: Vec<ObjectId> = tokens
        .iter()
        .map(|token| resolve_revision_as_commit(repo, token).map_err(map_revert_committish_error))
        .collect::<Result<_>>()?;
    order_commits_newest_first(repo, &commits)
}

fn order_commits_newest_first(
    repo: &grit_lib::repo::Repository,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let mut dated: Vec<(i64, ObjectId)> = commits
        .iter()
        .map(|oid| {
            let commit = context::read_commit(repo, oid)?;
            Ok((committer_timestamp(&commit.committer), *oid))
        })
        .collect::<Result<Vec<_>>>()?;
    dated.sort_by_key(|(ts, oid)| (std::cmp::Reverse(*ts), *oid));
    Ok(dated.into_iter().map(|(_, oid)| oid).collect())
}

fn map_sequence_outcome(
    repo: &grit_lib::repo::Repository,
    outcome: SequenceOutcome,
) -> Result<RevertOutcome> {
    match outcome {
        SequenceOutcome::Completed { created, dropped } => {
            build_completed_outcome(repo, &created, &dropped)
        }
        SequenceOutcome::Conflicts {
            conflicts,
            created_so_far,
            ..
        } => Ok(build_conflicts_outcome(&conflicts, &created_so_far, repo)),
        SequenceOutcome::Empty { created_so_far, .. } => {
            Ok(build_empty_outcome(repo, &created_so_far))
        }
    }
}

fn build_completed_outcome(
    repo: &grit_lib::repo::Repository,
    created: &[ReplayCreated],
    dropped: &[ObjectId],
) -> Result<RevertOutcome> {
    let commits = created
        .iter()
        .map(|record| entry_from_record(repo, record))
        .collect::<Result<Vec<_>>>()?;
    let (source, oid, subject, change_id) = commits.last().map_or((None, None, None, None), |e| {
        (
            Some(e.source.clone()),
            Some(e.oid.clone()),
            Some(e.subject.clone()),
            e.change_id.clone(),
        )
    });
    Ok(RevertOutcome {
        result: RevertResult::Completed,
        source,
        oid,
        subject,
        change_id,
        commits,
        dropped: dropped.iter().map(ObjectId::to_hex).collect(),
        conflicts: None,
        next_steps: None,
        alternatives: None,
        dry_run: None,
        check: false,
    })
}

fn build_conflicts_outcome(
    conflicts: &[grit_lib::operation::Conflict],
    created_so_far: &[ReplayCreated],
    repo: &grit_lib::repo::Repository,
) -> RevertOutcome {
    let block = OperationConflictBlock::for_revert(conflicts);
    let commits = created_so_far
        .iter()
        .filter_map(|record| entry_from_record(repo, record).ok())
        .collect();
    RevertOutcome {
        result: RevertResult::Conflicts,
        source: None,
        oid: None,
        subject: None,
        change_id: None,
        commits,
        dropped: Vec::new(),
        conflicts: Some(block.conflicts),
        next_steps: Some(block.next_steps),
        alternatives: block.alternatives,
        dry_run: None,
        check: false,
    }
}

fn build_empty_outcome(
    repo: &grit_lib::repo::Repository,
    created_so_far: &[ReplayCreated],
) -> RevertOutcome {
    let block = OperationConflictBlock::for_revert(&[]);
    let commits = created_so_far
        .iter()
        .filter_map(|record| entry_from_record(repo, record).ok())
        .collect();
    RevertOutcome {
        result: RevertResult::Empty,
        source: None,
        oid: None,
        subject: None,
        change_id: None,
        commits,
        dropped: Vec::new(),
        conflicts: None,
        next_steps: Some(block.next_steps),
        alternatives: block.alternatives,
        dry_run: None,
        check: false,
    }
}

fn entry_from_record(
    repo: &grit_lib::repo::Repository,
    record: &ReplayCreated,
) -> Result<RevertCommitEntry> {
    let commit = context::read_commit(repo, &record.new_oid)?;
    Ok(RevertCommitEntry {
        source: record.source.to_hex(),
        oid: record.new_oid.to_hex(),
        subject: context::subject_line(&commit.message),
        change_id: record.change_id.as_ref().map(|id| id.as_str().to_owned()),
    })
}

fn read_message_override(
    message: Option<&str>,
    message_file: Option<&str>,
) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(_), Some(_)) => bail!("cannot use both --message and --message-file"),
        (Some(text), None) => Ok(Some(text.to_owned())),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(Some)
            .with_context(|| format!("could not read message file '{path}'")),
        (None, None) => Ok(None),
    }
}

fn map_revert_committish_error(err: Error) -> anyhow::Error {
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
