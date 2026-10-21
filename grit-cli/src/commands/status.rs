//! `grit status` (and bare `grit`) — the dashboard: where you are, the commits
//! you're ahead by, what's changed, and what to do next.

use anyhow::{Context, Result};
use grit_lib::diff::DiffEntry;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::status::Conflict;
use grit_lib::porcelain::status::{
    status, untracked_mode_from_config, StatusOptions, UntrackedMode,
};
use grit_lib::progress::NullProgress;
use grit_lib::refs::branch_tracking::{comparison_base, Standing, TipComparison};
use grit_lib::repo::Repository;
use grit_lib::state::{detect_in_progress, HeadState, InProgressOperation, NextStep};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context::{self, CommitSummary};
use crate::markdown;
use crate::output::{
    change_json, outln, ChangeJson, CommitJson, HumanRender, MarkdownRender, RenderContext,
};
use crate::ui::{self, PathDisplayOptions};

/// Maximum number of commits to list in the status shortlog before summarizing.
const SHORTLOG_LIMIT: usize = 10;

/// Which header line `grit status` shows (drives only the human rendering; the
/// JSON fields below carry the same information in a flat, stable form).
#[derive(Clone, Copy)]
enum HeaderKind {
    /// On a branch with a base: even with, ahead of, behind or diverged from it.
    Compared(Standing),
    NoTarget,
    Unborn,
    Detached,
    Invalid,
    MergeInProgress,
    OtherInProgress,
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
    /// Number of commits ahead of `target` (full count).
    pub ahead: usize,
    /// Number of commits on `target` not in the current branch (full count).
    pub behind: usize,
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
    header: HeaderKind,
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
    conflict_entries: Vec<Conflict>,
    #[serde(skip)]
    #[schemars(skip)]
    next_steps: Vec<NextStep>,
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
    }
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
    fn render_header(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let target = self.target.as_deref().unwrap_or_default();
        let now = ctx.now.unix_timestamp();
        match self.header {
            HeaderKind::Compared(Standing::Ahead) => {
                outln!(w, "On {branch}  ·  {} ahead of {target}", self.ahead);
                outln!(w);
                self.render_ahead_shortlog(w, now)?;
            }
            HeaderKind::Compared(Standing::Behind) => {
                outln!(w, "On {branch}  ·  {} behind {target}", self.behind);
                outln!(w);
            }
            HeaderKind::Compared(Standing::Diverged) => {
                outln!(
                    w,
                    "On {branch}  ·  {} ahead, {} behind {target}",
                    self.ahead,
                    self.behind
                );
                outln!(w);
                self.render_ahead_shortlog(w, now)?;
            }
            HeaderKind::Compared(Standing::Even) => {
                outln!(w, "On {branch}  ·  even with {target}");
                outln!(w);
            }
            HeaderKind::NoTarget => {
                outln!(w, "On {branch}");
                outln!(w);
            }
            HeaderKind::Unborn => {
                outln!(w, "On {branch} — no commits yet");
                outln!(w);
            }
            HeaderKind::Detached => {
                let short = self.head.as_deref().map(short_hex).unwrap_or_default();
                outln!(w, "Detached at {short}");
                outln!(w);
            }
            HeaderKind::Invalid => {
                outln!(w, "HEAD is in an unknown state");
                outln!(w);
            }
            HeaderKind::MergeInProgress => {
                outln!(w, "On {branch}  ·  merging — resolve conflicts");
                outln!(w);
            }
            HeaderKind::OtherInProgress => {
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
        if let Some(step) = self.next_steps.iter().find_map(NextStep::grit_command) {
            outln!(w, "→ {step}");
            for extra in self
                .next_steps
                .iter()
                .filter_map(NextStep::grit_command)
                .skip(1)
            {
                outln!(w, "→ {extra}");
            }
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
        if self.ahead == 0 && self.behind > 0 {
            let target = self.target.as_deref().unwrap_or("target");
            hints.push(format!("grit pull to update from {target}"));
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
        if self.ahead > SHORTLOG_LIMIT {
            outln!(w, "  … and {} more", self.ahead - SHORTLOG_LIMIT);
        }
        outln!(w);
        Ok(())
    }

    fn render_markdown_hints(&self, w: &mut dyn Write) -> Result<()> {
        if let Some(step) = self.next_steps.iter().find_map(NextStep::grit_command) {
            outln!(w, "→ `{step}`");
            for extra in self
                .next_steps
                .iter()
                .filter_map(NextStep::grit_command)
                .skip(1)
            {
                outln!(w, "→ `{extra}`");
            }
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
        if self.ahead == 0 && self.behind > 0 {
            let target = self.target.as_deref().unwrap_or("target");
            hints.push(format!("grit pull to update from `{target}`"));
        }
        if !hints.is_empty() {
            outln!(w, "→ {}", hints.join("  ·  "));
        }
        Ok(())
    }

    fn render_markdown_header(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let branch = self.branch.as_deref().unwrap_or_default();
        let target = self.target.as_deref().unwrap_or_default();
        match self.header {
            HeaderKind::Compared(Standing::Ahead) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** ahead of `{target}`",
                    self.ahead
                );
                self.render_markdown_ahead_shortlog(w, ctx)?;
            }
            HeaderKind::Compared(Standing::Behind) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** behind `{target}`",
                    self.behind
                );
                outln!(w);
            }
            HeaderKind::Compared(Standing::Diverged) => {
                outln!(
                    w,
                    "On **`{branch}`** · **{}** ahead, **{}** behind `{target}`",
                    self.ahead,
                    self.behind
                );
                self.render_markdown_ahead_shortlog(w, ctx)?;
            }
            HeaderKind::Compared(Standing::Even) => {
                outln!(w, "On **`{branch}`** · even with `{target}`");
                outln!(w);
            }
            HeaderKind::NoTarget => {
                outln!(w, "On **`{branch}`**");
                outln!(w);
            }
            HeaderKind::Unborn => {
                outln!(w, "On **`{branch}`** — no commits yet");
                outln!(w);
            }
            HeaderKind::Detached => {
                let short = self.head.as_deref().map(short_hex).unwrap_or_default();
                outln!(w, "Detached at `{short}`");
                outln!(w);
            }
            HeaderKind::Invalid => {
                outln!(w, "HEAD is in an unknown state");
                outln!(w);
            }
            HeaderKind::MergeInProgress => {
                outln!(w, "On **`{branch}`** · merging — resolve conflicts");
                outln!(w);
            }
            HeaderKind::OtherInProgress => {
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
            if self.ahead > SHORTLOG_LIMIT {
                outln!(w);
                outln!(w, "… and {} more commit(s).", self.ahead - SHORTLOG_LIMIT);
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
            self.render_markdown_conflicts(w, display)?;
        }
        markdown::print_untracked(w, &self.untracked, display)?;
        Ok(())
    }

    fn render_markdown_conflicts(
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

pub fn run(untracked: Option<UntrackedListing>) -> Result<StatusOutcome> {
    let repo = context::discover()?;
    let untracked_mode = match untracked {
        Some(mode) => mode.into_lib(),
        None => {
            let config = repo.config().context("could not load config")?;
            untracked_mode_from_config(&config)?
        }
    };
    let status_opts = StatusOptions {
        untracked: untracked_mode,
        ..StatusOptions::default()
    };
    let model =
        status(&repo, &status_opts, &mut NullProgress).context("could not compute status")?;

    let operations = detect_in_progress(repo.git_dir());
    let merging = model.state.merge_in_progress;
    let conflict_entries = model.conflict_details.clone();
    let conflicts: Vec<ConflictJson> = conflict_entries.iter().map(conflict_json).collect();
    let operation = model.operation.as_ref().map(operation_json);
    let next_steps = model
        .operation
        .as_ref()
        .map(|op| op.next_steps.clone())
        .unwrap_or_default();

    let summary = summarize_head(&repo, &model.head, in_progress_header(merging, &operations))?;
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let ahead_oids: Vec<_> = summary.ahead_commits.iter().map(|c| c.oid).collect();
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

    Ok(StatusOutcome {
        branch: summary.branch,
        detached: matches!(model.head, HeadState::Detached { .. }),
        head: model.head.oid().map(ObjectId::to_hex),
        target: summary.target,
        ahead: summary.ahead,
        behind: summary.behind,
        commits,
        staged,
        unstaged,
        untracked: model.untracked,
        clean,
        merging,
        in_progress: operations.into_iter().map(in_progress_label).collect(),
        conflicts,
        operation,
        header: summary.header,
        commit_rows: summary.ahead_commits,
        staged_entries: model.staged,
        unstaged_entries: model.unstaged,
        path_display,
        conflict_entries,
        next_steps,
    })
}

/// Where HEAD stands: the current branch, and how it compares with its base.
struct HeadSummary {
    branch: Option<String>,
    target: Option<String>,
    ahead: usize,
    behind: usize,
    ahead_commits: Vec<CommitSummary>,
    header: HeaderKind,
}

impl HeadSummary {
    fn new(branch: Option<String>, header: HeaderKind) -> Self {
        Self {
            branch,
            target: None,
            ahead: 0,
            behind: 0,
            ahead_commits: Vec::new(),
            header,
        }
    }
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

fn in_progress_header(merging: bool, operations: &[InProgressOperation]) -> Option<HeaderKind> {
    if merging || operations.contains(&InProgressOperation::Merge) {
        return Some(HeaderKind::MergeInProgress);
    }
    if !operations.is_empty() {
        return Some(HeaderKind::OtherInProgress);
    }
    None
}

/// `in_progress` replaces the comparison header on a branch with commits; unborn, detached
/// and invalid HEADs keep their own header.
fn summarize_head(
    repo: &Repository,
    head: &HeadState,
    in_progress: Option<HeaderKind>,
) -> Result<HeadSummary> {
    let (short_name, head_oid) = match head {
        HeadState::Branch {
            short_name,
            oid: Some(oid),
            ..
        } => (short_name, *oid),
        HeadState::Branch { short_name, .. } => {
            return Ok(HeadSummary::new(
                Some(short_name.clone()),
                HeaderKind::Unborn,
            ));
        }
        HeadState::Detached { .. } => return Ok(HeadSummary::new(None, HeaderKind::Detached)),
        HeadState::Invalid => return Ok(HeadSummary::new(None, HeaderKind::Invalid)),
    };
    let mut summary = HeadSummary::new(
        Some(short_name.clone()),
        in_progress.unwrap_or(HeaderKind::NoTarget),
    );
    let Some(base) =
        comparison_base(repo, short_name).context("could not resolve the target branch")?
    else {
        return Ok(summary);
    };
    let comparison = TipComparison::compute(repo, head_oid, base.oid, SHORTLOG_LIMIT)?;
    summary.header = in_progress.unwrap_or(HeaderKind::Compared(comparison.standing()));
    summary.target = Some(base.name);
    summary.ahead = comparison.ahead;
    summary.behind = comparison.behind;
    summary.ahead_commits = context::commit_summaries(repo, &comparison.newest_ahead)?;
    Ok(summary)
}
