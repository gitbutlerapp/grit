//! `grit status` (and bare `grit`) — the dashboard: where you are, the commits
//! you're ahead by, what's changed, and what to do next.

use anyhow::{Context, Result};
use grit_lib::diff::DiffEntry;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::status::{status, status_options_from_config, Conflict, UntrackedMode};
use grit_lib::progress::NullProgress;
use grit_lib::refs::branch_tracking::{
    status_ahead_behind_enabled, BranchStanding, Standing, StandingHeader, StandingOptions,
    StatusHeader, TrackedComparison,
};
use grit_lib::state::{detect_in_progress, HeadState, InProgressOperation, NextStep};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context::{self, CommitSummary};
use crate::markdown;
use crate::output::{
    change_json, outln, ChangeJson, CommitJson, HumanRender, MarkdownRender, RenderContext,
};
use crate::pathspec_args::PathspecArgs;
use crate::ui::{self, PathDisplayOptions};
use clap::Args;

/// Maximum number of commits to list in the status shortlog before summarizing.
const SHORTLOG_LIMIT: usize = 10;

/// Expand Git-style `-u`, `-uno`, `-unormal`, and `-uall` only for status invocations.
pub fn normalize_status_argv(argv: &mut [String]) {
    let Some(range) = status_untracked_rewrite_range(argv) else {
        return;
    };
    for arg in argv[range].iter_mut() {
        let Some(rest) = arg.strip_prefix("-u") else {
            continue;
        };
        if rest.is_empty() {
            *arg = "--untracked=all".to_owned();
        } else if matches!(rest, "no" | "normal" | "all") {
            *arg = format!("--untracked={rest}");
        }
    }
}

fn status_untracked_rewrite_range(argv: &[String]) -> Option<std::ops::Range<usize>> {
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" {
            return None;
        }
        if let Some(n) = skip_global_flag(argv, i) {
            i += n;
            continue;
        }
        break;
    }
    if i >= argv.len() {
        return Some(1..1);
    }
    let start = match argv[i].as_str() {
        "status" | "st" => i + 1,
        cmd if is_grit_subcommand(cmd) => return None,
        _ => i,
    };
    let end = argv[start..]
        .iter()
        .position(|token| token == "--")
        .map_or(argv.len(), |pos| start + pos);
    Some(start..end)
}

fn skip_global_flag(argv: &[String], i: usize) -> Option<usize> {
    match argv[i].as_str() {
        "--json" | "--markdown" | "-v" | "-V" | "--version" => Some(1),
        "--filter" | "--threads" => argv.get(i + 1).map(|_| 2).or(Some(1)),
        s if s.starts_with("--filter=") || s.starts_with("--threads=") => Some(1),
        _ => None,
    }
}

#[cfg(test)]
mod argv_tests {
    use super::normalize_status_argv;

    #[test]
    fn normalize_skips_non_status_commands() {
        let mut argv = vec![
            "grit".into(),
            "add".into(),
            "-uno".into(),
            "file.txt".into(),
        ];
        normalize_status_argv(&mut argv);
        assert_eq!(argv[2], "-uno");
    }

    #[test]
    fn normalize_expands_status_u_and_uno() {
        let mut argv = vec![
            "grit".into(),
            "status".into(),
            "-u".into(),
            "keep.txt".into(),
        ];
        normalize_status_argv(&mut argv);
        assert_eq!(argv[2], "--untracked=all");
        assert_eq!(argv[3], "keep.txt");
        let mut argv2 = vec!["grit".into(), "st".into(), "-uno".into()];
        normalize_status_argv(&mut argv2);
        assert_eq!(argv2[2], "--untracked=no");
    }

    #[test]
    fn normalize_does_not_rewrite_pathspecs_after_double_dash() {
        for path in ["-uno", "-unormal", "-uall", "-u"] {
            let mut argv = vec!["grit".into(), "status".into(), "--".into(), path.into()];
            normalize_status_argv(&mut argv);
            assert_eq!(argv[3], path, "path token after -- must be preserved");
        }
        let mut argv = vec![
            "grit".into(),
            "status".into(),
            "-uno".into(),
            "--".into(),
            "keep.txt".into(),
        ];
        normalize_status_argv(&mut argv);
        assert_eq!(argv[2], "--untracked=no");
        assert_eq!(argv[4], "keep.txt");
    }
}

fn is_grit_subcommand(name: &str) -> bool {
    matches!(
        name,
        "init"
            | "clone"
            | "remote"
            | "fetch"
            | "pull"
            | "push"
            | "log"
            | "show"
            | "diff"
            | "status"
            | "st"
            | "shortlog"
            | "add"
            | "rm"
            | "mv"
            | "clean"
            | "restore"
            | "reset"
            | "commit"
            | "branch"
            | "tag"
            | "switch"
            | "merge"
            | "pick"
            | "revert"
            | "history"
            | "stash"
            | "config"
            | "fsck"
            | "gc"
            | "bundle"
            | "notes"
            | "cat"
            | "rev-parse"
            | "revs"
            | "worktree"
            | "auth"
            | "skill"
            | "completions"
            | "manager"
            | "upload-pack"
            | "receive-pack"
            | "help"
    )
}

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
    /// Suggested follow-up commands, in order.
    pub next_steps: Vec<String>,
    /// Mutually exclusive options (for example cancel the merge).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub alternatives: Vec<String>,
}

/// Result of `grit status`.
#[derive(Serialize, JsonSchema)]
pub struct StatusOutcome {
    /// Current branch (short name), or `null` when detached / invalid.
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
    conflict_entries: Vec<Conflict>,
    #[serde(skip)]
    #[schemars(skip)]
    next_steps: Vec<NextStep>,
    #[serde(skip)]
    #[schemars(skip)]
    alternative_steps: Vec<NextStep>,
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
        next_steps: op
            .next_steps
            .iter()
            .filter_map(NextStep::grit_command)
            .collect(),
        alternatives: op
            .alternatives
            .iter()
            .filter_map(NextStep::grit_command)
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
        self.render_header(w, ctx)?;
        self.render_changes(w)?;
        self.render_hints(w)?;
        Ok(())
    }
}

impl MarkdownRender for StatusOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        self.render_markdown_header(w, ctx)?;
        self.render_markdown_changes(w)?;
        self.render_markdown_hints(w)?;
        Ok(())
    }
}

impl StatusOutcome {
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

    /// Ref name for the `grit pull` hint (`pull` integrates the configured upstream).
    fn pull_update_ref(&self) -> Option<&str> {
        StatusHeader::pull_integration_ref(&self.standing)
    }

    fn render_header(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let base = self.header_comparison_name();
        let now = ctx.now.unix_timestamp();
        match self.header {
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
            StatusHeader::OtherInProgress => {
                outln!(w, "On {branch}  ·  operation in progress");
                outln!(w);
            }
        }
        Ok(())
    }

    fn render_changes(&self, w: &mut dyn Write) -> Result<()> {
        if self.clean {
            outln!(w, "Nothing to commit — working tree clean.");
            return Ok(());
        }
        let display = self.path_display.as_ref();
        ui::print_change_group(w, "Staged", &self.staged_entries, display)?;
        ui::print_change_group(w, "Changed (not staged)", &self.unstaged_entries, display)?;
        if !self.conflict_entries.is_empty() {
            self.render_conflicts_by_kind(w, display)?;
        }
        ui::print_untracked(w, &self.untracked, display)?;
        Ok(())
    }

    fn render_conflicts_by_kind(
        &self,
        w: &mut dyn Write,
        display: Option<&PathDisplayOptions>,
    ) -> Result<()> {
        use grit_lib::porcelain::status::ConflictKind;
        use std::collections::BTreeMap;

        let mut by_kind: BTreeMap<ConflictKind, Vec<&Conflict>> = BTreeMap::new();
        for c in &self.conflict_entries {
            by_kind.entry(c.kind).or_default().push(c);
        }
        outln!(w, "Conflicts");
        for (kind, entries) in by_kind {
            outln!(w, "  {} ({})", kind_label(kind), entries.len());
            for c in entries {
                let path = display
                    .map(|d| d.format_path_for_human(&c.path))
                    .unwrap_or_else(|| c.path.clone());
                outln!(w, "    {path}");
            }
        }
        Ok(())
    }

    fn render_hints(&self, w: &mut dyn Write) -> Result<()> {
        let sequential: Vec<_> = self
            .next_steps
            .iter()
            .filter_map(NextStep::grit_command)
            .collect();
        if !sequential.is_empty() {
            outln!(w, "→ {}", sequential.join("  ·  "));
            if let Some(alt) = self
                .alternative_steps
                .iter()
                .filter_map(NextStep::grit_command)
                .next()
            {
                outln!(w, "or {alt}");
            }
            return Ok(());
        }
        if !self.conflicts.is_empty() {
            outln!(w, "→ resolve conflicts, then grit commit \"message\"");
            return Ok(());
        }
        if self.merging {
            outln!(w, "→ grit commit \"message\" to finish the merge");
            return Ok(());
        }
        let mut hints: Vec<String> = Vec::new();
        if !self.unstaged_entries.is_empty() {
            hints.push("grit restore <file> to discard worktree changes".to_owned());
        }
        if !self.unstaged_entries.is_empty() || !self.untracked.is_empty() {
            hints.push("grit add <file> to stage".to_owned());
        }
        if !self.staged_entries.is_empty() {
            hints.push("grit commit \"message\" to commit".to_owned());
        }
        if let Some(base) = self.pull_update_ref() {
            hints.push(format!("grit pull to update from {base}"));
        }
        if !hints.is_empty() {
            outln!(w, "→ {}", hints.join("  ·  "));
        }
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
        let sequential: Vec<_> = self
            .next_steps
            .iter()
            .filter_map(NextStep::grit_command)
            .collect();
        if !sequential.is_empty() {
            outln!(w, "→ `{}`", sequential.join("`  ·  `"));
            if let Some(alt) = self
                .alternative_steps
                .iter()
                .filter_map(NextStep::grit_command)
                .next()
            {
                outln!(w, "or `{alt}`");
            }
            return Ok(());
        }
        if !self.conflicts.is_empty() {
            outln!(w, "→ resolve conflicts, then grit commit `\"message\"`");
            return Ok(());
        }
        if self.merging {
            outln!(w, "→ grit commit `\"message\"` to finish the merge");
            return Ok(());
        }
        let mut hints: Vec<String> = Vec::new();
        if !self.unstaged_entries.is_empty() || !self.untracked.is_empty() {
            hints.push("grit add `PATH` to stage".to_owned());
        }
        if !self.staged_entries.is_empty() {
            hints.push("grit commit `\"message\"` to commit".to_owned());
        }
        if let Some(base) = self.pull_update_ref() {
            hints.push(format!("grit pull to update from `{base}`"));
        }
        if !hints.is_empty() {
            outln!(w, "→ {}", hints.join("  ·  "));
        }
        Ok(())
    }

    fn render_markdown_header(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let base = self.header_comparison_name();
        match self.header {
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
        if self.clean {
            outln!(w, "Nothing to commit — working tree clean.");
            return Ok(());
        }
        let display = self.path_display.as_ref();
        markdown::print_change_section(w, "Staged", &self.staged_entries, display)?;
        markdown::print_change_section(w, "Changed (not staged)", &self.unstaged_entries, display)?;
        if !self.conflict_entries.is_empty() {
            self.render_markdown_conflicts_by_kind(w, display)?;
        }
        markdown::print_untracked(w, &self.untracked, display)?;
        Ok(())
    }

    fn render_markdown_conflicts_by_kind(
        &self,
        w: &mut dyn Write,
        display: Option<&PathDisplayOptions>,
    ) -> Result<()> {
        use grit_lib::porcelain::status::ConflictKind;
        use std::collections::BTreeMap;

        let mut by_kind: BTreeMap<ConflictKind, Vec<&Conflict>> = BTreeMap::new();
        for c in &self.conflict_entries {
            by_kind.entry(c.kind).or_default().push(c);
        }
        outln!(w, "### Conflicts");
        for (kind, entries) in by_kind {
            outln!(w, "- **{}** ({} paths)", kind_label(kind), entries.len());
            for c in entries {
                let path = display
                    .map(|d| d.format_path_for_human(&c.path))
                    .unwrap_or_else(|| c.path.clone());
                outln!(w, "  - `{path}`");
            }
        }
        Ok(())
    }
}

/// Abbreviate a full hex oid to the 7-char short form used in human output.
fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
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

fn kind_label(kind: grit_lib::porcelain::status::ConflictKind) -> &'static str {
    match kind {
        grit_lib::porcelain::status::ConflictKind::BothModified => "both modified",
        grit_lib::porcelain::status::ConflictKind::BothAdded => "both added",
        grit_lib::porcelain::status::ConflictKind::BothDeleted => "both deleted",
        grit_lib::porcelain::status::ConflictKind::AddedByUs => "added by us",
        grit_lib::porcelain::status::ConflictKind::AddedByThem => "added by them",
        grit_lib::porcelain::status::ConflictKind::DeletedByUs => "deleted by us",
        grit_lib::porcelain::status::ConflictKind::DeletedByThem => "deleted by them",
    }
}

/// CLI arguments for `grit status` (also the default when no subcommand is given).
#[derive(Debug, Clone, Default, Args)]
pub struct StatusCliArgs {
    /// How untracked paths are listed: `normal` (default) collapses wholly untracked
    /// directories; `all` lists every file; `no` omits untracked paths. When omitted,
    /// uses `status.showUntrackedFiles` from config. Git-style `-u`, `-uno`, `-unormal`,
    /// and `-uall` are accepted.
    #[arg(
        long = "untracked",
        value_name = "MODE",
        num_args = 0..=1,
        default_missing_value = "all"
    )]
    pub untracked: Option<UntrackedListing>,
    /// Skip ahead/behind rev-list walks (also when `status.aheadBehind=false`).
    #[arg(long = "no-ahead-behind")]
    pub no_ahead_behind: bool,
    #[command(flatten)]
    pub pathspecs: PathspecArgs,
}

pub fn run(args: &StatusCliArgs) -> Result<StatusOutcome> {
    let repo = context::discover()?;
    let config = repo.config().context("could not load config")?;
    let mut status_opts = status_options_from_config(&config)?;
    if let Some(mode) = args.untracked {
        status_opts.untracked = mode.into_lib();
    }
    if !args.pathspecs.paths.is_empty() || args.pathspecs.pathspec_from_file.is_some() {
        let resolved = args.pathspecs.resolve(&repo)?;
        status_opts.pathspecs = resolved.specs;
    }
    let show_counts = !args.no_ahead_behind && status_ahead_behind_enabled(&config);
    let model =
        status(&repo, &status_opts, &mut NullProgress).context("could not compute status")?;

    let operations = detect_in_progress(repo.git_dir());
    let merging = model.state.merge_in_progress;
    let conflict_entries = model.conflict_details.clone();
    let conflicts: Vec<ConflictJson> = conflict_entries.iter().map(conflict_json).collect();
    let operation = model.operation.as_ref().map(operation_json);
    let (next_steps, alternative_steps) = model
        .operation
        .as_ref()
        .map(|op| (op.next_steps.clone(), op.alternatives.clone()))
        .unwrap_or_else(|| (Vec::new(), Vec::new()));

    let standing = BranchStanding::compute(
        &repo,
        &model.head,
        StandingOptions {
            newest_ahead_limit: SHORTLOG_LIMIT,
            counts: show_counts,
        },
    )
    .context("could not resolve branch standing")?;
    let header = StatusHeader::resolve(&standing, merging, &operations);
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let newest_ahead = standing
        .target
        .as_ref()
        .map(|t| t.newest_ahead.as_slice())
        .unwrap_or(&[]);
    let commit_rows = context::commit_summaries(&repo, newest_ahead)?;
    let ahead_oids: Vec<_> = commit_rows.iter().map(|c| c.oid).collect();
    let commits = if ahead_oids.is_empty() {
        Vec::new()
    } else {
        crate::commits_json::CommitJsonLoader::new(&repo)?.load(&repo, &ahead_oids, now)?
    };

    let staged: Vec<ChangeJson> = model.staged.iter().map(change_json).collect();
    let unstaged: Vec<ChangeJson> = model.unstaged.iter().map(change_json).collect();
    let clean = model.staged.is_empty()
        && model.unstaged.is_empty()
        && model.untracked.is_empty()
        && conflict_entries.is_empty()
        && !merging;

    let path_display = PathDisplayOptions::from_repo(&repo).ok();

    let standing_for_outcome = standing.clone();
    Ok(StatusOutcome {
        branch: standing.branch,
        detached: matches!(model.head, HeadState::Detached { .. }),
        head: model.head.oid().map(ObjectId::to_hex),
        target: standing.target.as_ref().map(|t| t.name.clone()),
        ahead: standing.target.as_ref().and_then(|t| t.ahead),
        behind: standing.target.as_ref().and_then(|t| t.behind),
        upstream: standing.upstream.as_ref().map(tracked_to_upstream_json),
        commits,
        staged,
        unstaged,
        untracked: model.untracked,
        clean,
        merging,
        in_progress: operations.into_iter().map(in_progress_label).collect(),
        conflicts,
        operation,
        header,
        commit_rows,
        staged_entries: model.staged,
        unstaged_entries: model.unstaged,
        path_display,
        show_counts,
        standing: standing_for_outcome,
        conflict_entries,
        next_steps,
        alternative_steps,
    })
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
    }
}

fn tracked_to_upstream_json(tracked: &TrackedComparison) -> StatusUpstreamJson {
    StatusUpstreamJson {
        r#ref: tracked.name.clone(),
        ahead: tracked.ahead,
        behind: tracked.behind,
    }
}
