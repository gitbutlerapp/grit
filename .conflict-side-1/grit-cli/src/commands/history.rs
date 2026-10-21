//! `grit history` — stateless history rewrite helpers.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::history::{
    absorb, apply_absorb_rewrite, apply_fixup_rewrite, apply_rewrite, drop as history_drop, fixup,
    move_commit, reword, squash, staged_fixup_paths, verify_rewrite_plan, AbsorbOptions,
    ApplyOptions, EmptyPolicy, FixupOptions, HistoryError, HunkAssignment, HunkRange, Placement,
    RefScope, RewriteOptions, RewritePlan, SquashMessage, SquashOptions, UnassignedHunk,
};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::json_error;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

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

fn squash_message_from_cli(
    message: Option<String>,
    message_file: Option<String>,
    combine: bool,
) -> Result<SquashMessage> {
    if combine && (message.is_some() || message_file.is_some()) {
        bail!("use either -m/--message, --message-file, or --combine, not more than one");
    }
    if combine {
        return Ok(SquashMessage::Combine);
    }
    match (message, message_file) {
        (None, None) => Ok(SquashMessage::Oldest),
        (Some(_), Some(_)) => bail!("use either -m/--message or --message-file, not both"),
        (Some(m), None) => Ok(SquashMessage::Custom(m)),
        (None, Some(path)) => {
            let raw = if path == "-" {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .context("could not read message from stdin")?;
                buf
            } else {
                std::fs::read_to_string(&path)
                    .with_context(|| format!("could not read message file '{path}'"))?
            };
            Ok(SquashMessage::Custom(raw))
        }
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
}

/// Result of `grit history move`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HistoryMoveOutcome {
    /// Operation name (`move`).
    pub operation: String,
    /// Original and replacement commit OIDs for every rewritten commit.
    pub rewritten: Vec<RewrittenPair>,
    /// Planned or applied ref tip updates.
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes; JSON `null` when unchanged.
    pub head: Option<HeadPair>,
    /// Whether refs were left unchanged (`true` for `--dry-run`).
    pub dry_run: bool,
}

/// Result of `grit history reword`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HistoryRewordOutcome {
    /// Operation name (`reword`).
    pub operation: String,
    /// Original and replacement commit OIDs for every rewritten commit.
    pub rewritten: Vec<RewrittenPair>,
    /// Planned or applied ref tip updates.
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes; JSON `null` when unchanged.
    pub head: Option<HeadPair>,
    /// Whether refs were left unchanged (`true` for `--dry-run`).
    pub dry_run: bool,
}

/// One commit OID before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RewrittenPair {
    /// Commit OID before rewrite (full hex).
    pub old: String,
    /// Replacement commit OID (full hex), or JSON `null` when the commit was dropped.
    pub new: Option<String>,
}

/// One ref tip before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefPair {
    /// Full ref name.
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

impl HumanRender for HistoryRewordOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(w, "Dry run — refs would change:");
        } else if self.operation == "drop" {
            outln!(
                w,
                "Dropped 1 commit, replayed {} descendant commit(s)",
                self.rewritten.len()
            );
        } else {
            let verb = match self.operation.as_str() {
                "fixup" => "Fixed up",
                "squash" => "Squashed",
                _ => "Rewrote",
            };
            outln!(w, "{verb} {} commit(s)", self.rewritten.len());
        }
        for pair in &self.rewritten {
            let old_short = pair.old.get(..7).unwrap_or(&pair.old);
            match &pair.new {
                Some(new) => {
                    let new_short = new.get(..7).unwrap_or(new);
                    outln!(w, "  commit {old_short} → {new_short}");
                }
                None => outln!(w, "  commit {old_short} → (dropped)"),
            }
        }
        for update in &self.refs {
            let old_short = update.old.get(..7).unwrap_or(&update.old);
            let new_short = update.new.get(..7).unwrap_or(&update.new);
            outln!(w, "  {} {old_short} → {new_short}", update.name);
        }
        Ok(())
    }
}

fn history_rewrite_markdown_title(operation: &str) -> &'static str {
    match operation {
        "drop" => "History drop",
        "fixup" => "History fixup",
        "squash" => "History squash",
        "reword" => "History reword",
        _ => "History rewrite",
    }
}

impl MarkdownRender for HistoryRewordOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_history_rewrite_outcome(
            w,
            history_rewrite_markdown_title(&self.operation),
            self,
        )?;
        Ok(())
    }
}

impl HumanRender for HistoryMoveOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        HistoryRewordOutcome {
            operation: self.operation.clone(),
            rewritten: self.rewritten.clone(),
            refs: self.refs.clone(),
            head: self.head.clone(),
            dry_run: self.dry_run,
        }
        .render_human(w, ctx)
    }
}

impl MarkdownRender for HistoryMoveOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_history_rewrite_outcome(
            w,
            "History move",
            &HistoryRewordOutcome {
                operation: self.operation.clone(),
                rewritten: self.rewritten.clone(),
                refs: self.refs.clone(),
                head: self.head.clone(),
                dry_run: self.dry_run,
            },
        )?;
        Ok(())
    }
}

/// Reorder `commit` relative to `--before` or `--after` on the first-parent chain of `HEAD`.
pub fn run_move(
    commit: &str,
    before: Option<String>,
    after: Option<String>,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryMoveOutcome> {
    if before.is_some() && after.is_some() {
        bail!("use either --before or --after, not both");
    }
    let use_before = before.is_some();
    let anchor = before
        .or(after)
        .ok_or_else(|| anyhow::anyhow!("--before or --after is required"))?;
    let repo = context::discover()?;
    let commit_oid = resolve_revision_as_commit(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
    let anchor_oid = resolve_revision_as_commit(&repo, &anchor)
        .with_context(|| format!("could not resolve anchor commit '{anchor}'"))?;
    let placement = if use_before {
        Placement::Before(anchor_oid)
    } else {
        Placement::After(anchor_oid)
    };
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let plan = move_commit(
        &repo,
        commit_oid,
        placement,
        &RewriteOptions {
            scope: shared.update_refs.into(),
            empty: EmptyPolicy::Drop,
            committer,
            preserve_committer: false,
        },
    )
    .map_err(map_move_error)?;
    let short = &commit_oid.to_hex()[..7];
    let mut outcome = move_outcome_from_plan(&repo, &plan, shared.dry_run)?;
    if !shared.dry_run {
        let identity = context::committer_reflog_identity(env, &config)?;
        apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity,
                reflog_message: format!("history move: {short}"),
            },
        )
        .map_err(map_move_error)?;
    }
    outcome.dry_run = shared.dry_run;
    Ok(outcome)
}

/// Remove `commit` from history and replay descendants onto its parent.
pub fn run_drop(commit: &str, shared: &HistoryRewriteArgs) -> Result<HistoryRewordOutcome> {
    let repo = context::discover()?;
    let commit_oid = resolve_revision_as_commit(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let plan = history_drop(
        &repo,
        commit_oid,
        &RewriteOptions {
            scope: shared.update_refs.into(),
            empty: EmptyPolicy::Drop,
            committer,
            preserve_committer: false,
        },
    )
    .map_err(map_history_error_reword)?;
    verify_rewrite_plan(&repo, &plan).map_err(map_history_error_drop)?;
    let subject = dropped_subject(&repo, commit_oid)?;
    let mut outcome = drop_outcome_from_plan(&repo, &plan, shared.dry_run)?;
    if !shared.dry_run {
        let identity = context::committer_reflog_identity(env, &config)?;
        apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity,
                reflog_message: format!("history drop: {subject}"),
            },
        )
        .map_err(map_history_error_drop)?;
    }
    outcome.dry_run = shared.dry_run;
    Ok(outcome)
}

/// Rewrite `commit`'s message to `message` and replay descendants.
pub fn run_reword(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewordOutcome> {
    let message = read_message(message, message_file)?;
    let repo = context::discover()?;
    let commit_oid = resolve_revision_as_commit(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let plan = reword(
        &repo,
        commit_oid,
        message,
        &RewriteOptions {
            scope: shared.update_refs.into(),
            empty: EmptyPolicy::Drop,
            committer,
            preserve_committer: false,
        },
    )
    .map_err(map_history_error_reword)?;
    verify_rewrite_plan(&repo, &plan).map_err(map_history_error_reword)?;
    let subject = rewritten_subject(&repo, &plan, commit_oid)?;
    let mut outcome = outcome_from_plan(&repo, &plan, shared.dry_run)?;
    if !shared.dry_run {
        let identity = context::committer_reflog_identity(env, &config)?;
        apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity,
                reflog_message: format!("history reword: {subject}"),
            },
        )
        .map_err(map_history_error_reword)?;
    }
    outcome.dry_run = shared.dry_run;
    Ok(outcome)
}

/// Apply staged changes onto `commit` and replay descendants.
pub fn run_fixup(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    empty: EmptyPolicyArg,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewordOutcome> {
    let message = read_optional_message(message, message_file)?;
    let repo = context::discover()?;
    let commit_oid = resolve_revision_as_commit(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
    let staged_paths = staged_fixup_paths(&repo).map_err(map_history_error_fixup)?;
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let plan = fixup(
        &repo,
        commit_oid,
        &FixupOptions {
            message,
            empty: empty.into(),
            rewrite: RewriteOptions {
                scope: shared.update_refs.into(),
                empty: empty.into(),
                committer,
                preserve_committer: false,
            },
        },
    )
    .map_err(map_history_error_fixup)?;
    let subject = fixup_reflog_subject(&repo, &plan, commit_oid, commit)?;
    let mut outcome = outcome_from_plan_fixup(&repo, &plan, shared.dry_run)?;
    if !shared.dry_run {
        let identity = context::committer_reflog_identity(env, &config)?;
        apply_fixup_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity,
                reflog_message: format!("history fixup: {subject}"),
            },
            &staged_paths,
        )
        .map_err(map_history_error_fixup)?;
    }
    outcome.dry_run = shared.dry_run;
    Ok(outcome)
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
            let short = row.commit.get(..7).unwrap_or(&row.commit);
            outln!(w, "  {} → commit {short}", row.path);
        }
        for row in &self.unassigned {
            outln!(w, "  {} stays staged ({})", row.path, row.reason);
        }
        for pair in &self.rewritten {
            let old_short = pair.old.get(..7).unwrap_or(&pair.old);
            match &pair.new {
                Some(new) => {
                    let new_short = new.get(..7).unwrap_or(new);
                    outln!(w, "  commit {old_short} → {new_short}");
                }
                None => outln!(w, "  commit {old_short} → (dropped)"),
            }
        }
        for update in &self.refs {
            let old_short = update.old.get(..7).unwrap_or(&update.old);
            let new_short = update.new.get(..7).unwrap_or(&update.new);
            outln!(w, "  {} {old_short} → {new_short}", update.name);
        }
        Ok(())
    }
}

impl MarkdownRender for HistoryAbsorbOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_history_absorb_outcome(w, self)
    }
}

/// Squash a revision range into the oldest commit and replay descendants.
pub fn run_squash(
    range: &[String],
    message: Option<String>,
    message_file: Option<String>,
    combine: bool,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewordOutcome> {
    if range.is_empty() {
        bail!("revision range is required");
    }
    let message_policy = squash_message_from_cli(message, message_file, combine)?;
    let range_refs: Vec<&str> = range.iter().map(String::as_str).collect();
    let repo = context::discover()?;
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let plan = squash(
        &repo,
        &range_refs,
        &SquashOptions {
            message: message_policy,
            rewrite: RewriteOptions {
                scope: shared.update_refs.into(),
                empty: EmptyPolicy::Drop,
                committer,
                preserve_committer: false,
            },
        },
    )
    .map_err(map_history_error_squash)?;
    verify_rewrite_plan(&repo, &plan).map_err(map_history_error_squash)?;
    let subject = squash_reflog_subject(&repo, &plan)?;
    let mut outcome = outcome_from_plan_squash(&repo, &plan, shared.dry_run)?;
    if !shared.dry_run {
        let identity = context::committer_reflog_identity(env, &config)?;
        apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity,
                reflog_message: format!("history squash: {subject}"),
            },
        )
        .map_err(map_history_error_squash)?;
    }
    outcome.dry_run = shared.dry_run;
    Ok(outcome)
}

/// Assign staged hunks into history and replay descendants once.
pub fn run_absorb(base: Option<&str>, shared: &HistoryRewriteArgs) -> Result<HistoryAbsorbOutcome> {
    let repo = context::discover()?;
    let base_oid = match base {
        Some(rev) => Some(
            resolve_revision_as_commit(&repo, rev)
                .with_context(|| format!("could not resolve base commit '{rev}'"))?,
        ),
        None => None,
    };
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let absorb_plan = absorb(
        &repo,
        &AbsorbOptions {
            base: base_oid,
            rewrite: RewriteOptions {
                scope: shared.update_refs.into(),
                empty: EmptyPolicy::Drop,
                committer,
                preserve_committer: false,
            },
        },
    )
    .map_err(map_history_error_absorb)?;
    let mut outcome = outcome_from_absorb_plan(&repo, &absorb_plan, shared.dry_run)?;
    if !shared.dry_run && !absorb_plan.plan.commits.is_empty() {
        let identity = context::committer_reflog_identity(env, &config)?;
        apply_absorb_rewrite(
            &repo,
            &absorb_plan,
            &ApplyOptions {
                identity,
                reflog_message: "history absorb".into(),
            },
        )
        .map_err(map_history_error_absorb)?;
    }
    outcome.dry_run = shared.dry_run;
    Ok(outcome)
}

fn outcome_from_absorb_plan(
    repo: &grit_lib::repo::Repository,
    plan: &grit_lib::revwalk::history::AbsorbPlan,
    dry_run: bool,
) -> Result<HistoryAbsorbOutcome> {
    let base = outcome_from_plan(repo, &plan.plan, dry_run)?;
    Ok(HistoryAbsorbOutcome {
        operation: "absorb".into(),
        assignments: plan.assignments.iter().map(map_assignment_out).collect(),
        unassigned: plan.unassigned.iter().map(map_unassigned_out).collect(),
        rewritten: base.rewritten,
        refs: base.refs,
        head: base.head,
        dry_run: base.dry_run,
    })
}

fn map_assignment_out(row: &HunkAssignment) -> AbsorbAssignmentOut {
    AbsorbAssignmentOut {
        path: row.path.clone(),
        hunk_range: map_hunk_range(&row.hunk_range),
        commit: row.commit.to_hex(),
    }
}

fn map_unassigned_out(row: &UnassignedHunk) -> UnassignedHunkOut {
    UnassignedHunkOut {
        path: row.path.clone(),
        hunk_range: map_hunk_range(&row.hunk_range),
        reason: row.reason.as_str().into(),
    }
}

fn map_hunk_range(range: &HunkRange) -> HunkRangeOut {
    HunkRangeOut {
        old_start: range.old_start,
        old_lines: range.old_lines,
        new_start: range.new_start,
        new_lines: range.new_lines,
    }
}

fn map_history_error_absorb(err: HistoryError) -> anyhow::Error {
    map_history_error(err, "history absorb")
}

fn read_optional_message(
    message: Option<String>,
    message_file: Option<String>,
) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(_), Some(_)) => bail!("use either -m/--message or --message-file, not both"),
        (None, None) => Ok(None),
        (Some(m), None) => Ok(Some(m)),
        (None, Some(path)) => {
            if path == "-" {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .context("could not read message from stdin")?;
                Ok(Some(buf))
            } else {
                Ok(Some(std::fs::read_to_string(&path).with_context(|| {
                    format!("could not read message file '{path}'")
                })?))
            }
        }
    }
}

fn fixup_reflog_subject(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    target: ObjectId,
    rev: &str,
) -> Result<String> {
    if let Some(row) = plan.commits.iter().find(|r| r.old == target) {
        if let Some(new) = row.new {
            let commit = context::read_commit(repo, &new)?;
            return Ok(context::subject_line(&commit.message));
        }
    }
    Ok(rev.to_owned())
}

fn outcome_from_plan_fixup(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    dry_run: bool,
) -> Result<HistoryRewordOutcome> {
    let mut outcome = outcome_from_plan(repo, plan, dry_run)?;
    outcome.operation = "fixup".into();
    Ok(outcome)
}

fn outcome_from_plan_squash(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    dry_run: bool,
) -> Result<HistoryRewordOutcome> {
    let mut outcome = outcome_from_plan(repo, plan, dry_run)?;
    outcome.operation = "squash".into();
    Ok(outcome)
}

fn squash_reflog_subject(repo: &grit_lib::repo::Repository, plan: &RewritePlan) -> Result<String> {
    let Some(first) = plan.commits.first() else {
        return Ok("squash".into());
    };
    if let Some(new) = first.new {
        let commit = context::read_commit(repo, &new)?;
        return Ok(context::subject_line(&commit.message));
    }
    let commit = context::read_commit(repo, &first.old)?;
    Ok(context::subject_line(&commit.message))
}

fn read_message(message: Option<String>, message_file: Option<String>) -> Result<String> {
    match (message, message_file) {
        (Some(_), Some(_)) => bail!("use either -m/--message or --message-file, not both"),
        (None, None) => bail!("a new message is required (-m/--message or --message-file)"),
        (Some(m), None) => Ok(m),
        (None, Some(path)) => {
            if path == "-" {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .context("could not read message from stdin")?;
                Ok(buf)
            } else {
                std::fs::read_to_string(&path)
                    .with_context(|| format!("could not read message file '{path}'"))
            }
        }
    }
}

fn dropped_subject(repo: &grit_lib::repo::Repository, dropped: ObjectId) -> Result<String> {
    let commit = context::read_commit(repo, &dropped)?;
    Ok(context::subject_line(&commit.message))
}

fn drop_outcome_from_plan(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    dry_run: bool,
) -> Result<HistoryRewordOutcome> {
    Ok(HistoryRewordOutcome {
        operation: "drop".into(),
        rewritten: rewritten_pairs(plan),
        refs: ref_pairs(plan),
        head: head_pair(repo, plan)?,
        dry_run,
    })
}

fn rewritten_subject(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    target: ObjectId,
) -> Result<String> {
    let new_oid = plan
        .commits
        .iter()
        .find(|r| r.old == target)
        .and_then(|r| r.new)
        .ok_or_else(|| anyhow::anyhow!("rewritten commit missing from plan"))?;
    let commit = context::read_commit(repo, &new_oid)?;
    Ok(context::subject_line(&commit.message))
}

fn move_outcome_from_plan(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    dry_run: bool,
) -> Result<HistoryMoveOutcome> {
    let head = head_pair(repo, plan)?;
    Ok(HistoryMoveOutcome {
        operation: "move".into(),
        rewritten: rewritten_pairs(plan),
        refs: ref_pairs(plan),
        head,
        dry_run,
    })
}

fn outcome_from_plan(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    dry_run: bool,
) -> Result<HistoryRewordOutcome> {
    Ok(HistoryRewordOutcome {
        operation: "reword".into(),
        rewritten: rewritten_pairs(plan),
        refs: ref_pairs(plan),
        head: head_pair(repo, plan)?,
        dry_run,
    })
}

fn rewritten_pairs(plan: &RewritePlan) -> Vec<RewrittenPair> {
    plan.commits
        .iter()
        .map(|row| RewrittenPair {
            old: row.old.to_hex(),
            new: row.new.map(|new| new.to_hex()),
        })
        .collect()
}

fn ref_pairs(plan: &RewritePlan) -> Vec<RefPair> {
    plan.ref_updates
        .iter()
        .map(|u| RefPair {
            name: u.name.clone(),
            old: u.old.to_hex(),
            new: u.new.to_hex(),
        })
        .collect()
}

fn head_pair(repo: &grit_lib::repo::Repository, plan: &RewritePlan) -> Result<Option<HeadPair>> {
    if !plan.head.commit_changes {
        return Ok(None);
    }
    let old_oid = plan.head.old_commit.or_else(|| head_commit_oid(repo).ok());
    let Some(old_oid) = old_oid else {
        return Ok(None);
    };
    let new_oid = plan
        .ref_updates
        .iter()
        .find(|u| u.old == old_oid)
        .map(|u| u.new)
        .or_else(|| {
            let mapping: HashMap<ObjectId, ObjectId> = plan
                .commits
                .iter()
                .filter_map(|r| r.new.map(|n| (r.old, n)))
                .collect();
            mapping.get(&old_oid).copied()
        })
        .unwrap_or(old_oid);
    Ok(Some(HeadPair {
        old: old_oid.to_hex(),
        new: new_oid.to_hex(),
    }))
}

fn head_commit_oid(repo: &grit_lib::repo::Repository) -> Result<ObjectId> {
    match resolve_head(repo.git_dir())? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => Ok(oid),
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => {
            bail!("HEAD does not point at a commit")
        }
    }
}

fn map_move_error(err: HistoryError) -> anyhow::Error {
    map_history_error(err, "history move")
}

fn map_history_error_drop(err: HistoryError) -> anyhow::Error {
    map_history_error(err, "history drop")
}

fn map_history_error_reword(err: HistoryError) -> anyhow::Error {
    map_history_error(err, "history reword")
}

fn map_history_error_fixup(err: HistoryError) -> anyhow::Error {
    map_history_error(err, "history fixup")
}

fn map_history_error_squash(err: HistoryError) -> anyhow::Error {
    map_history_error(err, "history squash")
}

fn map_history_error(err: HistoryError, operation: &str) -> anyhow::Error {
    match err {
        HistoryError::EmptyMessage => anyhow::anyhow!("commit message cannot be empty"),
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
            json_error::operation_conflict(operation, paths)
        }
        HistoryError::Planning { message } => {
            anyhow::anyhow!("history rewrite planning failed: {message}")
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
        _ => anyhow::anyhow!("{err}"),
    }
}
