//! `grit history` — stateless history rewrite helpers.

use std::io::{self, Read, Write};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use grit_lib::command_runner::system_command_runner;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error as LibError;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::hunk_select::{parse_hunk_id_list, HunkSelectError, HunkSelection};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    abort_rebase, absorb, apply_absorb_rewrite, apply_fixup_hunk_selection, apply_fixup_rewrite,
    apply_rewrite, continue_rebase, drop as history_drop, fixup, format_hunk_range_header,
    move_commit, plan_rebase, plan_rebase_check, reword_with, skip_rebase, split, squash,
    staged_fixup_paths, start_rebase, undo_last_rewrite, verify_rebase_with_exec,
    verify_rewrite_plan, AbsorbOptions, ExecRunRecord, ExecSpec,
    ApplyOptions, EmptyPolicy, FixupOptions, HistoryError, HunkAssignment, HunkRange, Placement,
    RebaseCheckPlan, RebaseOntoOptions, RebaseOutcome, RebasePlan, RebaseRequest,
    RebaseSessionError, RefScope, RewordOptions, RewriteOptions, RewritePlan, SplitOptions,
    SquashMessage, SquashOptions, StartRebaseOptions, UnassignedHunk,
};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use grit_lib::revwalk::rev_parse_error::RevParseError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::merge_strategy::MergeStrategyArg;
use crate::context;
use crate::json_error;
use crate::markdown;
use crate::operation_output::{
    render_conflicts_human, render_rebase_conflict_footer_human, ConflictJson,
    OperationConflictBlock,
};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::pathspec_args::TrailingPathspecArgs;

/// Which local refs a history rewrite may move.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum UpdateRefs {
    /// Every local branch descending from the edited commit, plus `HEAD` when applicable.
    #[default]
    Branches,
    /// Only the commit `HEAD` currently points at.
    Head,
}

impl From<UpdateRefs> for RefScope {
    fn from(value: UpdateRefs) -> Self {
        match value {
            UpdateRefs::Branches => RefScope::Branches,
            UpdateRefs::Head => RefScope::Head,
        }
    }
}

/// How empty commits are handled during history rewrite.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum EmptyPolicyArg {
    /// Omit commits whose tree matches the new parent.
    #[default]
    Drop,
    /// Keep empty commits.
    Keep,
    /// Refuse when a commit would become empty.
    Abort,
}

impl From<EmptyPolicyArg> for EmptyPolicy {
    fn from(value: EmptyPolicyArg) -> Self {
        match value {
            EmptyPolicyArg::Drop => EmptyPolicy::Drop,
            EmptyPolicyArg::Keep => EmptyPolicy::Keep,
            EmptyPolicyArg::Abort => EmptyPolicy::Abort,
        }
    }
}

/// Extra flags for `grit rebase` / `grit history rebase`.
#[derive(Debug, clap::Args)]
pub struct RebaseRunFlags {
    /// Predict whether the rebase would succeed without changing refs, index, or worktree.
    #[arg(long = "check", conflicts_with_all = ["dry_run", "exec"])]
    pub check: bool,
    /// Auto-resolve replay conflicts: `ours` keeps the onto side, `theirs` keeps replayed commits.
    #[arg(short = 'X', value_enum, value_name = "OURS|THEIRS")]
    pub strategy: Option<MergeStrategyArg>,
    /// Run a shell command after each replayed commit is checked out (repeatable; incompatible with `--dry-run` and `--check`).
    #[arg(long = "exec", value_name = "CMD", conflicts_with = "dry_run")]
    pub exec: Vec<String>,
}

/// Result of `grit rebase --check`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RebaseCheckOutcome {
    pub operation: String,
    pub result: RebaseCheckResult,
    pub check: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Outcome category for [`RebaseCheckOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RebaseCheckResult {
    UpToDate,
    Clean,
    Conflicts,
}

impl RebaseCheckOutcome {
    fn up_to_date() -> Self {
        Self {
            operation: "rebase".into(),
            result: RebaseCheckResult::UpToDate,
            check: true,
            commits: None,
            conflicts: None,
            commit: None,
        }
    }

    fn clean(commits: Vec<ObjectId>) -> Self {
        Self {
            operation: "rebase".into(),
            result: RebaseCheckResult::Clean,
            check: true,
            commits: Some(commits.iter().map(ObjectId::to_hex).collect()),
            conflicts: None,
            commit: None,
        }
    }

    fn conflicts(commit: ObjectId, paths: Vec<String>) -> Self {
        let conflicts = Some(
            paths
                .iter()
                .map(|path| ConflictJson {
                    path: path.clone(),
                    kind: "content".into(),
                    summary: None,
                })
                .collect(),
        );
        Self {
            operation: "rebase".into(),
            result: RebaseCheckResult::Conflicts,
            check: true,
            commits: None,
            conflicts,
            commit: Some(commit.to_hex()),
        }
    }
}

impl HumanRender for RebaseCheckOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.result {
            RebaseCheckResult::UpToDate => outln!(w, "Already up to date"),
            RebaseCheckResult::Clean => {
                let n = self.commits.as_ref().map_or(0, Vec::len);
                outln!(w, "Would rebase {n} commit(s) cleanly");
            }
            RebaseCheckResult::Conflicts => {
                outln!(w, "Would conflict during rebase");
                if let Some(conflicts) = &self.conflicts {
                    render_conflicts_human(conflicts, w)?;
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RebaseCheckOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_rebase_check_outcome(w, self)
    }
}

/// Shared flags for history rewrite subcommands.
#[derive(Debug, clap::Args)]
pub struct HistoryRewriteArgs {
    /// Print planned ref updates without moving refs (new commit objects are still written).
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Which refs to rewrite: all descendant branches (default) or only `HEAD`.
    #[arg(long = "update-refs", value_enum, default_value_t = UpdateRefs::Branches)]
    pub update_refs: UpdateRefs,
    /// When a replayed commit would not change the tree: drop (default), keep an empty commit, or abort.
    #[arg(long = "empty", value_enum, default_value_t = EmptyPolicyArg::Drop)]
    pub empty: EmptyPolicyArg,
    /// Keep each replayed commit's original committer line instead of the configured committer.
    #[arg(long = "keep-committer")]
    pub keep_committer: bool,
}

/// Result of `grit history drop`, `move`, `reword`, `fixup`, `squash`, `absorb`, `rebase`, and `undo`, and top-level `grit rebase`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HistoryRewriteOutcome {
    /// Operation name (`drop`, `move`, `reword`, `fixup`, `squash`, `rebase`, or `undo`).
    pub operation: String,
    /// Original and replacement commit OIDs for every rewritten commit.
    pub rewritten: Vec<RewrittenPair>,
    /// Planned or applied ref tip updates.
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes; JSON `null` when unchanged.
    pub head: Option<HeadPair>,
    /// Whether refs were left unchanged (`true` for `--dry-run`).
    pub dry_run: bool,
    /// `--exec` subprocess results when `--exec` was used (success only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<ExecResultJson>,
}

/// One `rebase --exec` command result in `--json` output.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExecResultJson {
    /// Commit OID (full hex) checked out when the command ran.
    pub commit: String,
    /// Shell command string.
    pub command: String,
    /// Subprocess exit code.
    pub exit_code: i32,
}

impl HistoryRewriteOutcome {
    /// Outcome when there is nothing to replay (`rewritten` is empty).
    pub fn up_to_date(operation: &str, dry_run: bool) -> Self {
        Self {
            operation: operation.into(),
            rewritten: Vec::new(),
            refs: Vec::new(),
            head: None,
            dry_run,
            exec: Vec::new(),
        }
    }

    pub(crate) fn new(operation: &str, plan: &RewritePlan, dry_run: bool) -> Self {
        Self {
            operation: operation.into(),
            rewritten: plan
                .commits
                .iter()
                .map(|row| RewrittenPair {
                    old: row.old.to_hex(),
                    new: row.new.map(|new| new.to_hex()),
                    change_id: row.change_id.as_ref().map(|id| id.as_str().to_owned()),
                    subject: row.subject.clone(),
                })
                .collect(),
            refs: plan
                .ref_updates
                .iter()
                .map(|u| RefPair {
                    ref_name: u.name.clone(),
                    name: u.name.clone(),
                    old: u.old.to_hex(),
                    new: u.new.to_hex(),
                })
                .collect(),
            head: plan.head_change().map(|(old, new)| HeadPair {
                old: old.to_hex(),
                new: new.to_hex(),
            }),
            dry_run,
            exec: Vec::new(),
        }
    }

    fn with_exec(mut self, exec: Vec<ExecResultJson>) -> Self {
        self.exec = exec;
        self
    }
}

/// One commit OID before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RewrittenPair {
    /// Commit OID before rewrite (full hex).
    pub old: String,
    /// Replacement commit OID (full hex), or JSON `null` when the commit was dropped.
    pub new: Option<String>,
    /// Stored `change-id` on the replacement commit, or JSON `null` when dropped or absent.
    pub change_id: Option<String>,
    /// First line of the replacement commit message, or the original when dropped.
    pub subject: Option<String>,
}

/// One ref tip before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefPair {
    /// Full ref name.
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Deprecated duplicate of [`Self::ref_name`]; removed in a future release.
    pub name: String,
    /// Previous tip OID (full hex).
    pub old: String,
    /// New tip OID (full hex).
    pub new: String,
}

/// `HEAD` commit before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HeadPair {
    /// Commit OID at `HEAD` before rewrite (full hex).
    pub old: String,
    /// Commit OID at `HEAD` after rewrite (full hex).
    pub new: String,
}

/// High-level result of `grit rebase`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RebaseResult {
    Completed,
    Conflicts,
    Aborted,
}

/// Result of `grit rebase` and `grit history rebase`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RebaseCliOutcome {
    pub result: RebaseResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewritten: Vec<RewrittenPair>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes.
    #[serde(default)]
    pub head: Option<HeadPair>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// `--exec` subprocess results when `--exec` was used (success only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<ExecResultJson>,
}

impl RebaseCliOutcome {
    fn aborted() -> Self {
        Self {
            result: RebaseResult::Aborted,
            operation: None,
            rewritten: Vec::new(),
            refs: Vec::new(),
            head: None,
            dry_run: false,
            commit: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            exec: Vec::new(),
        }
    }

    fn from_rewrite(outcome: HistoryRewriteOutcome) -> Self {
        Self {
            result: RebaseResult::Completed,
            operation: Some(outcome.operation),
            rewritten: outcome.rewritten,
            refs: outcome.refs,
            head: outcome.head,
            dry_run: outcome.dry_run,
            exec: outcome.exec,
            commit: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
        }
    }

    fn from_stop(commit: ObjectId, conflicts: &[grit_lib::operation::Conflict]) -> Self {
        let block = OperationConflictBlock::for_rebase(conflicts);
        Self {
            result: RebaseResult::Conflicts,
            operation: Some("rebase".into()),
            rewritten: Vec::new(),
            refs: Vec::new(),
            head: None,
            dry_run: false,
            commit: Some(commit.to_hex()),
            conflicts: Some(block.conflicts),
            next_steps: Some(block.next_steps),
            alternatives: block.alternatives,
            exec: Vec::new(),
        }
    }
}

impl HumanRender for RebaseCliOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self.result {
            RebaseResult::Aborted => outln!(w, "Rebase aborted."),
            RebaseResult::Conflicts => {
                if let Some(details) = &self.conflicts {
                    render_conflicts_human(details, w)?;
                }
                render_rebase_conflict_footer_human();
            }
            RebaseResult::Completed => {
                HistoryRewriteOutcome {
                    operation: self.operation.clone().unwrap_or_else(|| "rebase".into()),
                    rewritten: self.rewritten.clone(),
                    refs: self.refs.clone(),
                    head: self.head.clone(),
                    dry_run: self.dry_run,
                    exec: self.exec.clone(),
                }
                .render_human(w, ctx)?;
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RebaseCliOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_rebase_outcome(w, self)?;
        Ok(())
    }
}

/// Either a resumable rebase outcome or a `--check` prediction.
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
pub enum RebaseRunOutcome {
    Rebase(RebaseCliOutcome),
    Check(RebaseCheckOutcome),
}

impl HumanRender for RebaseRunOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            Self::Rebase(o) => o.render_human(w, ctx),
            Self::Check(o) => o.render_human(w, ctx),
        }
    }
}

impl MarkdownRender for RebaseRunOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            Self::Rebase(o) => o.render_markdown(w, ctx),
            Self::Check(o) => o.render_markdown(w, ctx),
        }
    }
}

/// Options for [`run_rebase`].
#[derive(Debug)]
pub struct RebaseRunOptions<'a> {
    pub upstream: Option<String>,
    pub onto: Option<String>,
    pub continue_rebase: bool,
    pub abort: bool,
    pub skip: bool,
    pub flags: &'a RebaseRunFlags,
    pub shared: &'a HistoryRewriteArgs,
}

fn short(hex: &str) -> &str {
    hex.get(..7).unwrap_or(hex)
}

fn render_rewrite_rows(
    w: &mut dyn Write,
    rewritten: &[RewrittenPair],
    refs: &[RefPair],
) -> Result<()> {
    for pair in rewritten {
        match &pair.new {
            Some(new) => {
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(
                        w,
                        "  commit {} → {} ({subject})",
                        short(&pair.old),
                        short(new)
                    );
                } else {
                    outln!(w, "  commit {} → {}", short(&pair.old), short(new));
                }
            }
            None => {
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(w, "  commit {} → (dropped) ({subject})", short(&pair.old));
                } else {
                    outln!(w, "  commit {} → (dropped)", short(&pair.old));
                }
            }
        }
    }
    for update in refs {
        outln!(
            w,
            "  {} {} → {}",
            update.ref_name,
            short(&update.old),
            short(&update.new)
        );
    }
    Ok(())
}

impl HumanRender for HistoryRewriteOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.operation == "rebase" && self.rewritten.is_empty() {
            outln!(w, "Already up to date");
            return Ok(());
        }
        if self.dry_run {
            if self.operation == "undo" {
                outln!(w, "Dry run — undo would restore refs:");
            } else {
                outln!(w, "Dry run — refs would change:");
            }
        } else if self.operation == "undo" {
            outln!(w, "Undid last history rewrite");
        } else if self.operation == "drop" {
            outln!(
                w,
                "Dropped 1 commit, replayed {} descendant commit(s)",
                self.rewritten.len()
            );
        } else if self.operation == "rebase" {
            outln!(w, "Rebased {} commit(s)", self.rewritten.len());
        } else {
            let verb = match self.operation.as_str() {
                "fixup" => "Fixed up",
                "squash" => "Squashed",
                "split" => "Split",
                _ => "Rewrote",
            };
            outln!(w, "{verb} {} commit(s)", self.rewritten.len());
        }
        render_rewrite_rows(w, &self.rewritten, &self.refs)
    }
}

impl MarkdownRender for HistoryRewriteOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let title = match self.operation.as_str() {
            "drop" => "History drop",
            "move" => "History move",
            "fixup" => "History fixup",
            "squash" => "History squash",
            "reword" => "History reword",
            "rebase" => "History rebase",
            "split" => "History split",
            "undo" => "History undo",
            _ => "History rewrite",
        };
        markdown::print_history_rewrite_outcome(w, title, self)
    }
}

/// Repository, config and rewrite options shared by every history subcommand.
struct Rewrite {
    repo: Repository,
    config: ConfigSet,
    options: RewriteOptions,
}

impl Rewrite {
    fn new(repo: Repository, shared: &HistoryRewriteArgs) -> Result<Self> {
        let env = repo.environment();
        let config =
            ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
        let committer = context::identity(
            env,
            &config,
            IdentRole::Committer,
            context::wall_clock_now(env),
        )?;
        Ok(Self {
            repo,
            config,
            options: RewriteOptions {
                scope: shared.update_refs.into(),
                empty: shared.empty.into(),
                committer,
                preserve_committer: shared.keep_committer,
                sign: !shared.dry_run,
            },
        })
    }

    fn apply_options(&self, operation: &str, reflog_message: String) -> Result<ApplyOptions> {
        Ok(ApplyOptions {
            identity: context::committer_reflog_identity(self.repo.environment(), &self.config)?,
            reflog_message,
            record_operation: Some(operation.into()),
        })
    }

    fn subject(&self, oid: ObjectId) -> Result<String> {
        let commit = context::read_commit(&self.repo, &oid)?;
        Ok(context::subject_line(&commit.message))
    }
}

fn resolve_commit(repo: &Repository, rev: &str, what: &str) -> Result<ObjectId> {
    resolve_revision_as_commit(repo, rev)
        .with_context(|| format!("could not resolve {what} '{rev}'"))
}

/// Reorder `commit` relative to `--before` or `--after` on the first-parent chain of `HEAD`.
pub fn run_move(
    commit: &str,
    before: Option<String>,
    after: Option<String>,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let (anchor, placement): (String, fn(ObjectId) -> Placement) = match (before, after) {
        (Some(rev), _) => (rev, Placement::Before),
        (None, Some(rev)) => (rev, Placement::After),
        (None, None) => bail!("--before or --after is required"),
    };
    let on_err = history_error("history move");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let anchor_oid = resolve_commit(&repo, &anchor, "anchor commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = move_commit(
        &rewrite.repo,
        commit_oid,
        placement(anchor_oid),
        &rewrite.options,
    )
    .map_err(on_err)?;
    if !shared.dry_run {
        let message = format!("history move: {}", short(&commit_oid.to_hex()));
        apply_rewrite(
            &rewrite.repo,
            &plan,
            &rewrite.apply_options("move", message)?,
        )
        .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("move", &plan, shared.dry_run))
}

/// Remove `commit` from history and replay descendants onto its parent.
pub fn run_drop(commit: &str, shared: &HistoryRewriteArgs) -> Result<HistoryRewriteOutcome> {
    let on_err = history_error("history drop");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = history_drop(&rewrite.repo, commit_oid, &rewrite.options).map_err(on_err)?;
    verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
    if !shared.dry_run {
        let message = format!("history drop: {}", rewrite.subject(commit_oid)?);
        apply_rewrite(
            &rewrite.repo,
            &plan,
            &rewrite.apply_options("drop", message)?,
        )
        .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("drop", &plan, shared.dry_run))
}

/// Rewrite `commit`'s message and/or author and replay descendants.
pub fn run_reword(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    author: Option<String>,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let message = read_message(message, message_file)?;
    let author = author.filter(|a| !a.trim().is_empty());
    let has_author = author.is_some();
    if message.is_none() && !has_author {
        bail!("reword requires a new message (-m/--message or --message-file) or --author");
    }
    let on_err = history_error("history reword");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = reword_with(
        &rewrite.repo,
        commit_oid,
        &RewordOptions { message, author },
        &rewrite.options,
    )
    .map_err(on_err)?;
    verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
    let new_oid = plan
        .replacement(commit_oid)
        .ok_or_else(|| anyhow::anyhow!("rewritten commit missing from plan"))?;
    if !shared.dry_run {
        let message = format!("history reword: {}", rewrite.subject(new_oid)?);
        apply_rewrite(
            &rewrite.repo,
            &plan,
            &rewrite.apply_options("reword", message)?,
        )
        .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("reword", &plan, shared.dry_run))
}

/// Apply staged changes onto `commit` and replay descendants.
pub fn run_fixup(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    hunks: &[String],
    lines: &[String],
    pathspecs: &TrailingPathspecArgs,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let message = read_message(message, message_file)?;
    let on_err = history_error("history fixup");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let resolved = pathspecs.resolve(&repo)?;
    let (pathspec_list, _) = resolved.into_pathspec_lists();
    let hunk_selection = hunk_selection_from_args(hunks, lines)?;
    let rewrite = Rewrite::new(repo, shared)?;
    let empty = rewrite.options.empty;
    let plan = fixup(
        &rewrite.repo,
        commit_oid,
        &FixupOptions {
            message,
            empty,
            rewrite: rewrite.options.clone(),
            selection: hunk_selection.clone(),
            pathspecs: pathspec_list.clone(),
        },
    )
    .map_err(map_fixup_error)?;
    if !shared.dry_run {
        let subject = match plan.replacement(commit_oid) {
            Some(new_oid) => rewrite.subject(new_oid)?,
            None => commit.to_owned(),
        };
        let apply_opts = rewrite.apply_options("fixup", format!("history fixup: {subject}"))?;
        if let Some(selection) = hunk_selection.as_ref() {
            apply_fixup_hunk_selection(
                &rewrite.repo,
                &plan,
                &apply_opts,
                selection,
                &pathspec_list,
            )
            .map_err(map_fixup_error)?;
        } else {
            let staged_paths = staged_fixup_paths(&rewrite.repo).map_err(on_err)?;
            apply_fixup_rewrite(&rewrite.repo, &plan, &apply_opts, &staged_paths)
                .map_err(on_err)?;
        }
    }
    Ok(HistoryRewriteOutcome::new("fixup", &plan, shared.dry_run))
}

fn hunk_selection_from_args(hunks: &[String], lines: &[String]) -> Result<Option<HunkSelection>> {
    if hunks.is_empty() && lines.is_empty() {
        return Ok(None);
    }
    let mut selection = parse_hunk_id_list(hunks).map_err(map_hunk_select_error)?;
    selection.ranges = crate::commands::add::parse_line_range_list(lines)?;
    Ok(Some(selection))
}

fn map_hunk_select_error(err: HunkSelectError) -> anyhow::Error {
    anyhow::Error::new(err)
}

fn map_fixup_error(err: HistoryError) -> anyhow::Error {
    match err {
        HistoryError::HunkSelect(h) => anyhow::Error::new(h),
        other => history_error("history fixup")(other),
    }
}

/// Squash a revision range into the oldest commit and replay descendants.
pub fn run_squash(
    range: &[String],
    message: Option<String>,
    message_file: Option<String>,
    combine: bool,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let message = if combine {
        SquashMessage::Combine
    } else {
        read_message(message, message_file)?.map_or(SquashMessage::Oldest, SquashMessage::Custom)
    };
    let on_err = history_error("history squash");
    let range_refs: Vec<&str> = range.iter().map(String::as_str).collect();
    let rewrite = Rewrite::new(context::discover()?, shared)?;
    let plan = squash(
        &rewrite.repo,
        &range_refs,
        &SquashOptions {
            message,
            rewrite: rewrite.options.clone(),
        },
    )
    .map_err(on_err)?;
    verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
    if !shared.dry_run {
        let subject = match plan.commits.first() {
            Some(first) => rewrite.subject(first.new.unwrap_or(first.old))?,
            None => "squash".into(),
        };
        let message = format!("history squash: {subject}");
        apply_rewrite(
            &rewrite.repo,
            &plan,
            &rewrite.apply_options("squash", message)?,
        )
        .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("squash", &plan, shared.dry_run))
}

/// Split path-limited changes out of `commit` into a new parent commit and replay descendants.
pub fn run_split(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    rest_message: Option<String>,
    paths: &TrailingPathspecArgs,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let Some(split_message) = read_message(message, message_file)? else {
        bail!("a message is required for the new commit (-m/--message or --message-file)");
    };
    let on_err = history_error("history split");
    let repo = context::discover()?;
    paths.require_non_empty()?;
    let resolved = paths.resolve(&repo)?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = split(
        &rewrite.repo,
        commit_oid,
        &resolved.specs,
        split_message,
        &SplitOptions { rest_message },
        &rewrite.options,
    )
    .map_err(on_err)?;
    verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
    if !shared.dry_run {
        let subject = rewrite.subject(commit_oid)?;
        apply_rewrite(
            &rewrite.repo,
            &plan,
            &rewrite.apply_options("split", format!("history split: {subject}"))?,
        )
        .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("split", &plan, shared.dry_run))
}

/// JSON/markdown assignment row for `grit history absorb`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AbsorbAssignmentOut {
    /// Repository-relative path.
    pub path: String,
    /// Staged hunk range in the index↔`HEAD` diff.
    pub hunk_range: HunkRangeOut,
    /// Target commit (full hex).
    pub commit: String,
    /// Target commit subject (first line, folded).
    pub subject: String,
}

/// Hunk coordinates for JSON output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HunkRangeOut {
    /// Old side start line (1-based).
    pub old_start: usize,
    /// Old side line count.
    pub old_lines: usize,
    /// New side start line (1-based).
    pub new_start: usize,
    /// New side line count.
    pub new_lines: usize,
}

impl From<&HunkRange> for HunkRangeOut {
    fn from(range: &HunkRange) -> Self {
        Self {
            old_start: range.old_start,
            old_lines: range.old_lines,
            new_start: range.new_start,
            new_lines: range.new_lines,
        }
    }
}

impl From<&HunkAssignment> for AbsorbAssignmentOut {
    fn from(row: &HunkAssignment) -> Self {
        Self {
            path: row.path.clone(),
            hunk_range: (&row.hunk_range).into(),
            commit: row.commit.to_hex(),
            subject: row.subject.clone(),
        }
    }
}

impl From<&UnassignedHunk> for UnassignedHunkOut {
    fn from(row: &UnassignedHunk) -> Self {
        Self {
            path: row.path.clone(),
            hunk_range: (&row.hunk_range).into(),
            reason: row.reason.as_str().into(),
        }
    }
}

/// JSON/markdown unassigned hunk row.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UnassignedHunkOut {
    /// Repository-relative path.
    pub path: String,
    /// Staged hunk range.
    pub hunk_range: HunkRangeOut,
    /// Stable reason code.
    pub reason: String,
}

/// Result of `grit history absorb`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HistoryAbsorbOutcome {
    /// Operation name (`absorb`).
    pub operation: String,
    /// Assigned hunks.
    pub assignments: Vec<AbsorbAssignmentOut>,
    /// Hunks left staged.
    pub unassigned: Vec<UnassignedHunkOut>,
    /// Original and replacement commit OIDs for every rewritten commit.
    pub rewritten: Vec<RewrittenPair>,
    /// Planned or applied ref tip updates.
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes; JSON `null` when unchanged.
    pub head: Option<HeadPair>,
    /// Whether refs were left unchanged (`true` for `--dry-run`).
    pub dry_run: bool,
}

impl HumanRender for HistoryAbsorbOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Dry run — absorb would change {} commit(s)",
                self.rewritten.len()
            );
        } else {
            outln!(
                w,
                "Absorbed {} hunk(s) into {} commit(s)",
                self.assignments.len(),
                self.rewritten.len()
            );
        }
        for row in &self.assignments {
            let header = format_absorb_hunk_header(&row.hunk_range);
            outln!(
                w,
                "  {} {} → {} {}",
                row.path,
                header,
                short(&row.commit),
                row.subject
            );
        }
        for row in &self.unassigned {
            let header = format_absorb_hunk_header(&row.hunk_range);
            outln!(w, "  {} {} — {}", row.path, header, row.reason);
        }
        render_rewrite_rows(w, &self.rewritten, &self.refs)
    }
}

impl MarkdownRender for HistoryAbsorbOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_history_absorb_outcome(w, self)
    }
}

/// Non-zero exit from `rebase --exec` (refs were not updated).
#[derive(Debug)]
pub struct RebaseExecExit {
    /// Subprocess exit code to return to the shell.
    pub code: i32,
}

impl std::fmt::Display for RebaseExecExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rebase --exec command failed (exit {})", self.code)
    }
}

impl std::error::Error for RebaseExecExit {}

fn write_exec_streams_to_stderr(row: &ExecRunRecord) {
    let _ = io::stderr().write_all(&row.stdout);
    let _ = io::stderr().write_all(&row.stderr);
}

fn run_rebase_with_exec(
    rewrite: &Rewrite,
    request: &RebaseRequest,
    rebase_opts: RebaseOntoOptions,
    upstream_oid: ObjectId,
    onto_oid: Option<ObjectId>,
    exec: &[String],
    on_err: impl Fn(HistoryError) -> anyhow::Error + Copy,
) -> Result<RebaseRunOutcome> {
    let plan = plan_rebase(&rewrite.repo, request, &rebase_opts).map_err(on_err)?;
    match plan {
        RebasePlan::UpToDate => Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
            HistoryRewriteOutcome::up_to_date("rebase", false),
        ))),
        RebasePlan::Conflict { commit, paths } => Err(json_error::operation_conflict(
            "rebase",
            paths,
            Some(commit),
        )),
        RebasePlan::Plan(plan) => {
            verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
            let spec = ExecSpec {
                commands: exec.to_vec(),
            };
            let runner = system_command_runner();
            let mut exec_json = Vec::new();
            match verify_rebase_with_exec(&rewrite.repo, &plan, &spec, runner.as_ref()) {
                Ok(report) => {
                    for row in &report.runs {
                        write_exec_streams_to_stderr(row);
                        exec_json.push(ExecResultJson {
                            commit: row.commit.to_hex(),
                            command: row.command.clone(),
                            exit_code: row.exit_code,
                        });
                    }
                }
                Err(HistoryError::ExecFailed {
                    exit_code,
                    stdout,
                    stderr,
                    completed_runs,
                    ..
                }) => {
                    for row in &completed_runs {
                        write_exec_streams_to_stderr(row);
                    }
                    let _ = io::stderr().write_all(&stdout);
                    let _ = io::stderr().write_all(&stderr);
                    return Err(RebaseExecExit { code: exit_code }.into());
                }
                Err(other) => return Err(on_err(other)),
            }
            let effective_onto = onto_oid.unwrap_or(upstream_oid);
            let message = format!("rebase: onto {}", short(&effective_onto.to_hex()));
            apply_rewrite(
                &rewrite.repo,
                &plan,
                &rewrite.apply_options("rebase", message)?,
            )
            .map_err(on_err)?;
            Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
                HistoryRewriteOutcome::new("rebase", &plan, false).with_exec(exec_json),
            )))
        }
    }
}

/// Replay local commits onto a new base (`grit rebase` / `grit history rebase`).
pub fn run_rebase(opts: &RebaseRunOptions<'_>) -> Result<RebaseRunOutcome> {
    let shared = opts.shared;
    let flags = opts.flags;
    if !flags.exec.is_empty() && (opts.continue_rebase || opts.skip || opts.abort) {
        bail!("--exec cannot be used with --continue, --skip, or --abort");
    }
    let on_err = history_error("rebase");
    let repo = context::discover()?;
    if opts.abort {
        abort_rebase(&repo, &reflog_identity(&repo)?).map_err(map_rebase_session_error)?;
        return Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::aborted()));
    }

    let rewrite = Rewrite::new(repo, shared)?;
    let resume_apply = rewrite.apply_options("rebase", "rebase: continue".into())?;

    if opts.continue_rebase || opts.skip {
        let head_oid = resolve_commit(&rewrite.repo, "HEAD", "HEAD")?;
        let resume_opts = StartRebaseOptions {
            request: RebaseRequest {
                upstream: head_oid,
                onto: None,
                branch_tip: head_oid,
            },
            rebase: RebaseOntoOptions::new(rewrite.options.clone()),
            apply: resume_apply,
        };
        let outcome = if opts.continue_rebase {
            continue_rebase(&rewrite.repo, &resume_opts).map_err(map_rebase_session_error)?
        } else {
            skip_rebase(&rewrite.repo, &resume_opts).map_err(map_rebase_session_error)?
        };
        return Ok(RebaseRunOutcome::Rebase(map_rebase_outcome(outcome)?));
    }

    let branch_tip = resolve_commit(&rewrite.repo, "HEAD", "HEAD")?;
    let upstream_oid = resolve_rebase_upstream(&rewrite.repo, opts.upstream.as_deref())?;
    let onto_oid = opts
        .onto
        .as_deref()
        .map(|rev| resolve_commit(&rewrite.repo, rev, "onto"))
        .transpose()?;
    let request = RebaseRequest {
        upstream: upstream_oid,
        onto: onto_oid,
        branch_tip,
    };
    let mut rebase_opts = RebaseOntoOptions::new(rewrite.options.clone());
    if flags.check {
        rebase_opts.rewrite.sign = false;
    }
    if let Some(strategy) = flags.strategy {
        rebase_opts.favor = strategy.into();
    }
    if flags.check {
        return run_rebase_check(&rewrite.repo, &request, &rebase_opts, on_err);
    }

    if !flags.exec.is_empty() {
        if shared.dry_run {
            bail!("--exec cannot be used with --dry-run");
        }
        if flags.strategy.is_some() {
            bail!("--exec cannot be used with -X");
        }
        return run_rebase_with_exec(
            &rewrite,
            &request,
            rebase_opts,
            upstream_oid,
            onto_oid,
            &flags.exec,
            on_err,
        );
    }

    if flags.strategy.is_some() {
        let plan = plan_rebase(&rewrite.repo, &request, &rebase_opts).map_err(on_err)?;
        return match plan {
            RebasePlan::UpToDate => Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
                HistoryRewriteOutcome::up_to_date("rebase", shared.dry_run),
            ))),
            RebasePlan::Conflict { commit, paths } => Err(json_error::operation_conflict(
                "rebase",
                paths,
                Some(commit),
            )),
            RebasePlan::Plan(plan) => {
                verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
                if !shared.dry_run {
                    let effective_onto = onto_oid.unwrap_or(upstream_oid);
                    let message = format!("rebase: onto {}", short(&effective_onto.to_hex()));
                    apply_rewrite(
                        &rewrite.repo,
                        &plan,
                        &rewrite.apply_options("rebase", message)?,
                    )
                    .map_err(on_err)?;
                }
                Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
                    HistoryRewriteOutcome::new("rebase", &plan, shared.dry_run),
                )))
            }
        };
    }

    let effective_onto = onto_oid.unwrap_or(upstream_oid);
    let apply_message = format!("rebase: onto {}", short(&effective_onto.to_hex()));
    let start_opts = StartRebaseOptions {
        request,
        rebase: rebase_opts.clone(),
        apply: rewrite.apply_options("rebase", apply_message)?,
    };

    if shared.dry_run {
        let plan = plan_rebase(&rewrite.repo, &start_opts.request, &rebase_opts).map_err(on_err)?;
        return match plan {
            RebasePlan::UpToDate => Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
                HistoryRewriteOutcome::up_to_date("rebase", true),
            ))),
            RebasePlan::Conflict { .. } => bail!(
                "rebase would conflict; run without --dry-run to start a resumable rebase session"
            ),
            RebasePlan::Plan(plan) => {
                verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
                Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
                    HistoryRewriteOutcome::new("rebase", &plan, true),
                )))
            }
        };
    }

    let outcome = start_rebase(&rewrite.repo, &start_opts).map_err(map_rebase_session_error)?;
    Ok(RebaseRunOutcome::Rebase(map_rebase_outcome(outcome)?))
}

fn reflog_identity(repo: &Repository) -> Result<String> {
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    context::committer_reflog_identity(env, &config)
}

fn map_rebase_outcome(outcome: RebaseOutcome) -> Result<RebaseCliOutcome> {
    match outcome {
        RebaseOutcome::Done { plan, .. } => Ok(RebaseCliOutcome::from_rewrite(
            HistoryRewriteOutcome::new("rebase", &plan, false),
        )),
        RebaseOutcome::Stopped(stop) => {
            Ok(RebaseCliOutcome::from_stop(stop.commit, &stop.conflicts))
        }
    }
}

fn map_rebase_session_error(err: RebaseSessionError) -> anyhow::Error {
    match err {
        RebaseSessionError::NoRebaseInProgress => anyhow::anyhow!("no rebase in progress"),
        RebaseSessionError::RebaseAlreadyInProgress => {
            anyhow::anyhow!("rebase is already in progress")
        }
        RebaseSessionError::UnmergedPaths => {
            anyhow::anyhow!("cannot continue: unmerged paths remain in the index")
        }
        RebaseSessionError::NothingToCommit => {
            anyhow::anyhow!("nothing to commit after conflict resolution")
        }
        RebaseSessionError::NotOnBranch => {
            anyhow::anyhow!("rebase session requires HEAD on a branch")
        }
        RebaseSessionError::History(e) => history_error("rebase")(e),
        RebaseSessionError::Library(e) => e.into(),
        _ => err.into(),
    }
}

fn run_rebase_check(
    repo: &Repository,
    request: &RebaseRequest,
    opts: &RebaseOntoOptions,
    on_err: impl Fn(HistoryError) -> anyhow::Error + Copy,
) -> Result<RebaseRunOutcome> {
    match plan_rebase_check(repo, request, opts).map_err(on_err)? {
        RebaseCheckPlan::UpToDate => Ok(RebaseRunOutcome::Check(RebaseCheckOutcome::up_to_date())),
        RebaseCheckPlan::Clean { commits } => {
            Ok(RebaseRunOutcome::Check(RebaseCheckOutcome::clean(commits)))
        }
        RebaseCheckPlan::Conflict { commit, paths } => Ok(RebaseRunOutcome::Check(
            RebaseCheckOutcome::conflicts(commit, paths),
        )),
    }
}

fn resolve_rebase_upstream(repo: &Repository, upstream: Option<&str>) -> Result<ObjectId> {
    let spec = upstream.unwrap_or("@{upstream}");
    match resolve_revision_as_commit(repo, spec) {
        Ok(oid) => Ok(oid),
        Err(LibError::RevParse(RevParseError::NoUpstream { branch })) if upstream.is_none() => {
            anyhow::bail!(
                "no upstream configured for branch '{branch}'\n\
                 Set one with `grit branch -u <remote>/<branch>`"
            )
        }
        Err(other) => Err(other).with_context(|| format!("could not resolve upstream '{spec}'")),
    }
}

/// Restore refs (and the worktree when needed) from the last recorded history rewrite.
pub fn run_undo(shared: &HistoryRewriteArgs) -> Result<HistoryRewriteOutcome> {
    let on_err = history_error("history undo");
    let repo = context::discover()?;
    let rewrite = Rewrite::new(repo, shared)?;
    let apply_opts = ApplyOptions {
        identity: context::committer_reflog_identity(rewrite.repo.environment(), &rewrite.config)?,
        reflog_message: "history undo".into(),
        record_operation: None,
    };
    let outcome = undo_last_rewrite(&rewrite.repo, &apply_opts, shared.dry_run).map_err(on_err)?;
    Ok(HistoryRewriteOutcome {
        operation: "undo".into(),
        rewritten: Vec::new(),
        refs: outcome
            .ref_updates
            .iter()
            .map(|u| RefPair {
                ref_name: u.name.clone(),
                name: u.name.clone(),
                old: u.old.to_hex(),
                new: u.new.to_hex(),
            })
            .collect(),
        head: outcome.head_change.map(|(old, new)| HeadPair {
            old: old.to_hex(),
            new: new.to_hex(),
        }),
        dry_run: outcome.dry_run,
        exec: Vec::new(),
    })
}

/// Assign staged hunks into history and replay descendants once.
pub fn run_absorb(base: Option<&str>, shared: &HistoryRewriteArgs) -> Result<HistoryAbsorbOutcome> {
    let on_err = history_error("history absorb");
    let repo = context::discover()?;
    let base = base
        .map(|rev| resolve_commit(&repo, rev, "base commit"))
        .transpose()?;
    let rewrite = Rewrite::new(repo, shared)?;
    let absorb_plan = absorb(
        &rewrite.repo,
        &AbsorbOptions {
            base,
            rewrite: rewrite.options.clone(),
        },
    )
    .map_err(on_err)?;
    if !shared.dry_run && !absorb_plan.plan.commits.is_empty() {
        apply_absorb_rewrite(
            &rewrite.repo,
            &absorb_plan,
            &rewrite.apply_options("absorb", "history absorb".into())?,
        )
        .map_err(on_err)?;
    }
    let summary = HistoryRewriteOutcome::new("absorb", &absorb_plan.plan, shared.dry_run);
    Ok(HistoryAbsorbOutcome {
        operation: summary.operation,
        assignments: absorb_plan.assignments.iter().map(Into::into).collect(),
        unassigned: absorb_plan.unassigned.iter().map(Into::into).collect(),
        rewritten: summary.rewritten,
        refs: summary.refs,
        head: summary.head,
        dry_run: summary.dry_run,
    })
}

/// Message from `-m` or `--message-file` (`-` reads stdin); clap rejects passing both.
fn read_message(message: Option<String>, message_file: Option<String>) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(message), _) => Ok(Some(message)),
        (None, None) => Ok(None),
        (None, Some(path)) if path == "-" => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("could not read message from stdin")?;
            Ok(Some(buf))
        }
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map(Some)
            .with_context(|| format!("could not read message file '{path}'")),
    }
}

fn format_absorb_hunk_header(range: &HunkRangeOut) -> String {
    format_hunk_range_header(&HunkRange {
        old_start: range.old_start,
        old_lines: range.old_lines,
        new_start: range.new_start,
        new_lines: range.new_lines,
    })
}

/// Error mapper for `operation` (for example `history drop`), for use with `map_err`.
pub(crate) fn history_error(
    operation: &'static str,
) -> impl Fn(HistoryError) -> anyhow::Error + Copy {
    move |err| map_history_error(err, operation)
}

fn map_history_error(err: HistoryError, operation: &str) -> anyhow::Error {
    match err {
        HistoryError::EmptyMessage => anyhow::anyhow!("commit message cannot be empty"),
        HistoryError::NothingToReword => {
            anyhow::anyhow!("reword requires a new message or author")
        }
        HistoryError::InvalidAuthor(err) => anyhow::anyhow!("{err}"),
        HistoryError::BareRepository => {
            anyhow::anyhow!("cannot run {operation} in a bare repository")
        }
        HistoryError::NothingStaged => {
            anyhow::anyhow!("nothing to {operation}: no staged changes")
        }
        HistoryError::FixupMakesTargetEmpty { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("{operation} makes commit {short} empty")
        }
        HistoryError::Conflict { mut paths, .. } => {
            paths.sort();
            paths.dedup();
            json_error::operation_conflict(operation, paths, None)
        }
        HistoryError::Planning { message } => {
            anyhow::anyhow!("history rewrite planning failed: {message}")
        }
        HistoryError::Signing { message } => {
            anyhow::anyhow!("commit signing failed: {message}")
        }
        HistoryError::InsertBeforeOnMerge { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("insert-before on merge commit {short} is not supported")
        }
        HistoryError::MergeCommitInReplay { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!(
                "{short} is a merge commit — {operation} cannot drop or reorder merge commits yet"
            )
        }
        HistoryError::RootCommit { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!(
                "{short} is a root commit and cannot be dropped or replayed without a parent"
            )
        }
        HistoryError::BecameEmpty { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("replay of {short} would become empty — refusing rewrite")
        }
        HistoryError::NotInHistory { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("{short} is not reachable from HEAD for this rewrite scope")
        }
        HistoryError::MoveTargetNotInHistory { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("{short} is not on the linear first-parent history of HEAD")
        }
        HistoryError::NoOp { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("{short} is already at the requested position")
        }
        HistoryError::NotLinear => anyhow::anyhow!(
            "history move requires a linear first-parent range without merge commits"
        ),
        HistoryError::DivergentBranch { name } => anyhow::anyhow!(
            "branch {name} diverged before the rewritten commits and cannot be updated"
        ),
        HistoryError::BranchCheckedOutElsewhere { name, worktree } => {
            anyhow::anyhow!("branch {name} is checked out in worktree {worktree}")
        }
        HistoryError::LocalChangesWouldBeOverwritten { mut paths } => {
            paths.sort();
            paths.dedup();
            anyhow::anyhow!(
                "local changes would be overwritten:\n  {}",
                paths.join("\n  ")
            )
        }
        HistoryError::RefCasMismatch { name } => anyhow::anyhow!(
            "ref {name} changed since the rewrite plan was built — retry the command"
        ),
        HistoryError::RefApply { message } => {
            anyhow::anyhow!("ref update failed: {message}")
        }
        HistoryError::WorktreeApply { message } => {
            anyhow::anyhow!("worktree update failed: {message}")
        }
        HistoryError::SplitSelectsNothing => {
            anyhow::anyhow!("pathspec did not select any changes from the commit")
        }
        HistoryError::SplitSelectsEverything => {
            anyhow::anyhow!("pathspec selected every change in the commit")
        }
        HistoryError::NoUndoRecord => anyhow::anyhow!("no history rewrite to undo"),
        HistoryError::UndoStale { name } => anyhow::anyhow!(
            "ref {name} changed since the last history rewrite — undo is no longer safe"
        ),
        HistoryError::UndoRecordRead { message } | HistoryError::UndoRecordWrite { message } => {
            anyhow::anyhow!("history undo record error: {message}")
        }
        _ => anyhow::anyhow!("{err}"),
    }
}
