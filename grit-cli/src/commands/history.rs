//! `grit history` — stateless history rewrite helpers.

use std::io::{self, Read, Write};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    absorb, apply_absorb_rewrite, apply_fixup_rewrite, apply_rewrite, drop as history_drop, fixup,
    move_commit, reword, squash, staged_fixup_paths, verify_rewrite_plan, AbsorbOptions,
    ApplyOptions, EmptyPolicy, FixupOptions, HistoryError, HunkAssignment, HunkRange, Placement,
    RefScope, RewriteOptions, RewritePlan, SquashMessage, SquashOptions, UnassignedHunk,
};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
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

/// Result of `grit history drop`, `move`, `reword`, `fixup` and `squash`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HistoryRewriteOutcome {
    /// Operation name (`drop`, `move`, `reword`, `fixup` or `squash`).
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

impl HistoryRewriteOutcome {
    fn new(operation: &str, plan: &RewritePlan, dry_run: bool) -> Self {
        Self {
            operation: operation.into(),
            rewritten: plan
                .commits
                .iter()
                .map(|row| RewrittenPair {
                    old: row.old.to_hex(),
                    new: row.new.map(|new| new.to_hex()),
                })
                .collect(),
            refs: plan
                .ref_updates
                .iter()
                .map(|u| RefPair {
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
        }
    }
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
            Some(new) => outln!(w, "  commit {} → {}", short(&pair.old), short(new)),
            None => outln!(w, "  commit {} → (dropped)", short(&pair.old)),
        }
    }
    for update in refs {
        outln!(
            w,
            "  {} {} → {}",
            update.name,
            short(&update.old),
            short(&update.new)
        );
    }
    Ok(())
}

impl HumanRender for HistoryRewriteOutcome {
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
                empty: EmptyPolicy::Drop,
                committer,
                preserve_committer: false,
            },
        })
    }

    fn apply_options(&self, reflog_message: String) -> Result<ApplyOptions> {
        Ok(ApplyOptions {
            identity: context::committer_reflog_identity(self.repo.environment(), &self.config)?,
            reflog_message,
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
        apply_rewrite(&rewrite.repo, &plan, &rewrite.apply_options(message)?).map_err(on_err)?;
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
        apply_rewrite(&rewrite.repo, &plan, &rewrite.apply_options(message)?).map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("drop", &plan, shared.dry_run))
}

/// Rewrite `commit`'s message to `message` and replay descendants.
pub fn run_reword(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let Some(message) = read_message(message, message_file)? else {
        bail!("a new message is required (-m/--message or --message-file)");
    };
    let on_err = history_error("history reword");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = reword(&rewrite.repo, commit_oid, message, &rewrite.options).map_err(on_err)?;
    verify_rewrite_plan(&rewrite.repo, &plan).map_err(on_err)?;
    let new_oid = plan
        .replacement(commit_oid)
        .ok_or_else(|| anyhow::anyhow!("rewritten commit missing from plan"))?;
    if !shared.dry_run {
        let message = format!("history reword: {}", rewrite.subject(new_oid)?);
        apply_rewrite(&rewrite.repo, &plan, &rewrite.apply_options(message)?).map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("reword", &plan, shared.dry_run))
}

/// Apply staged changes onto `commit` and replay descendants.
pub fn run_fixup(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    empty: EmptyPolicyArg,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let message = read_message(message, message_file)?;
    let on_err = history_error("history fixup");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let staged_paths = staged_fixup_paths(&repo).map_err(on_err)?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = fixup(
        &rewrite.repo,
        commit_oid,
        &FixupOptions {
            message,
            empty: empty.into(),
            rewrite: RewriteOptions {
                empty: empty.into(),
                ..rewrite.options.clone()
            },
        },
    )
    .map_err(on_err)?;
    if !shared.dry_run {
        let subject = match plan.replacement(commit_oid) {
            Some(new_oid) => rewrite.subject(new_oid)?,
            None => commit.to_owned(),
        };
        apply_fixup_rewrite(
            &rewrite.repo,
            &plan,
            &rewrite.apply_options(format!("history fixup: {subject}"))?,
            &staged_paths,
        )
        .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("fixup", &plan, shared.dry_run))
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
        apply_rewrite(&rewrite.repo, &plan, &rewrite.apply_options(message)?).map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("squash", &plan, shared.dry_run))
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
            outln!(w, "  {} → commit {}", row.path, short(&row.commit));
        }
        for row in &self.unassigned {
            outln!(w, "  {} stays staged ({})", row.path, row.reason);
        }
        render_rewrite_rows(w, &self.rewritten, &self.refs)
    }
}

impl MarkdownRender for HistoryAbsorbOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_history_absorb_outcome(w, self)
    }
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
            &rewrite.apply_options("history absorb".into())?,
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

/// Error mapper for `operation` (for example `history drop`), for use with `map_err`.
fn history_error(operation: &'static str) -> impl Fn(HistoryError) -> anyhow::Error + Copy {
    move |err| map_history_error(err, operation)
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
