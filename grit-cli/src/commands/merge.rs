//! `grit merge` — merge another branch, tag or commit into the current branch.
//!
//! Fast-forwards when possible; otherwise performs a real three-way merge and
//! records a merge commit. Conflicts leave merge-in-progress state with conflict
//! markers in the worktree.

use crate::context;
use crate::json_error;
use crate::markdown;
use crate::merge_strategy_args::MergeStrategyArgs;
use crate::message_input::{read_message_input, MessageInputArgs};
use crate::operation_output::{
    conflict_json, render_conflicts_human, ConflictJson, OperationConflictBlock,
};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

pub use crate::render::conflict::MergeConflictJson;
use crate::ui::path_display_for_discovered_repo;
use anyhow::{bail, Context, Result};
use grit_lib::change_id::ChangeId;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::merge::{
    ContinueOptions, FastForwardPolicy, IntegrateBranchOutcome, MergeCommitMode, MergeDryRunPlan,
    MergeOptions, MergePrediction, MergePredictionOptions, MergeSource,
};
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::operation::Conflict;
use grit_lib::porcelain::commit::HookPolicy;
use grit_lib::porcelain::MergeError;
use grit_lib::progress::CancelToken;
use grit_lib::progress::ProgressSink;
use grit_lib::repo::Repository;
use grit_lib::state::HeadState;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// CLI flags for `grit merge` beyond the revision and `--abort`.
#[derive(Debug, Clone, Default)]
pub struct MergeRunOptions {
    pub ff_only: bool,
    pub no_ff: bool,
    pub no_commit: bool,
    pub squash: bool,
    pub allow_unrelated_histories: bool,
    pub no_verify: bool,
    pub strategy: MergeStrategyArgs,
    pub message: MessageInputArgs,
}

/// Merge / pull integration result (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeResult {
    UpToDate,
    FastForward,
    /// Three-way merge would succeed (`grit merge --check` only).
    Clean,
    Merged,
    Conflicts,
    Aborted,
    Squashed,
    Stopped,
    RefusedNotFf,
}

/// Result of `grit merge` (and the merge half of `grit pull`).
#[derive(Serialize, JsonSchema)]
pub struct MergeOutcome {
    pub result: MergeResult,
    /// The merge source label.
    pub branch: String,
    /// Current branch whose tip moved after a fast-forward; omitted when it matches `branch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub updated_branch: Option<String>,
    /// Resulting commit (fast-forward target or merge commit).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// Stored `change-id` on the resulting commit when `oid` is set and a header is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Structured conflicts when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    /// Rerere replay or record events when `rerere.enabled` is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rerere: Vec<crate::operation_output::RerereEventJson>,
    /// Suggested `grit` commands when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    /// Abort/skip options when `result` is `conflicts` (not part of the ordered sequence).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// True when this outcome is from `grit merge --check` (nothing was merged).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
    /// True when this outcome is from `grit merge --dry-run` (nothing was merged).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Parent commits of a predicted merge commit (`--dry-run` with `result: merged`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub parents: Option<Vec<String>>,
    /// True when `HEAD` was detached for this merge (not serialized in `--json`).
    #[serde(skip)]
    pub(crate) detached_head: bool,
    /// Library conflicts for safe human/markdown path display (JSON uses [`Self::conflicts`]).
    #[serde(skip)]
    pub(crate) conflict_sources: Option<Vec<Conflict>>,
}

pub(crate) fn merge_conflict_json(detail: &Conflict, merge_label: &str) -> MergeConflictJson {
    let ConflictJson {
        path,
        kind,
        summary,
    } = conflict_json(detail, Some(merge_label));
    MergeConflictJson {
        path,
        kind,
        summary,
    }
}

impl MergeOutcome {
    fn up_to_date(label: &str) -> Self {
        Self {
            result: MergeResult::UpToDate,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn up_to_date_check(label: &str) -> Self {
        let mut o = Self::up_to_date(label);
        o.check = true;
        o
    }

    fn up_to_date_dry_run(label: &str) -> Self {
        let mut o = Self::up_to_date(label);
        o.dry_run = Some(true);
        o
    }

    fn fast_forward(
        label: &str,
        head_short: &str,
        oid: ObjectId,
        change_id: Option<ChangeId>,
    ) -> Self {
        Self {
            result: MergeResult::FastForward,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: fast_forward_updated_branch(head_short, label),
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn fast_forward_check(
        label: &str,
        head_short: &str,
        oid: ObjectId,
        change_id: Option<ChangeId>,
    ) -> Self {
        Self {
            result: MergeResult::FastForward,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: true,
            dry_run: None,
            parents: None,
            updated_branch: fast_forward_updated_branch(head_short, label),
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn fast_forward_dry_run(
        label: &str,
        head_short: &str,
        oid: ObjectId,
        change_id: Option<ChangeId>,
    ) -> Self {
        let mut o = Self::fast_forward(label, head_short, oid, change_id);
        o.dry_run = Some(true);
        o
    }

    fn merged(label: &str, oid: ObjectId, change_id: Option<ChangeId>) -> Self {
        Self {
            result: MergeResult::Merged,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn merged_dry_run(label: &str, parents: Vec<ObjectId>) -> Self {
        Self {
            result: MergeResult::Merged,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: Some(true),
            parents: Some(parents.iter().map(ObjectId::to_hex).collect()),
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn clean_check(label: &str, tree: ObjectId) -> Self {
        Self {
            result: MergeResult::Clean,
            branch: label.to_owned(),
            oid: Some(tree.to_hex()),
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: true,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn conflicts_check(label: &str, details: Vec<Conflict>) -> Self {
        let conflict_sources = Some(details.clone());
        let conflicts = Some(
            details
                .iter()
                .map(|d| conflict_json(d, Some(label)))
                .collect(),
        );
        Self {
            result: MergeResult::Conflicts,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts,
            next_steps: None,
            alternatives: None,
            check: true,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources,
        }
    }

    fn conflicts_dry_run(label: &str, details: Vec<Conflict>) -> Self {
        let mut o = Self::conflicts_check(label, details);
        o.check = false;
        o.dry_run = Some(true);
        o
    }

    fn conflicts(
        label: &str,
        details: Vec<Conflict>,
        rerere: Vec<grit_lib::diff::rerere::RerereEvent>,
    ) -> Self {
        let block = OperationConflictBlock::for_merge(&details, label);
        Self {
            result: MergeResult::Conflicts,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: Some(block.conflicts),
            rerere: crate::operation_output::rerere_events_json(&rerere),
            next_steps: Some(block.next_steps),
            alternatives: block.alternatives,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            detached_head: false,
            conflict_sources: Some(details),
        }
    }

    fn squashed(label: &str) -> Self {
        Self {
            result: MergeResult::Squashed,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: Some(vec![
                "grit commit".into(),
                "Review SQUASH_MSG or pass -m / --message-file".into(),
            ]),
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn stopped(label: &str) -> Self {
        Self {
            result: MergeResult::Stopped,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: Some(vec!["grit commit".into(), "grit merge --abort".into()]),
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn refused_not_ff(label: &str) -> Self {
        Self {
            result: MergeResult::RefusedNotFf,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn refused_not_ff_dry_run(label: &str) -> Self {
        let mut o = Self::refused_not_ff(label);
        o.dry_run = Some(true);
        o
    }

    fn aborted() -> Self {
        Self {
            result: MergeResult::Aborted,
            branch: String::new(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    fn for_detached_head(mut self) -> Self {
        self.detached_head = true;
        self.updated_branch = None;
        self
    }
}

fn fast_forward_updated_branch(head_short: &str, merge_label: &str) -> Option<String> {
    (head_short != merge_label).then(|| head_short.to_owned())
}

fn write_fast_forward_human(w: &mut dyn Write, outcome: &MergeOutcome, would: bool) -> Result<()> {
    let short = outcome.oid.as_deref().map(short_hex).unwrap_or_default();
    let head = if outcome.detached_head {
        "HEAD"
    } else {
        outcome
            .updated_branch
            .as_deref()
            .unwrap_or(outcome.branch.as_str())
    };
    let source = outcome
        .updated_branch
        .as_ref()
        .map(|_| outcome.branch.as_str());
    match (would, source) {
        (true, Some(src)) => outln!(w, "Would fast-forward {head} → {short} ({src})"),
        (true, None) => outln!(w, "Would fast-forward {head} → {short}"),
        (false, Some(src)) => outln!(w, "Fast-forwarded {head} → {short} ({src})"),
        (false, None) => outln!(w, "Fast-forwarded {head} → {short}"),
    }
    Ok(())
}

impl HumanRender for MergeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let short = self.oid.as_deref().map(short_hex).unwrap_or_default();
        match self.result {
            MergeResult::UpToDate if self.dry_run == Some(true) => {
                if self.detached_head {
                    outln!(w, "Would leave HEAD up to date with {}.", self.branch);
                } else {
                    outln!(
                        w,
                        "Would leave the current branch up to date with {}.",
                        self.branch
                    );
                }
            }
            MergeResult::UpToDate => outln!(w, "Already up to date."),
            MergeResult::FastForward if self.check || self.dry_run == Some(true) => {
                write_fast_forward_human(w, self, true)?;
            }
            MergeResult::FastForward => write_fast_forward_human(w, self, false)?,
            MergeResult::Clean => outln!(w, "Would merge {} cleanly (tree {short})", self.branch),
            MergeResult::Merged if self.dry_run == Some(true) => {
                if self.detached_head {
                    outln!(w, "Would merge {} into HEAD (merge commit)", self.branch);
                } else {
                    outln!(
                        w,
                        "Would merge {} into the current branch (merge commit)",
                        self.branch
                    );
                }
            }
            MergeResult::Merged if self.detached_head => {
                outln!(w, "Merged {} into HEAD ({short})", self.branch);
                crate::operation_output::render_rerere_human(&self.rerere);
            }
            MergeResult::Merged => {
                outln!(
                    w,
                    "Merged {} into the current branch ({short})",
                    self.branch
                );
                crate::operation_output::render_rerere_human(&self.rerere);
            }
            MergeResult::Squashed => {
                outln!(
                    w,
                    "Squash merge applied from {} — run `grit commit` to record the squashed commit",
                    self.branch
                );
            }
            MergeResult::Stopped => {
                outln!(
                    w,
                    "Merge from {} applied — run `grit commit` to finish, or `grit merge --abort` to cancel",
                    self.branch
                );
            }
            MergeResult::RefusedNotFf if self.dry_run == Some(true) => {
                outln!(
                    w,
                    "Would refuse to fast-forward {} — histories have diverged (use merge without --ff-only)",
                    self.branch
                );
            }
            MergeResult::RefusedNotFf => {
                outln!(
                    w,
                    "Cannot fast-forward {} — histories have diverged (use merge without --ff-only)",
                    self.branch
                );
            }
            MergeResult::Conflicts if self.check || self.dry_run == Some(true) => {
                if let Some(details) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    crate::operation_output::render_merge_check_conflicts_human(
                        &self.branch,
                        details,
                        path_opts.as_ref(),
                        w,
                    )?;
                }
            }
            MergeResult::Conflicts => {
                if let Some(details) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    render_conflicts_human(
                        details,
                        Some(self.branch.as_str()),
                        path_opts.as_ref(),
                        w,
                    )?;
                }
                crate::operation_output::render_rerere_human(&self.rerere);
                crate::operation_output::render_merge_conflict_footer_human();
            }
            MergeResult::Aborted => outln!(w, "Merge aborted."),
        }
        Ok(())
    }
}

impl MarkdownRender for MergeOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_merge_outcome(w, self)?;
        Ok(())
    }
}

pub fn run(
    branch: Option<&str>,
    abort: bool,
    continue_merge_flag: bool,
    check: bool,
    dry_run: bool,
    options: MergeRunOptions,
    progress: &mut dyn ProgressSink,
) -> Result<MergeOutcome> {
    let repo = context::discover()?;
    let _interrupt = crate::interrupt::InterruptCancel::install();

    if abort {
        repo.merge().abort().map_err(anyhow::Error::new)?;
        return Ok(MergeOutcome::aborted());
    }

    if continue_merge_flag {
        if branch.is_some() {
            bail!("cannot merge a branch while finishing a merge with --continue");
        }
        let continue_message = read_message_input(&options.message)?;
        return run_continue(
            &repo,
            continue_message.as_deref(),
            progress,
            &crate::interrupt::process_cancel_token(),
        );
    }

    let branch = branch.context("branch name required unless using --abort or --continue")?;

    if let Some(op) = repo.operation().blocking_switch()? {
        if !matches!(op, grit_lib::state::InProgressOperation::Merge) {
            return Err(crate::json_error::operation_in_progress_for_repo(
                &repo, "merge", op,
            ));
        }
    }

    let head = repo.references().head()?;
    let source = repo
        .merge()
        .resolve_source(branch)
        .map_err(crate::json_error::map_lib_error)?;
    let other_oid = source.oid;
    let merge_options = build_merge_options(&options)?;

    match head {
        HeadState::Branch {
            short_name,
            oid: Some(head_oid),
            ..
        } => {
            if dry_run {
                return dry_run_merge(
                    &repo,
                    head_oid,
                    other_oid,
                    branch,
                    &short_name,
                    &merge_options,
                );
            }
            if check {
                return check_merge(
                    &repo,
                    head_oid,
                    other_oid,
                    branch,
                    &short_name,
                    &merge_options,
                );
            }
            integrate_with_options(
                &repo,
                &source,
                branch,
                &short_name,
                &merge_options,
                progress,
                &crate::interrupt::process_cancel_token(),
            )
        }
        HeadState::Branch { .. } => bail!("no commits yet on this branch"),
        HeadState::Detached { oid: head_oid } => {
            if dry_run {
                return Ok(dry_run_merge(
                    &repo,
                    head_oid,
                    other_oid,
                    branch,
                    "HEAD",
                    &merge_options,
                )?
                .for_detached_head());
            }
            if check {
                return Ok(check_merge(
                    &repo,
                    head_oid,
                    other_oid,
                    branch,
                    "HEAD",
                    &merge_options,
                )?
                .for_detached_head());
            }
            integrate_with_options(
                &repo,
                &source,
                branch,
                "HEAD",
                &merge_options,
                progress,
                &crate::interrupt::process_cancel_token(),
            )
        }
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    }
}

fn build_merge_options(run: &MergeRunOptions) -> Result<MergeOptions> {
    let fast_forward = if run.ff_only {
        FastForwardPolicy::Only
    } else if run.no_ff {
        FastForwardPolicy::Never
    } else {
        FastForwardPolicy::Allow
    };
    let commit = if run.squash {
        MergeCommitMode::Squash
    } else if run.no_commit {
        MergeCommitMode::NoCommit
    } else {
        MergeCommitMode::Commit
    };
    let (favor, conflict_style) = run.strategy.clone().into_merge_fields();
    Ok(MergeOptions {
        fast_forward,
        commit,
        message: read_message_input(&run.message)?,
        favor,
        conflict_style,
        allow_unrelated: run.allow_unrelated_histories,
        hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
        verify: if run.no_verify {
            HookPolicy::SkipVerify
        } else {
            HookPolicy::Run
        },
        directory_renames: None,
    })
}

fn merge_continue_label(repo: &Repository) -> String {
    repo.operation()
        .state()
        .ok()
        .and_then(|state| state.merge_message)
        .and_then(|msg| {
            let line = msg.lines().next()?.trim();
            if let Some(rest) = line.strip_prefix("Merge branch '") {
                rest.strip_suffix('\'').map(str::to_owned)
            } else if let Some(rest) = line.strip_prefix("Merge branch \"") {
                rest.strip_suffix('"').map(str::to_owned)
            } else {
                line.strip_prefix("Merge ")
                    .map(|rest| rest.trim_end_matches('\n').to_owned())
            }
        })
        .unwrap_or_else(|| "merge".to_owned())
}

fn run_continue(
    repo: &Repository,
    message: Option<&str>,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<MergeOutcome> {
    let label = merge_continue_label(repo);
    let detached = matches!(repo.references().head()?, HeadState::Detached { .. });
    let now = context::wall_clock_now(repo.environment());
    let options = ContinueOptions {
        message: message.map(str::to_owned),
        author: Some(context::signature(repo, IdentRole::Author, now)?),
        committer: Some(context::signature(repo, IdentRole::Committer, now)?),
    };
    match repo.merge().continue_merge(&options, progress, cancel) {
        Ok(outcome) => {
            let mut merged = MergeOutcome::merged(&label, outcome.oid, outcome.change_id);
            merged.rerere = crate::operation_output::rerere_events_json(&outcome.rerere);
            if detached {
                merged = merged.for_detached_head();
            }
            Ok(merged)
        }
        Err(err) => Err(anyhow::Error::new(err)),
    }
}

fn dry_run_merge(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    head_short: &str,
    options: &MergeOptions,
) -> Result<MergeOutcome> {
    match repo
        .merge()
        .plan_integration(into_oid, other_oid, label, options)?
    {
        MergeDryRunPlan::UpToDate => Ok(MergeOutcome::up_to_date_dry_run(label)),
        MergeDryRunPlan::FastForward { tip, change_id } => Ok(MergeOutcome::fast_forward_dry_run(
            label, head_short, tip, change_id,
        )),
        MergeDryRunPlan::RefusedNotFf => Ok(MergeOutcome::refused_not_ff_dry_run(label)),
        MergeDryRunPlan::MergeCommit { parents } => {
            Ok(MergeOutcome::merged_dry_run(label, parents))
        }
        MergeDryRunPlan::Conflicts { conflicts } => {
            Ok(MergeOutcome::conflicts_dry_run(label, conflicts))
        }
    }
}

fn check_merge(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    head_short: &str,
    merge_options: &MergeOptions,
) -> Result<MergeOutcome> {
    let options = MergePredictionOptions {
        favor: merge_options.favor,
        theirs_label: label.to_owned(),
        directory_renames: merge_options.directory_renames,
        ..MergePredictionOptions::default()
    };
    match repo.merge().predict(into_oid, other_oid, &options)? {
        MergePrediction::UpToDate => Ok(MergeOutcome::up_to_date_check(label)),
        MergePrediction::FastForward => Ok(MergeOutcome::fast_forward_check(
            label,
            head_short,
            other_oid,
            stored_change_id(repo, other_oid)?,
        )),
        MergePrediction::Clean { tree } => Ok(MergeOutcome::clean_check(label, tree)),
        MergePrediction::Conflicts { conflicts } => {
            Ok(MergeOutcome::conflicts_check(label, conflicts))
        }
        MergePrediction::Unrelated => {
            bail!("'{label}' has no common history with the current branch")
        }
    }
}

fn stored_change_id(repo: &Repository, oid: ObjectId) -> Result<Option<ChangeId>> {
    let obj = repo.odb().read(&oid)?;
    Ok(grit_lib::change_id::change_id(
        &grit_lib::objects::parse_commit(&obj.data)?,
    ))
}

fn integrate_with_options(
    repo: &Repository,
    source: &MergeSource,
    label: &str,
    head_short: &str,
    options: &MergeOptions,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<MergeOutcome> {
    let detached = matches!(repo.references().head()?, HeadState::Detached { .. });
    match repo
        .merge()
        .integrate(source, label, options, progress, cancel)
    {
        Ok(outcome) => {
            let mut mapped = map_integrate_outcome(outcome, label, head_short);
            if detached {
                mapped = mapped.for_detached_head();
            }
            Ok(mapped)
        }
        Err(Error::Merge(MergeError::NotFastForward)) => {
            let mut o = MergeOutcome::refused_not_ff(label);
            if detached {
                o = o.for_detached_head();
            }
            Ok(o)
        }
        Err(err @ Error::Hook(_)) => Err(json_error::hook_failed_with_hint(
            err,
            "the merge is still in progress: run `grit merge --continue` to commit it, or `grit merge --abort` to cancel",
        )),
        Err(err) => Err(err.into()),
    }
}

fn map_integrate_outcome(
    outcome: IntegrateBranchOutcome,
    label: &str,
    head_short: &str,
) -> MergeOutcome {
    match outcome {
        IntegrateBranchOutcome::UpToDate => MergeOutcome::up_to_date(label),
        IntegrateBranchOutcome::FastForward { oid, change_id } => {
            MergeOutcome::fast_forward(label, head_short, oid, change_id)
        }
        IntegrateBranchOutcome::Merged { oid, change_id } => {
            MergeOutcome::merged(label, oid, change_id)
        }
        IntegrateBranchOutcome::Squashed => MergeOutcome::squashed(label),
        IntegrateBranchOutcome::Stopped => MergeOutcome::stopped(label),
        IntegrateBranchOutcome::Conflicts {
            details, rerere, ..
        } => MergeOutcome::conflicts(label, details, rerere),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::objects::ObjectId;
    use grit_lib::operation::ConflictKind;
    use grit_test_support::CliFixture;

    #[test]
    fn merge_result_serializes_snake_case() {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
        let cases = [
            (MergeOutcome::up_to_date("topic"), "up_to_date"),
            (
                MergeOutcome::fast_forward("topic", "topic", oid, None),
                "fast_forward",
            ),
            (MergeOutcome::merged("topic", oid, None), "merged"),
            (MergeOutcome::squashed("topic"), "squashed"),
            (MergeOutcome::stopped("topic"), "stopped"),
            (MergeOutcome::refused_not_ff("topic"), "refused_not_ff"),
            (
                MergeOutcome::conflicts(
                    "topic",
                    vec![Conflict {
                        path: "a.txt".into(),
                        kind: ConflictKind::BothModified,
                    }],
                    Vec::new(),
                ),
                "conflicts",
            ),
            (MergeOutcome::aborted(), "aborted"),
        ];
        for (outcome, expected) in cases {
            let value = serde_json::to_value(&outcome).expect("json");
            assert_eq!(value["result"].as_str(), Some(expected), "{value}");
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_check_exit_code_and_json() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("a.txt"), "base\n").unwrap();
        fixture.run_ok(&["add", "a.txt"]);
        fixture.run_ok(&["commit", "-m", "base"]);
        fixture.run_ok(&["switch", "--create", "side"]);
        std::fs::write(fixture.path().join("a.txt"), "side\n").unwrap();
        fixture.run_ok(&["commit", "-m", "side"]);
        fixture.run_ok(&["switch", "main"]);
        std::fs::write(fixture.path().join("b.txt"), "main only\n").unwrap();
        fixture.run_ok(&["add", "b.txt"]);
        fixture.run_ok(&["commit", "-m", "main"]);

        let clean = fixture.run(&["--json", "merge", "--check", "side"]);
        assert_eq!(
            clean.status,
            Some(0),
            "{}",
            clean.dump("merge --check clean")
        );
        let v: serde_json::Value = serde_json::from_str(&clean.stdout).expect("json");
        assert_eq!(v["result"], "clean");
        assert_eq!(v["check"], true);
        assert!(v["oid"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(v["next_steps"].is_null() || v.get("next_steps").is_none());

        std::fs::write(fixture.path().join("a.txt"), "main edit\n").unwrap();
        fixture.run_ok(&["commit", "-m", "main conflict"]);
        let head_before = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();

        let conflict = fixture.run(&["--json", "merge", "--check", "side"]);
        assert_eq!(
            conflict.status,
            Some(1),
            "{}",
            conflict.dump("merge --check conflict")
        );
        let v: serde_json::Value = serde_json::from_str(&conflict.stdout).expect("json");
        assert_eq!(v["result"], "conflicts");
        assert_eq!(v["check"], true);
        assert!(v["conflicts"].as_array().is_some_and(|a| !a.is_empty()));
        let head_after = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        assert_eq!(
            head_before, head_after,
            "HEAD must be unchanged after --check"
        );
        let git_dir = fixture.path().join(".git");
        assert!(
            !git_dir.join("MERGE_HEAD").exists(),
            "MERGE_HEAD must not exist after --check"
        );
        assert!(
            !git_dir.join("ORIG_HEAD").exists(),
            "ORIG_HEAD must not exist after --check"
        );
        let a_txt = std::fs::read_to_string(fixture.path().join("a.txt")).expect("read a.txt");
        assert_eq!(
            a_txt, "main edit\n",
            "worktree must be unchanged after --check"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_check_honors_merge_favor() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("f"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();
        fixture.run_ok(&["add", "f"]);
        fixture.run_ok(&["commit", "-m", "base"]);
        fixture.run_ok(&["switch", "--create", "topic"]);
        std::fs::write(
            fixture.path().join("f"),
            "1\n2\n3\n4\ntopic\n6\n7\n8\n9\n10\n",
        )
        .unwrap();
        fixture.run_ok(&["commit", "-m", "topic"]);
        fixture.run_ok(&["switch", "main"]);
        std::fs::write(
            fixture.path().join("f"),
            "1\n2\n3\n4\nmain\n6\n7\n8\n9\n10\n",
        )
        .unwrap();
        fixture.run_ok(&["commit", "-m", "main"]);

        let head_before = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let main_oid = head_before.clone();
        let topic_oid = grit_test_support::git(fixture.path(), &["rev-parse", "topic"])
            .trim()
            .to_owned();

        let bare_conflict = fixture.run(&["--json", "merge", "--check", "topic"]);
        assert_eq!(
            bare_conflict.status,
            Some(1),
            "{}",
            bare_conflict.dump("merge --check conflict")
        );
        let v: serde_json::Value = serde_json::from_str(&bare_conflict.stdout).expect("json");
        assert_eq!(v["result"], "conflicts");

        for favor in ["ours", "theirs"] {
            let out = fixture.run(&["--json", "merge", "--check", "-X", favor, "topic"]);
            assert_eq!(
                out.status,
                Some(0),
                "{}",
                out.dump(&format!("merge --check -X {favor}"))
            );
            let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
            assert_eq!(v["result"], "clean");
            assert_eq!(v["check"], true);
            let grit_oid = v["oid"].as_str().expect("oid").to_owned();

            let git_tree = grit_test_support::git(
                fixture.path(),
                &[
                    "merge-tree",
                    "--write-tree",
                    "-X",
                    favor,
                    &main_oid,
                    &topic_oid,
                ],
            )
            .lines()
            .next()
            .expect("merge-tree tree oid")
            .trim()
            .to_owned();
            assert_eq!(
                grit_oid, git_tree,
                "-X {favor} tree must match git merge-tree"
            );

            let head_after = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
                .trim()
                .to_owned();
            assert_eq!(
                head_before, head_after,
                "HEAD unchanged after --check -X {favor}"
            );
        }
    }

    #[test]
    fn merge_check_human_output_no_real_merge_wording() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("f.txt"), "base\n").unwrap();
        fixture.run_ok(&["add", "f.txt"]);
        fixture.run_ok(&["commit", "-m", "base"]);
        fixture.run_ok(&["switch", "--create", "ahead"]);
        std::fs::write(fixture.path().join("g.txt"), "side\n").unwrap();
        fixture.run_ok(&["add", "g.txt"]);
        fixture.run_ok(&["commit", "-m", "side"]);
        fixture.run_ok(&["switch", "main"]);

        let ff = fixture.run(&["merge", "--check", "ahead"]);
        assert_eq!(ff.status, Some(0), "{}", ff.dump("ff check"));
        let ff_human = format!("{}{}", ff.stdout, ff.stderr);
        assert!(ff_human.contains("Would fast-forward"));
        for forbidden in [
            "Fast-forwarded",
            "Merged ",
            "merge in progress",
            "Automatic merge failed",
            "grit merge --abort",
            "grit commit",
        ] {
            assert!(
                !ff_human.contains(forbidden),
                "unexpected wording {forbidden:?} in {ff_human:?}"
            );
        }

        let conflict_fixture = CliFixture::empty();
        std::fs::write(conflict_fixture.path().join("f.txt"), "base\n").unwrap();
        conflict_fixture.run_ok(&["add", "f.txt"]);
        conflict_fixture.run_ok(&["commit", "-m", "base"]);
        conflict_fixture.run_ok(&["switch", "--create", "side"]);
        std::fs::write(conflict_fixture.path().join("f.txt"), "side\n").unwrap();
        conflict_fixture.run_ok(&["commit", "-m", "side"]);
        conflict_fixture.run_ok(&["switch", "main"]);
        std::fs::write(conflict_fixture.path().join("f.txt"), "main\n").unwrap();
        conflict_fixture.run_ok(&["commit", "-m", "main"]);
        let conflict = conflict_fixture.run(&["merge", "--check", "side"]);
        assert_eq!(
            conflict.status,
            Some(1),
            "{}",
            conflict.dump("conflict check")
        );
        let conflict_human = format!("{}{}", conflict.stdout, conflict.stderr);
        assert!(conflict_human.contains("Would conflict merging side"));
        assert!(conflict_human.contains("f.txt"));
        for forbidden in [
            "Fast-forwarded",
            "Merged ",
            "merge in progress",
            "Automatic merge failed",
            "grit merge --abort",
            "→",
        ] {
            assert!(
                !conflict_human.contains(forbidden),
                "unexpected wording {forbidden:?} in {conflict_human:?}"
            );
        }
    }

    #[test]
    fn merge_conflict_json_shape() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("f.txt"), "base\n").unwrap();
        fixture.run_ok(&["add", "f.txt"]);
        fixture.run_ok(&["commit", "-m", "base"]);
        fixture.run_ok(&["switch", "--create", "side"]);
        std::fs::write(fixture.path().join("f.txt"), "side\n").unwrap();
        fixture.run_ok(&["commit", "-m", "side"]);
        fixture.run_ok(&["switch", "main"]);
        std::fs::write(fixture.path().join("f.txt"), "main\n").unwrap();
        fixture.run_ok(&["commit", "-m", "main"]);

        let out = fixture.run(&["--json", "merge", "side"]);
        assert_eq!(out.status, Some(1), "{}", out.dump("merge"));
        let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
        assert_eq!(v["result"], "conflicts");
        let conflicts = v["conflicts"].as_array().expect("conflicts array");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0]["path"], "f.txt");
        assert_eq!(conflicts[0]["kind"], "both_modified");
        let next = v["next_steps"].as_array().expect("next_steps");
        assert!(
            next.iter().any(|s| s.as_str() == Some("grit diff")),
            "{next:?}"
        );
        let alts = v["alternatives"].as_array().expect("alternatives");
        assert!(
            alts.iter()
                .any(|s| s.as_str() == Some("grit merge --abort")),
            "{alts:?}"
        );
    }

    #[test]
    fn merge_on_detached_head_fast_forward_issue_2314() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("f"), "a\n").unwrap();
        fixture.run_ok(&["add", "f"]);
        fixture.run_ok(&["commit", "-m", "a"]);
        fixture.run_ok(&["switch", "--create", "topic"]);
        std::fs::write(fixture.path().join("g"), "b\n").unwrap();
        fixture.run_ok(&["add", "g"]);
        fixture.run_ok(&["commit", "-m", "b"]);
        fixture.run_ok(&["switch", "main"]);
        fixture.run_ok(&["switch", "--detach", "HEAD"]);
        let head_before = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let main_before = grit_test_support::git(fixture.path(), &["rev-parse", "main"])
            .trim()
            .to_owned();

        let out = fixture.run(&["--json", "merge", "topic"]);
        assert_eq!(out.status, Some(0), "{}", out.dump("merge detached"));
        let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
        assert_eq!(v["result"], "fast_forward");
        assert!(v.get("updated_branch").is_none());
        let head_after = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        assert_ne!(head_before, head_after);
        assert_eq!(
            grit_test_support::git(fixture.path(), &["rev-parse", "main"]).trim(),
            main_before
        );

        fixture.run_ok(&["switch", "main"]);
        fixture.run_ok(&["switch", "--detach", "HEAD"]);
        let head = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let check = fixture.run(&["--json", "merge", "--check", "topic"]);
        assert_eq!(check.status, Some(0), "{}", check.dump("check"));
        assert_eq!(
            grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"]).trim(),
            head
        );
        assert!(!fixture.path().join(".git/MERGE_HEAD").exists());

        fixture.run_ok(&["switch", "main"]);
        fixture.run_ok(&["switch", "--detach", "HEAD"]);
        let head = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let dry = fixture.run(&["--json", "merge", "--dry-run", "topic"]);
        assert_eq!(dry.status, Some(0), "{}", dry.dump("dry-run"));
        assert_eq!(
            grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"]).trim(),
            head
        );
        assert!(!fixture.path().join(".git/MERGE_HEAD").exists());
    }

    #[test]
    fn merge_continue_on_detached_head_human_says_head() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("f.txt"), "base\n").unwrap();
        fixture.run_ok(&["add", "f.txt"]);
        fixture.run_ok(&["commit", "-m", "base"]);
        fixture.run_ok(&["switch", "--create", "topic"]);
        std::fs::write(fixture.path().join("f.txt"), "topic\n").unwrap();
        fixture.run_ok(&["commit", "-m", "topic"]);
        fixture.run_ok(&["switch", "main"]);
        std::fs::write(fixture.path().join("f.txt"), "main\n").unwrap();
        fixture.run_ok(&["commit", "-m", "main"]);
        let main_before = grit_test_support::git(fixture.path(), &["rev-parse", "main"])
            .trim()
            .to_owned();
        fixture.run_ok(&["switch", "--detach", "HEAD"]);

        let conflict = fixture.run(&["merge", "topic"]);
        assert_eq!(
            conflict.status,
            Some(1),
            "{}",
            conflict.dump("merge conflict")
        );
        std::fs::write(fixture.path().join("f.txt"), "resolved\n").unwrap();
        fixture.run_ok(&["add", "f.txt"]);

        let cont = fixture.run(&["merge", "--continue"]);
        assert_eq!(cont.status, Some(0), "{}", cont.dump("continue"));
        let human = format!("{}{}", cont.stdout, cont.stderr);
        assert!(
            human.contains("Merged topic into HEAD"),
            "expected HEAD wording, got: {human:?}"
        );
        assert!(
            !human.contains("current branch"),
            "must not claim a branch moved: {human:?}"
        );
        assert!(
            grit_test_support::git(fixture.path(), &["branch", "--show-current"])
                .trim()
                .is_empty(),
            "HEAD should stay detached"
        );
        assert_eq!(
            grit_test_support::git(fixture.path(), &["rev-parse", "main"]).trim(),
            main_before
        );

        let md_fixture = CliFixture::empty();
        std::fs::write(md_fixture.path().join("f.txt"), "base\n").unwrap();
        md_fixture.run_ok(&["add", "f.txt"]);
        md_fixture.run_ok(&["commit", "-m", "base"]);
        md_fixture.run_ok(&["switch", "--create", "topic"]);
        std::fs::write(md_fixture.path().join("f.txt"), "topic\n").unwrap();
        md_fixture.run_ok(&["commit", "-m", "topic"]);
        md_fixture.run_ok(&["switch", "main"]);
        std::fs::write(md_fixture.path().join("f.txt"), "main\n").unwrap();
        md_fixture.run_ok(&["commit", "-m", "main"]);
        md_fixture.run_ok(&["switch", "--detach", "HEAD"]);
        md_fixture.run(&["merge", "topic"]);
        std::fs::write(md_fixture.path().join("f.txt"), "resolved\n").unwrap();
        md_fixture.run_ok(&["add", "f.txt"]);
        let md = md_fixture.run(&["--markdown", "merge", "--continue"]);
        assert_eq!(md.status, Some(0), "{}", md.dump("markdown continue"));
        let md_out = format!("{}{}", md.stdout, md.stderr);
        assert!(
            md_out.contains("into HEAD"),
            "markdown should say HEAD: {md_out:?}"
        );
        assert!(!md_out.contains("current branch"));
    }
}

/// Dispatch parsed CLI args for `Merge`.
pub fn dispatch(
    args: crate::args::MergeArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};
    use crate::exit_status::ExitStatus;

    use crate::progress;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let MergeArgs {
        branch,
        abort,
        continue_merge,
        check,
        dry_run,
        ff_only,
        no_ff,
        no_commit,
        squash,
        allow_unrelated_histories,
        no_verify,
        strategy,
        message,
    } = args;
    {
        let outcome = commands::merge::run(
            branch.as_deref(),
            abort,
            continue_merge,
            check,
            dry_run,
            commands::merge::MergeRunOptions {
                ff_only,
                no_ff,
                no_commit,
                squash,
                allow_unrelated_histories,
                no_verify,
                strategy,
                message,
            },
            &mut progress::TerminalProgress::new(opts.mode.is_human()),
        )?;
        let status = ExitStatus::from_merge_outcome(&outcome);
        emit_success(outcome, opts, ctx)?;
        Ok(status)
    }
}
