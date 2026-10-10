//! `grit status` (and bare `grit`) — the dashboard: where you are, the commits
//! you're ahead by, what's changed, and what to do next.

use anyhow::{Context, Result};
use grit_lib::diff::DiffEntry;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::state::{detect_in_progress, HeadState, InProgressOperation};
use serde::Serialize;

use crate::context::{self, CommitSummary};
use crate::markdown;
use crate::output::{change_json, ChangeJson, CommitJson, HumanRender, MarkdownRender};
use crate::ui::{self, PathDisplayContext};

/// Maximum number of commits to list in the status shortlog before summarizing.
const SHORTLOG_LIMIT: usize = 10;

/// Which header line `grit status` shows (drives only the human rendering; the
/// JSON fields below carry the same information in a flat, stable form).
#[derive(Clone, Copy)]
enum HeaderKind {
    AheadOfTarget,
    EvenWith,
    NoTarget,
    Unborn,
    Detached,
    Invalid,
    MergeInProgress,
    OtherInProgress,
}

/// Result of `grit status`.
#[derive(Serialize)]
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
    /// Newest ahead-of-target commits (newest first), up to the status shortlog limit.
    pub commits: Vec<CommitJson>,
    pub staged: Vec<ChangeJson>,
    pub unstaged: Vec<ChangeJson>,
    pub untracked: Vec<String>,
    pub clean: bool,
    /// True when a merge is in progress (`MERGE_HEAD` exists).
    pub merging: bool,
    /// In-progress operations (e.g. `merge`, `rebase`), when any apply.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub in_progress: Vec<String>,
    /// Unmerged paths from the index (stages 1–3).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,

    // Human-only state, not part of the JSON schema.
    #[serde(skip)]
    header: HeaderKind,
    #[serde(skip)]
    commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    staged_entries: Vec<DiffEntry>,
    #[serde(skip)]
    unstaged_entries: Vec<DiffEntry>,
    #[serde(skip)]
    path_display: Option<PathDisplayContext>,
}

impl HumanRender for StatusOutcome {
    fn render_human(&self) {
        self.render_header();
        self.render_changes();
        self.render_hints();
    }
}

impl MarkdownRender for StatusOutcome {
    fn render_markdown(&self) {
        self.render_markdown_header();
        self.render_markdown_changes();
        self.render_hints();
    }
}

impl StatusOutcome {
    fn render_header(&self) {
        let branch = self.branch.as_deref().unwrap_or_default();
        let target = self.target.as_deref().unwrap_or_default();
        match self.header {
            HeaderKind::AheadOfTarget => {
                println!("On {branch}  ·  {} ahead of {target}", self.ahead);
                println!();
                for row in ui::commit_rows(&self.commit_rows)
                    .iter()
                    .take(SHORTLOG_LIMIT)
                {
                    println!("{row}");
                }
                if self.ahead > SHORTLOG_LIMIT {
                    println!("  … and {} more", self.ahead - SHORTLOG_LIMIT);
                }
                println!();
            }
            HeaderKind::EvenWith => {
                println!("On {branch}  ·  even with {target}");
                println!();
            }
            HeaderKind::NoTarget => {
                println!("On {branch}");
                println!();
            }
            HeaderKind::Unborn => {
                println!("On {branch} — no commits yet");
                println!();
            }
            HeaderKind::Detached => {
                let short = self.head.as_deref().map(short_hex).unwrap_or_default();
                println!("Detached at {short}");
                println!();
            }
            HeaderKind::Invalid => {
                println!("HEAD is in an unknown state");
                println!();
            }
            HeaderKind::MergeInProgress => {
                println!("On {branch}  ·  merging — resolve conflicts");
                println!();
            }
            HeaderKind::OtherInProgress => {
                println!("On {branch}  ·  operation in progress");
                println!();
            }
        }
    }

    fn render_changes(&self) {
        if self.clean {
            println!("Nothing to commit — working tree clean.");
            return;
        }
        let display = self.path_display.as_ref();
        ui::print_change_group("Staged", &self.staged_entries, display);
        ui::print_change_group("Changed (not staged)", &self.unstaged_entries, display);
        ui::print_untracked(&self.untracked, display);
    }

    fn render_hints(&self) {
        if !self.conflicts.is_empty() {
            println!("→ resolve conflicts, then grit commit \"message\"");
            return;
        }
        if self.merging {
            println!("→ grit commit \"message\" to finish the merge");
            return;
        }
        let mut hints = Vec::new();
        if !self.unstaged_entries.is_empty() || !self.untracked.is_empty() {
            hints.push("grit add <file> to stage");
        }
        if !self.staged_entries.is_empty() {
            hints.push("grit commit \"message\" to commit");
        }
        if !hints.is_empty() {
            println!("→ {}", hints.join("  ·  "));
        }
    }

    fn render_markdown_header(&self) {
        let branch = self.branch.as_deref().unwrap_or_default();
        let target = self.target.as_deref().unwrap_or_default();
        match self.header {
            HeaderKind::AheadOfTarget => {
                println!("On **`{branch}`** · **{}** ahead of `{target}`", self.ahead);
                if !self.commit_rows.is_empty() {
                    println!();
                    markdown::print_commit_list(
                        &self.commit_rows[..self.commit_rows.len().min(SHORTLOG_LIMIT)],
                    );
                    if self.ahead > SHORTLOG_LIMIT {
                        println!();
                        println!("… and {} more commit(s).", self.ahead - SHORTLOG_LIMIT);
                    }
                }
                println!();
            }
            HeaderKind::EvenWith => {
                println!("On **`{branch}`** · even with `{target}`");
                println!();
            }
            HeaderKind::NoTarget => {
                println!("On **`{branch}`**");
                println!();
            }
            HeaderKind::Unborn => {
                println!("On **`{branch}`** — no commits yet");
                println!();
            }
            HeaderKind::Detached => {
                let short = self.head.as_deref().map(short_hex).unwrap_or_default();
                println!("Detached at `{short}`");
                println!();
            }
            HeaderKind::Invalid => {
                println!("HEAD is in an unknown state");
                println!();
            }
        }
    }

    fn render_markdown_changes(&self) {
        if self.clean {
            println!("Nothing to commit — working tree clean.");
            return;
        }
        let display = self.path_display.as_ref();
        markdown::print_change_section("Staged", &self.staged_entries, display);
        markdown::print_change_section("Changed (not staged)", &self.unstaged_entries, display);
        markdown::print_untracked(&self.untracked, display);
    }
}

/// Abbreviate a full hex oid to the 7-char short form used in human output.
fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run() -> Result<StatusOutcome> {
    let repo = context::discover()?;
    let model = status(&repo, &StatusOptions::default(), &mut NullProgress)
        .context("could not compute status")?;

    let in_progress: Vec<String> = detect_in_progress(&repo.git_dir)
        .into_iter()
        .map(in_progress_label)
        .collect();
    let merging = model.state.merge_in_progress;
    let conflicts = model.conflicts.clone();

    let (branch, detached, head, target, ahead_total, ahead_commits, header) =
        resolve_header(&repo, &model.head, merging, &in_progress)?;
    let ahead = ahead_total;
    let commits = ahead_commits.iter().map(CommitJson::from_summary).collect();

    let staged: Vec<ChangeJson> = model.staged.iter().map(change_json).collect();
    let unstaged: Vec<ChangeJson> = model.unstaged.iter().map(change_json).collect();
    let clean = model.staged.is_empty()
        && model.unstaged.is_empty()
        && model.untracked.is_empty()
        && conflicts.is_empty()
        && !merging;

    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let path_display = repo
        .work_tree
        .as_ref()
        .and_then(|wt| PathDisplayContext::from_cwd_and_work_tree(cwd, wt.clone()));

    Ok(StatusOutcome {
        branch,
        detached,
        head,
        target,
        ahead,
        commits,
        staged,
        unstaged,
        untracked: model.untracked,
        clean,
        merging,
        in_progress,
        conflicts,
        header,
        commit_rows: ahead_commits,
        staged_entries: model.staged,
        unstaged_entries: model.unstaged,
        path_display,
    })
}

/// Resolve the branch/target/ahead picture and the matching human header kind.
type HeaderResult = (
    Option<String>,
    bool,
    Option<String>,
    Option<String>,
    usize,
    Vec<CommitSummary>,
    HeaderKind,
);

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

fn header_for_in_progress(merging: bool, in_progress: &[String]) -> Option<HeaderKind> {
    if merging || in_progress.iter().any(|s| s == "merge") {
        return Some(HeaderKind::MergeInProgress);
    }
    if !in_progress.is_empty() {
        return Some(HeaderKind::OtherInProgress);
    }
    None
}

fn resolve_header(
    repo: &grit_lib::repo::Repository,
    head: &HeadState,
    merging: bool,
    in_progress: &[String],
) -> Result<HeaderResult> {
    Ok(match head {
        HeadState::Branch {
            short_name,
            oid: Some(head_oid),
            ..
        } => {
            let in_progress_header = header_for_in_progress(merging, in_progress);
            match context::find_target_branch(repo)? {
                None => (
                    Some(short_name.clone()),
                    false,
                    Some(head_oid.to_hex()),
                    None,
                    0,
                    Vec::new(),
                    in_progress_header.unwrap_or(HeaderKind::NoTarget),
                ),
                Some(target) => {
                    let ahead =
                        context::commits_ahead_of(repo, *head_oid, target.oid, SHORTLOG_LIMIT)?;
                    let header = in_progress_header.unwrap_or(if ahead.total == 0 {
                        HeaderKind::EvenWith
                    } else {
                        HeaderKind::AheadOfTarget
                    });
                    (
                        Some(short_name.clone()),
                        false,
                        Some(head_oid.to_hex()),
                        Some(target.display_name),
                        ahead.total,
                        ahead.commits,
                        header,
                    )
                }
            }
        }
        HeadState::Branch { short_name, .. } => (
            Some(short_name.clone()),
            false,
            None,
            None,
            0,
            Vec::new(),
            HeaderKind::Unborn,
        ),
        HeadState::Detached { oid } => (
            None,
            true,
            Some(oid.to_hex()),
            None,
            0,
            Vec::new(),
            HeaderKind::Detached,
        ),
        HeadState::Invalid => (None, false, None, None, 0, Vec::new(), HeaderKind::Invalid),
    })
}
