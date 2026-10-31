//! `grit status` (and bare `grit`) — the dashboard: where you are, the commits
//! you're ahead by, what's changed, and what to do next.

use anyhow::{Context, Result};
use grit_lib::diff::DiffEntry;
use grit_lib::gitbutler::GitButlerWorkspace;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{Conflict, ConflictKind};
use grit_lib::porcelain::status::{
    short_status_lines, status, status_options_from_config, ConflictGroup, IgnoredMode,
    InterruptedRewrite, StatusAction, StatusGuidance, UntrackedMode,
};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::refs::branch_tracking::{
    status_ahead_behind_enabled, BranchStanding, Standing, StandingHeader, StandingOptions,
    StatusHeader, StatusIntegrationAction, StatusIntegrationHint, TipComparison, TrackedComparison,
};
use grit_lib::state::{detect_in_progress, HeadState, InProgressOperation, NextStep};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context::{self, CommitSummary};
use crate::json_error::LockHolderJson;
use crate::markdown;
use crate::output::{
    change_json, outln, ChangeJson, CommitJson, HumanRender, MarkdownRender, RenderContext,
};
use crate::pathspec_args::PathspecArgs;
use crate::ui::{self, PathDisplayOptions};
use clap::Args;

/// Maximum number of commits to list in the status shortlog before summarizing.
const SHORTLOG_LIMIT: usize = 10;

/// One unmerged path in `--json` output.
#[derive(Serialize, JsonSchema)]
pub struct ConflictJson {
    /// Repository-relative path.
    pub path: String,
    /// Conflict category (`both_modified`, `both_added`, …).
    pub kind: String,
}

/// Paused operation and suggested `grit` commands.
#[derive(Serialize, JsonSchema)]
pub struct OperationJson {
    /// Stable operation id (for example `merge`).
    pub kind: String,
    /// Suggested follow-up commands, in order. A `git …` command appears only for an
    /// operation `git` started that grit cannot resume (for example `git rebase --continue`).
    pub next_steps: Vec<String>,
    /// Mutually exclusive options (for example cancel the merge).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub alternatives: Vec<String>,
}

/// Result of `grit status`.
#[derive(Serialize, JsonSchema)]
pub struct StatusOutcome {
    /// Current branch (short name), or `null` when detached / invalid. While a rebase is in
    /// progress, the branch being rebased (`null` when the rebase started detached).
    pub branch: Option<String>,
    pub detached: bool,
    /// HEAD commit (full oid), or `null` on an unborn / invalid HEAD.
    pub head: Option<String>,
    /// Target branch name, or `null` when none applies / was found.
    pub target: Option<String>,
    /// Number of commits ahead of `target` (full count), or `null` when counts were skipped.
    pub ahead: Option<usize>,
    /// Number of commits on `target` not in the current branch (full count), or `null` when skipped.
    pub behind: Option<usize>,
    /// Upstream tracking comparison when it differs from `target`, or `null`.
    pub upstream: Option<StatusUpstreamJson>,
    /// Newest ahead-of-target commits (newest first), up to the status shortlog limit.
    pub commits: Vec<CommitJson>,
    pub staged: Vec<ChangeJson>,
    pub unstaged: Vec<ChangeJson>,
    pub untracked: Vec<String>,
    /// Untracked nested repositories that would be recorded as gitlinks if staged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedded_repositories: Vec<String>,
    pub clean: bool,
    /// True when a merge is in progress (`MERGE_HEAD` exists). Deprecated: prefer `operation`.
    pub merging: bool,
    /// In-progress operations (e.g. `merge`, `rebase`), when any apply. Deprecated: prefer `operation`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub in_progress: Vec<String>,
    /// Unmerged paths with conflict kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflicts: Vec<ConflictJson>,
    /// Primary paused operation and suggested next commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub operation: Option<OperationJson>,
    /// Ignored paths (only when `--ignored` is passed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub ignored: Option<Vec<String>>,
    /// Number of entries on the stash reflog.
    pub stash_count: usize,
    /// What status did about a rebase or history edit that was killed after moving refs, when one
    /// was pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub interrupted_rewrite: Option<InterruptedRewriteJson>,
    /// The GitButler workspace `HEAD` is on, when it is on one. `head` is then the workspace
    /// commit, and `commits` and `ahead` leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub gitbutler_workspace: Option<GitButlerWorkspaceJson>,
    /// Non-fatal warnings emitted while scanning the worktree (for example unreadable directories).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub warnings: Vec<crate::diagnostics::WarningRecord>,

    // Human-only state, not part of the JSON schema.
    #[serde(skip)]
    #[schemars(skip)]
    header: StatusHeader,
    #[serde(skip)]
    #[schemars(skip)]
    commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    #[schemars(skip)]
    staged_entries: Vec<DiffEntry>,
    #[serde(skip)]
    #[schemars(skip)]
    unstaged_entries: Vec<DiffEntry>,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
    #[serde(skip)]
    #[schemars(skip)]
    show_counts: bool,
    #[serde(skip)]
    #[schemars(skip)]
    standing: BranchStanding,
    #[serde(skip)]
    #[schemars(skip)]
    conflict_groups: Vec<ConflictGroup>,
    #[serde(skip)]
    #[schemars(skip)]
    guidance: StatusGuidance,
    #[serde(skip)]
    #[schemars(skip)]
    integration_hint: Option<StatusIntegrationHint>,
    #[serde(skip)]
    #[schemars(skip)]
    short_format: bool,
    #[serde(skip)]
    #[schemars(skip)]
    short_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    interrupted_rewrite_lock_holder: Option<LockHolderJson>,
    /// Rows for the workspace's merged heads, in the same order as `gitbutler_workspace.heads`.
    #[serde(skip)]
    #[schemars(skip)]
    workspace_rows: Vec<CommitSummary>,
}

/// A GitButler workspace in `grit status --json`: GitButler's merge of the applied branches.
#[derive(Serialize, JsonSchema)]
pub struct GitButlerWorkspaceJson {
    /// The workspace commit at the tip of `gitbutler/workspace` (full oid).
    pub commit: String,
    /// The heads merged into the workspace, in its parent order.
    pub heads: Vec<WorkspaceHeadJson>,
}

/// One head merged into a GitButler workspace.
#[derive(Serialize, JsonSchema)]
pub struct WorkspaceHeadJson {
    /// The head commit (full oid).
    pub commit: String,
    /// Local branches that point at it, sorted; empty when none does.
    pub branches: Vec<String>,
    /// The head commit's subject.
    pub subject: String,
    /// Commits on this head not on `target`, or `null` when counts were skipped or there is no
    /// target.
    pub ahead: Option<usize>,
}

/// Recovery of an interrupted rebase or history edit, in `--json` output.
#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterruptedRewriteJson {
    /// Refs had already moved: the checkout was completed and the rewrite stands.
    FinishedCheckout,
    /// The rewrite was undone: refs, index and files are back to their earlier state.
    RolledBack,
    /// Another process holds `index.lock`; recovery runs on a later command.
    Pending,
}

impl From<InterruptedRewrite> for InterruptedRewriteJson {
    fn from(outcome: InterruptedRewrite) -> Self {
        match outcome {
            InterruptedRewrite::FinishedCheckout => Self::FinishedCheckout,
            InterruptedRewrite::RolledBack => Self::RolledBack,
            InterruptedRewrite::Pending => Self::Pending,
        }
    }
}

impl InterruptedRewriteJson {
    /// One line for human and Markdown output; `lock` is how the index lock path is shown.
    fn sentence(self, lock: &str, holder: Option<&LockHolderJson>) -> String {
        match self {
            Self::FinishedCheckout => "Finished the checkout of an interrupted rebase or history edit; HEAD stays on the rewritten commits.".to_owned(),
            Self::RolledBack => "Rolled back an interrupted rebase or history edit; refs, index and files are back to where they were.".to_owned(),
            Self::Pending => match holder {
                Some(h) if h.running => format!(
                    "An interrupted rebase or history edit is pending: process {} holds {lock}. Run grit status again once it exits.",
                    h.pid
                ),
                Some(h) => format!(
                    "An interrupted rebase or history edit is pending: {lock} is held but process {} has exited; run grit status again to recover.",
                    h.pid
                ),
                None => format!(
                    "An interrupted rebase or history edit is pending: {lock} is held and no grit process id was recorded; the lock may be stale (another tool or core.lockfilePid=false). Run grit status again once nothing else is using the repository."
                ),
            },
        }
    }
}

fn conflict_json(c: &Conflict) -> ConflictJson {
    ConflictJson {
        path: c.path.clone(),
        kind: c.kind.as_str().to_owned(),
    }
}

fn operation_json(op: &grit_lib::state::Operation) -> OperationJson {
    OperationJson {
        kind: in_progress_label(op.kind),
        next_steps: op.next_steps.iter().filter_map(NextStep::command).collect(),
        alternatives: op
            .alternatives
            .iter()
            .filter_map(NextStep::command)
            .collect(),
    }
}

/// Upstream ahead/behind in `--json` / `--markdown` output.
#[derive(Serialize, JsonSchema)]
pub struct StatusUpstreamJson {
    /// Short upstream ref name (`origin/topic`).
    pub r#ref: String,
    /// Commits on this branch not on the upstream, or `null` when counts were skipped.
    pub ahead: Option<usize>,
    /// Commits on the upstream not on this branch, or `null` when counts were skipped.
    pub behind: Option<usize>,
}

impl HumanRender for StatusOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.short_format {
            self.render_short_human(w)?;
            return Ok(());
        }
        if let Some(outcome) = self.interrupted_rewrite {
            outln!(
                w,
                "{}",
                outcome.sentence(
                    ".git/index.lock",
                    self.interrupted_rewrite_lock_holder.as_ref()
                )
            );
            outln!(w);
        }
        self.render_header(w, ctx)?;
        self.render_changes(w)?;
        self.render_hints(w)?;
        self.render_stash_footer(w)?;
        Ok(())
    }
}

impl MarkdownRender for StatusOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.short_format {
            self.render_short_human(w)?;
            return Ok(());
        }
        if let Some(outcome) = self.interrupted_rewrite {
            outln!(
                w,
                "> {}",
                outcome.sentence(
                    "`.git/index.lock`",
                    self.interrupted_rewrite_lock_holder.as_ref()
                )
            );
            outln!(w);
        }
        self.render_markdown_header(w, ctx)?;
        self.render_markdown_changes(w)?;
        self.render_markdown_hints(w)?;
        self.render_stash_footer(w)?;
        Ok(())
    }
}

impl StatusOutcome {
    fn render_short_human(&self, w: &mut dyn Write) -> Result<()> {
        for line in &self.short_lines {
            outln!(w, "{line}");
        }
        self.render_stash_footer(w)
    }

    fn render_stash_footer(&self, w: &mut dyn Write) -> Result<()> {
        if self.stash_count > 0 {
            let noun = if self.stash_count == 1 {
                "entry"
            } else {
                "entries"
            };
            outln!(w, "{} stash {noun}", self.stash_count);
        }
        Ok(())
    }

    fn ignored_paths(&self) -> &[String] {
        self.ignored.as_deref().unwrap_or(&[])
    }

    /// Trunk or upstream ref name used in the status header line.
    fn header_comparison_name(&self) -> &str {
        StatusHeader::primary_comparison_name(&self.standing).unwrap_or("")
    }

    fn render_upstream_line_human(&self, w: &mut dyn Write) -> Result<()> {
        if !self.show_counts {
            return Ok(());
        }
        let Some(up) = self.upstream.as_ref() else {
            return Ok(());
        };
        if self.target.is_none() {
            return Ok(());
        }
        if up.ahead.is_some() || up.behind.is_some() {
            outln!(
                w,
                "  ↑{} ↓{} {}",
                up.ahead.unwrap_or(0),
                up.behind.unwrap_or(0),
                up.r#ref
            );
        }
        Ok(())
    }

    fn render_upstream_line_markdown(&self, w: &mut dyn Write) -> Result<()> {
        if !self.show_counts {
            return Ok(());
        }
        let Some(up) = &self.upstream else {
            return Ok(());
        };
        if self.target.is_none() {
            return Ok(());
        }
        if up.ahead.is_some() || up.behind.is_some() {
            outln!(
                w,
                "↑**{}** ↓**{}** `{}`",
                up.ahead.unwrap_or(0),
                up.behind.unwrap_or(0),
                up.r#ref
            );
        }
        Ok(())
    }

    fn integration_update_hint(&self) -> Option<&StatusIntegrationHint> {
        self.integration_hint.as_ref()
    }

    fn render_header(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let base = self.header_comparison_name();
        let now = ctx.now.unix_timestamp();
        if let (Some(workspace), StatusHeader::Branch(_)) =
            (&self.gitbutler_workspace, &self.header)
        {
            return self.render_workspace_human(w, workspace, now);
        }
        match &self.header {
            StatusHeader::Branch(StandingHeader::Compared(Standing::Ahead)) => {
                outln!(
                    w,
                    "On {branch}  ·  {} ahead of {base}",
                    self.ahead.unwrap_or(0)
                );
                self.render_upstream_line_human(w)?;
                outln!(w);
                self.render_ahead_shortlog(w, now)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Behind)) => {
                outln!(
                    w,
                    "On {branch}  ·  {} behind {base}",
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_human(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Diverged)) => {
                outln!(
                    w,
                    "On {branch}  ·  {} ahead, {} behind {base}",
                    self.ahead.unwrap_or(0),
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_human(w)?;
                outln!(w);
                self.render_ahead_shortlog(w, now)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Even)) => {
                outln!(w, "On {branch}  ·  even with {base}");
                self.render_upstream_line_human(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) if base.is_empty() => {
                outln!(w, "On {branch}");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) => {
                outln!(w, "On {branch}  ·  {base}");
                self.render_upstream_line_human(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::NoTarget) => {
                outln!(w, "On {branch}");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Unborn) => {
                outln!(w, "On {branch} — no commits yet");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Detached) => {
                let short = self.head.as_deref().map(short_hex).unwrap_or_default();
                outln!(w, "Detached at {short}");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Invalid) => {
                outln!(w, "HEAD is in an unknown state");
                outln!(w);
            }
            StatusHeader::MergeInProgress => {
                outln!(w, "On {branch}  ·  merging — resolve conflicts");
                outln!(w);
            }
            StatusHeader::RebaseInProgress { branch, onto } => {
                let branch = branch.as_deref().unwrap_or("detached HEAD");
                match onto {
                    Some(onto) => outln!(w, "Rebasing {branch} onto {onto}"),
                    None => outln!(w, "Rebasing {branch}"),
                }
                outln!(w);
            }
            StatusHeader::OtherInProgress => {
                outln!(w, "On {branch}  ·  operation in progress");
                outln!(w);
            }
        }
        Ok(())
    }

    fn render_changes(&self, w: &mut dyn Write) -> Result<()> {
        let display = self.path_display.as_ref();
        let ignored = self.ignored_paths();
        if self.clean {
            outln!(w, "Nothing to commit — working tree clean.");
            ui::print_ignored(w, ignored, display)?;
            return Ok(());
        }
        ui::print_change_group(w, "Staged", &self.staged_entries, display)?;
        ui::print_change_group(w, "Changed (not staged)", &self.unstaged_entries, display)?;
        if !self.conflict_groups.is_empty() {
            self.render_conflicts_by_kind(w, display)?;
        }
        ui::print_untracked(w, &self.untracked, display)?;
        crate::embedded_gitlink::emit_embedded_repository_status_notes(
            crate::output::OutputMode::Human,
            &self.embedded_repositories,
        );
        ui::print_ignored(w, ignored, display)?;
        Ok(())
    }

    fn render_conflicts_by_kind(
        &self,
        w: &mut dyn Write,
        display: Option<&PathDisplayOptions>,
    ) -> Result<()> {
        outln!(w, "Conflicts");
        for group in &self.conflict_groups {
            outln!(w, "  {} ({})", kind_label(&group.kind), group.paths.len());
            for path in &group.paths {
                let path = display
                    .map(|d| d.format_path_for_human(path))
                    .unwrap_or_else(|| path.clone());
                outln!(w, "    {path}");
            }
        }
        outln!(w);
        Ok(())
    }

    fn render_hints(&self, w: &mut dyn Write) -> Result<()> {
        match &self.guidance {
            StatusGuidance::Operation { steps, alternative } => {
                let steps: Vec<_> = steps.iter().filter_map(NextStep::command).collect();
                outln!(w, "→ {}", steps.join("  ·  "));
                if let Some(alt) = alternative.as_ref().and_then(NextStep::command) {
                    outln!(w, "or {alt}");
                }
            }
            StatusGuidance::ResolveConflictsThenContinueMerge => {
                outln!(w, "→ resolve conflicts, then grit merge --continue");
            }
            StatusGuidance::ResolveConflictsThenStage => {
                outln!(
                    w,
                    "→ resolve conflicts, then grit add <file> to mark them resolved"
                );
            }
            StatusGuidance::ContinueMerge => {
                outln!(w, "→ grit merge --continue to finish the merge");
            }
            StatusGuidance::Actions(actions) if !actions.is_empty() => {
                let hints: Vec<String> = actions
                    .iter()
                    .filter_map(|action| match action {
                        StatusAction::DiscardWorktreeChanges => {
                            Some("grit restore <file> to discard worktree changes".to_owned())
                        }
                        StatusAction::Stage => Some("grit add <file> to stage".to_owned()),
                        StatusAction::Commit if self.gitbutler_workspace.is_some() => {
                            Some("but commit to commit (GitButler workspace)".to_owned())
                        }
                        StatusAction::Commit => {
                            Some("grit commit \"message\" to commit".to_owned())
                        }
                        StatusAction::Pull { .. } => self
                            .integration_update_hint()
                            .map(|hint| integration_hint_command(hint, false)),
                    })
                    .collect();
                if !hints.is_empty() {
                    outln!(w, "→ {}", hints.join("  ·  "));
                }
            }
            StatusGuidance::Actions(_) => {}
        }
        Ok(())
    }

    /// The first line of a workspace header: how many branches, and how far behind the target.
    fn workspace_summary(
        &self,
        workspace: &GitButlerWorkspaceJson,
        code: fn(&str) -> String,
    ) -> String {
        let count = workspace.heads.len();
        let mut line = format!(
            "{count} branch{} applied",
            if count == 1 { "" } else { "es" }
        );
        if let (Some(behind), Some(target)) = (self.behind.filter(|&n| n > 0), &self.target) {
            line.push_str(&format!("  ·  {behind} behind {}", code(target)));
        }
        line
    }

    fn render_workspace_human(
        &self,
        w: &mut dyn Write,
        workspace: &GitButlerWorkspaceJson,
        now: i64,
    ) -> Result<()> {
        outln!(
            w,
            "On GitButler workspace  ·  {}",
            self.workspace_summary(workspace, str::to_owned)
        );
        outln!(w);
        let names: Vec<String> = workspace.heads.iter().map(workspace_head_label).collect();
        let name_w = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
        let reserved = vec![name_w + 2; names.len()];
        let opts = ui::CommitRowOptions {
            reserved: &reserved,
            ..ui::CommitRowOptions::default()
        };
        for (name, row) in names
            .iter()
            .zip(ui::commit_rows_with(&self.workspace_rows, now, &opts))
        {
            outln!(w, "  {name:<name_w$}{row}");
        }
        outln!(w);
        Ok(())
    }

    fn render_workspace_markdown(
        &self,
        w: &mut dyn Write,
        workspace: &GitButlerWorkspaceJson,
    ) -> Result<()> {
        let code = |s: &str| format!("`{s}`");
        outln!(
            w,
            "On a **GitButler workspace** · {}",
            self.workspace_summary(workspace, |s| format!("`{s}`"))
                .replace("  ·  ", " · ")
        );
        outln!(w);
        for head in &workspace.heads {
            let ahead = head
                .ahead
                .map(|n| format!(" · {n} ahead"))
                .unwrap_or_default();
            outln!(
                w,
                "- **{}** `{}` {}{ahead}",
                code(&workspace_head_label(head)),
                short_hex(&head.commit),
                head.subject
            );
        }
        outln!(w);
        Ok(())
    }

    fn render_ahead_shortlog(&self, w: &mut dyn Write, now: i64) -> Result<()> {
        for row in ui::commit_rows(&self.commit_rows, now)
            .iter()
            .take(SHORTLOG_LIMIT)
        {
            outln!(w, "{row}");
        }
        if self.ahead.unwrap_or(0) > SHORTLOG_LIMIT {
            outln!(
                w,
                "  … and {} more",
                self.ahead.unwrap_or(0) - SHORTLOG_LIMIT
            );
        }
        outln!(w);
        Ok(())
    }

    fn render_markdown_hints(&self, w: &mut dyn Write) -> Result<()> {
        match &self.guidance {
            StatusGuidance::Operation { steps, alternative } => {
                let steps: Vec<_> = steps.iter().filter_map(NextStep::command).collect();
                outln!(w, "→ `{}`", steps.join("`  ·  `"));
                if let Some(alt) = alternative.as_ref().and_then(NextStep::command) {
                    outln!(w, "or `{alt}`");
                }
            }
            StatusGuidance::ResolveConflictsThenContinueMerge => {
                outln!(w, "→ resolve conflicts, then grit merge --continue");
            }
            StatusGuidance::ResolveConflictsThenStage => {
                outln!(
                    w,
                    "→ resolve conflicts, then grit add `PATH` to mark them resolved"
                );
            }
            StatusGuidance::ContinueMerge => {
                outln!(w, "→ grit merge --continue to finish the merge");
            }
            StatusGuidance::Actions(actions) => {
                let hints: Vec<String> = actions
                    .iter()
                    .filter_map(|action| match action {
                        StatusAction::DiscardWorktreeChanges => None,
                        StatusAction::Stage => Some("grit add `PATH` to stage".to_owned()),
                        StatusAction::Commit if self.gitbutler_workspace.is_some() => {
                            Some("`but commit` to commit (GitButler workspace)".to_owned())
                        }
                        StatusAction::Commit => {
                            Some("grit commit `\"message\"` to commit".to_owned())
                        }
                        StatusAction::Pull { .. } => self
                            .integration_update_hint()
                            .map(|hint| integration_hint_command(hint, true)),
                    })
                    .collect();
                if !hints.is_empty() {
                    outln!(w, "→ {}", hints.join("  ·  "));
                }
            }
        }
        Ok(())
    }

    fn render_markdown_header(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let base = self.header_comparison_name();
        if let (Some(workspace), StatusHeader::Branch(_)) =
            (&self.gitbutler_workspace, &self.header)
        {
            return self.render_workspace_markdown(w, workspace);
        }
        match &self.header {
            StatusHeader::Branch(StandingHeader::Compared(Standing::Ahead)) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** ahead of `{base}`",
                    self.ahead.unwrap_or(0)
                );
                self.render_upstream_line_markdown(w)?;
                self.render_markdown_ahead_shortlog(w, ctx)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Behind)) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** behind `{base}`",
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_markdown(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Diverged)) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** ahead, **{}** behind `{base}`",
                    self.ahead.unwrap_or(0),
                    self.behind.unwrap_or(0)
                );
                self.render_upstream_line_markdown(w)?;
                self.render_markdown_ahead_shortlog(w, ctx)?;
            }
            StatusHeader::Branch(StandingHeader::Compared(Standing::Even)) => {
                outln!(w, "On **`{branch}`** · even with `{base}`");
                self.render_upstream_line_markdown(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) if base.is_empty() => {
                outln!(w, "On **`{branch}`**");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::ComparedNoCounts) => {
                outln!(w, "On **`{branch}`** · `{base}`");
                self.render_upstream_line_markdown(w)?;
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::NoTarget) => {
                outln!(w, "On **`{branch}`**");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Unborn) => {
                outln!(w, "On **`{branch}`** — no commits yet");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Detached) => {
                let short = self.head.as_deref().map(short_hex).unwrap_or_default();
                outln!(w, "Detached at `{short}`");
                outln!(w);
            }
            StatusHeader::Branch(StandingHeader::Invalid) => {
                outln!(w, "HEAD is in an unknown state");
                outln!(w);
            }
            StatusHeader::MergeInProgress => {
                outln!(w, "On **`{branch}`** · merging — resolve conflicts");
                outln!(w);
            }
            StatusHeader::RebaseInProgress { branch, onto } => {
                let branch = branch
                    .as_deref()
                    .map_or_else(|| "detached HEAD".to_owned(), |b| format!("**`{b}`**"));
                match onto {
                    Some(onto) => outln!(w, "Rebasing {branch} onto `{onto}`"),
                    None => outln!(w, "Rebasing {branch}"),
                }
                outln!(w);
            }
            StatusHeader::OtherInProgress => {
                outln!(w, "On **`{branch}`** · operation in progress");
                outln!(w);
            }
        }
        Ok(())
    }

    fn render_markdown_ahead_shortlog(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if !self.commit_rows.is_empty() {
            outln!(w);
            markdown::print_commit_list(
                w,
                &self.commit_rows[..self.commit_rows.len().min(SHORTLOG_LIMIT)],
                ctx,
            )?;
            if self.ahead.unwrap_or(0) > SHORTLOG_LIMIT {
                outln!(w);
                outln!(
                    w,
                    "… and {} more commit(s).",
                    self.ahead.unwrap_or(0) - SHORTLOG_LIMIT
                );
            }
        }
        outln!(w);
        Ok(())
    }

    fn render_markdown_changes(&self, w: &mut dyn Write) -> Result<()> {
        let display = self.path_display.as_ref();
        let ignored = self.ignored_paths();
        if self.clean {
            outln!(w, "Nothing to commit — working tree clean.");
            markdown::print_ignored(w, ignored, display)?;
            return Ok(());
        }
        markdown::print_change_section(w, "Staged", &self.staged_entries, display)?;
        markdown::print_change_section(w, "Changed (not staged)", &self.unstaged_entries, display)?;
        if !self.conflict_groups.is_empty() {
            self.render_markdown_conflicts_by_kind(w, display)?;
        }
        markdown::print_untracked(w, &self.untracked, display)?;
        crate::embedded_gitlink::emit_embedded_repository_status_notes(
            crate::output::OutputMode::Markdown,
            &self.embedded_repositories,
        );
        markdown::print_ignored(w, ignored, display)?;
        Ok(())
    }

    fn render_markdown_conflicts_by_kind(
        &self,
        w: &mut dyn Write,
        display: Option<&PathDisplayOptions>,
    ) -> Result<()> {
        outln!(w, "## Conflicts");
        for group in &self.conflict_groups {
            outln!(
                w,
                "- **{}** ({} paths)",
                kind_label(&group.kind),
                group.paths.len()
            );
            for path in &group.paths {
                let path = display
                    .map(|d| d.format_path_for_human(path))
                    .unwrap_or_else(|| path.clone());
                let shown = markdown::format_inline_code(&path);
                outln!(w, "  - {shown}");
            }
        }
        outln!(w);
        Ok(())
    }
}

/// Abbreviate a full hex oid to the 7-char short form used in human output.
fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

fn kind_label(kind: &ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothModified => "both modified",
        ConflictKind::BothAdded => "both added",
        ConflictKind::BothDeleted => "both deleted",
        ConflictKind::AddedByUs => "added by us",
        ConflictKind::AddedByThem => "added by them",
        ConflictKind::DeletedByUs => "deleted by us",
        ConflictKind::DeletedByThem => "deleted by them",
        ConflictKind::RenameRename(_) => "renamed differently",
        ConflictKind::DirectoryRenameMoved(_) => "moved with its directory",
        ConflictKind::DirectoryRenameSplit => "directory split up",
        ConflictKind::DirectoryRenameBlocked(_) => "directory move blocked",
    }
}

/// CLI spelling for [`UntrackedMode`] (`grit status --untracked`).
#[derive(Clone, Copy, Debug)]
pub enum UntrackedListing {
    No,
    Normal,
    All,
}

impl std::str::FromStr for UntrackedListing {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl UntrackedListing {
    /// Parse `no`, `normal`, or `all` (case-insensitive).
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.to_ascii_lowercase().as_str() {
            "no" => Ok(Self::No),
            "normal" => Ok(Self::Normal),
            "all" => Ok(Self::All),
            _ => Err(format!(
                "invalid untracked mode {raw:?} (expected no, normal, or all)"
            )),
        }
    }

    fn into_lib(self) -> UntrackedMode {
        match self {
            Self::No => UntrackedMode::No,
            Self::Normal => UntrackedMode::Normal,
            Self::All => UntrackedMode::All,
        }
    }
}

/// CLI arguments for `grit status` (also the default when no subcommand is given).
#[derive(Debug, Clone, Default, Args)]
pub struct StatusCliArgs {
    /// How untracked paths are listed: `normal` (default) collapses wholly untracked
    /// directories; `all` lists every file; `no` omits untracked paths. When omitted,
    /// uses `status.showUntrackedFiles` from config.
    #[arg(long = "untracked", value_name = "MODE")]
    pub untracked: Option<UntrackedListing>,
    /// Skip ahead/behind rev-list walks (also when `status.aheadBehind=false`).
    #[arg(long = "no-ahead-behind")]
    pub no_ahead_behind: bool,
    /// List ignored paths (traditional mode; same set as `git status --ignored`).
    #[arg(long)]
    pub ignored: bool,
    /// One line per path: two-letter index/worktree code plus path (`??` / `!!` for untracked / ignored).
    #[arg(short = 's', long)]
    pub short: bool,
    #[command(flatten)]
    pub pathspecs: PathspecArgs,
}

pub fn run(args: &StatusCliArgs) -> Result<StatusOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let config = repo.config().context("could not load config")?;
    let mut status_opts = status_options_from_config(&config)?;
    if let Some(mode) = args.untracked {
        status_opts.untracked = mode.into_lib();
    }
    if !args.pathspecs.paths.is_empty() || args.pathspecs.pathspec_from_file.is_some() {
        let resolved = args.pathspecs.resolve(&repo)?;
        status_opts.pathspecs = resolved.specs;
    }
    if args.ignored {
        status_opts.ignored = IgnoredMode::Traditional;
    }
    status_opts.count_stash = true;
    let show_counts = !args.no_ahead_behind && status_ahead_behind_enabled(&config);
    let model = status(&repo, &status_opts, &mut NullProgress, &NeverCancel)
        .context("could not compute status")?;

    let operations = detect_in_progress(repo.git_dir());
    let merging = model.state.merge_in_progress;
    let conflicts: Vec<ConflictJson> = model.conflict_details.iter().map(conflict_json).collect();
    let conflict_groups = model.conflict_groups();
    let operation = model.operation.as_ref().map(operation_json);

    let standing = BranchStanding::compute(
        &repo,
        &model.head,
        StandingOptions {
            newest_ahead_limit: SHORTLOG_LIMIT,
            counts: show_counts,
        },
    )
    .context("could not resolve branch standing")?;
    let header = StatusHeader::resolve(&standing, &model.state, &operations);
    let branch = match &header {
        StatusHeader::RebaseInProgress { branch, .. } => branch.clone(),
        _ => standing.branch.clone(),
    };
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let newest_ahead = standing
        .target
        .as_ref()
        .map(|t| t.newest_ahead.as_slice())
        .unwrap_or(&[]);
    let mut commit_rows = context::commit_summaries(&repo, newest_ahead)?;
    // On a GitButler workspace its merge commit is bookkeeping: the applied branches stand in
    // for it, and it is left out of the commits ahead.
    let workspace = GitButlerWorkspace::detect(&repo).context("could not read HEAD")?;
    if let Some(workspace) = &workspace {
        commit_rows.retain(|c| c.oid != workspace.commit);
    }
    let workspace_rows = match &workspace {
        Some(workspace) => context::commit_summaries(&repo, &workspace.head_commits())?,
        None => Vec::new(),
    };
    let gitbutler_workspace = match &workspace {
        Some(workspace) => Some(workspace_json(
            &repo,
            workspace,
            &workspace_rows,
            standing.target.as_ref().filter(|_| show_counts),
        )?),
        None => None,
    };
    let ahead_oids: Vec<_> = commit_rows.iter().map(|c| c.oid).collect();
    let commits = if ahead_oids.is_empty() {
        Vec::new()
    } else {
        crate::commits_json::CommitJsonLoader::for_listed_commits().load(&repo, &ahead_oids, now)?
    };

    let staged: Vec<ChangeJson> = model.staged.iter().map(change_json).collect();
    let unstaged: Vec<ChangeJson> = model.unstaged.iter().map(change_json).collect();
    let clean = model.is_clean();
    let guidance = model.guidance(&standing);

    let path_display = PathDisplayOptions::from_repo(&repo).ok();
    let short_lines = if args.short {
        format_short_lines(&model, path_display.as_ref())
    } else {
        Vec::new()
    };
    let ignored_json = args.ignored.then(|| model.ignored.clone());

    let standing_for_outcome = standing.clone();
    let integration_hint = standing.branch.as_deref().and_then(|branch| {
        grit_lib::refs::branch_tracking::status_integration_hint(&standing, Some((&repo, branch)))
    });
    let interrupted_rewrite_lock_holder = model
        .interrupted_rewrite
        .as_ref()
        .filter(|outcome| matches!(outcome, InterruptedRewrite::Pending))
        .and_then(|_| {
            let lock_path = repo.git_dir().join("index.lock");
            grit_lib::lockfile::LockHolder::read(&lock_path).map(|h| LockHolderJson {
                pid: h.pid,
                running: h.running,
            })
        });
    Ok(StatusOutcome {
        warnings: sink.warnings(),
        branch,
        detached: matches!(model.head, HeadState::Detached { .. }),
        head: model.head.oid().map(ObjectId::to_hex),
        target: standing.target.as_ref().map(|t| t.name.clone()),
        ahead: standing
            .target
            .as_ref()
            .and_then(|t| t.ahead)
            .map(|n| match &workspace {
                Some(_) => n.saturating_sub(1),
                None => n,
            }),
        behind: standing.target.as_ref().and_then(|t| t.behind),
        upstream: standing.upstream.as_ref().map(tracked_to_upstream_json),
        commits,
        staged,
        unstaged,
        untracked: model.untracked,
        embedded_repositories: model.embedded_repositories,
        clean,
        merging,
        in_progress: operations.into_iter().map(in_progress_label).collect(),
        conflicts,
        operation,
        ignored: ignored_json,
        stash_count: model.stash_count,
        interrupted_rewrite: model.interrupted_rewrite.map(InterruptedRewriteJson::from),
        gitbutler_workspace,
        header,
        commit_rows,
        staged_entries: model.staged,
        unstaged_entries: model.unstaged,
        path_display,
        show_counts,
        standing: standing_for_outcome,
        conflict_groups,
        guidance,
        integration_hint,
        short_format: args.short,
        short_lines,
        interrupted_rewrite_lock_holder,
        workspace_rows,
    })
}

/// `grit status --json`'s view of `workspace`; `rows` are its heads' summaries, in order.
fn workspace_json(
    repo: &grit_lib::repo::Repository,
    workspace: &GitButlerWorkspace,
    rows: &[CommitSummary],
    target: Option<&TrackedComparison>,
) -> Result<GitButlerWorkspaceJson> {
    let heads = workspace
        .heads
        .iter()
        .zip(rows)
        .map(|(head, row)| {
            let ahead = target
                .map(|target| TipComparison::compute(repo, head.commit, target.oid, 0))
                .transpose()
                .context("could not compare a workspace branch with the target")?
                .map(|comparison| comparison.ahead);
            Ok(WorkspaceHeadJson {
                commit: head.commit.to_hex(),
                branches: head.branches.clone(),
                subject: row.subject.clone(),
                ahead,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(GitButlerWorkspaceJson {
        commit: workspace.commit.to_hex(),
        heads,
    })
}

/// The branch names at a workspace head, or `(no branch)` when none points there.
fn workspace_head_label(head: &WorkspaceHeadJson) -> String {
    if head.branches.is_empty() {
        "(no branch)".to_owned()
    } else {
        head.branches.join(", ")
    }
}

fn integration_hint_command(hint: &StatusIntegrationHint, markdown: bool) -> String {
    let ref_name = if markdown {
        format!("`{}`", hint.ref_name)
    } else {
        hint.ref_name.clone()
    };
    match hint.action {
        StatusIntegrationAction::Pull => format!("grit pull to update from {ref_name}"),
        StatusIntegrationAction::Merge => format!("grit merge {ref_name}"),
    }
}

fn format_short_lines(
    model: &grit_lib::porcelain::status::StatusModel,
    display: Option<&PathDisplayOptions>,
) -> Vec<String> {
    short_status_lines(model)
        .into_iter()
        .map(|line| {
            line.render_with(|path| {
                display
                    .map(|d| d.format_path_for_human(path))
                    .unwrap_or_else(|| path.to_owned())
            })
        })
        .collect()
}

fn in_progress_label(op: InProgressOperation) -> String {
    match op {
        InProgressOperation::Merge => "merge".to_owned(),
        InProgressOperation::RebaseInteractive => "rebase-interactive".to_owned(),
        InProgressOperation::Rebase => "rebase".to_owned(),
        InProgressOperation::CherryPick => "cherry-pick".to_owned(),
        InProgressOperation::Revert => "revert".to_owned(),
        InProgressOperation::Bisect => "bisect".to_owned(),
        InProgressOperation::Am => "am".to_owned(),
        InProgressOperation::RewriteApply => "rewrite-apply".to_owned(),
    }
}

fn tracked_to_upstream_json(tracked: &TrackedComparison) -> StatusUpstreamJson {
    StatusUpstreamJson {
        r#ref: tracked.name.clone(),
        ahead: tracked.ahead,
        behind: tracked.behind,
    }
}
