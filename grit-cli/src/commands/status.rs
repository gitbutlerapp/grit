//! `grit status` (and bare `grit`) — the dashboard: where you are, the commits
//! you're ahead by, what's changed, and what to do next.

use anyhow::{Context, Result};
use grit_lib::diff::DiffEntry;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::state::HeadState;
use serde::Serialize;

use crate::context::{self, CommitSummary};
use crate::output::{change_json, ChangeJson, CommitJson, HumanRender};
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
    /// Number of commits ahead of `target`.
    pub ahead: usize,
    /// The ahead-of-target commits (newest first); empty unless ahead of a target.
    pub commits: Vec<CommitJson>,
    pub staged: Vec<ChangeJson>,
    pub unstaged: Vec<ChangeJson>,
    pub untracked: Vec<String>,
    pub clean: bool,

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
}

/// Abbreviate a full hex oid to the 7-char short form used in human output.
fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run() -> Result<StatusOutcome> {
    let repo = context::discover()?;
    let model = status(&repo, &StatusOptions::default(), &mut NullProgress)
        .context("could not compute status")?;

    let (branch, detached, head, target, ahead_commits, header) =
        resolve_header(&repo, &model.head)?;
    let ahead = ahead_commits.len();
    let commits = ahead_commits.iter().map(CommitJson::from_summary).collect();

    let staged: Vec<ChangeJson> = model.staged.iter().map(change_json).collect();
    let unstaged: Vec<ChangeJson> = model.unstaged.iter().map(change_json).collect();
    let clean = model.staged.is_empty() && model.unstaged.is_empty() && model.untracked.is_empty();

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
    Vec<CommitSummary>,
    HeaderKind,
);

fn resolve_header(repo: &grit_lib::repo::Repository, head: &HeadState) -> Result<HeaderResult> {
    Ok(match head {
        HeadState::Branch {
            short_name,
            oid: Some(head_oid),
            ..
        } => match context::find_target_branch(repo)? {
            None => (
                Some(short_name.clone()),
                false,
                Some(head_oid.to_hex()),
                None,
                Vec::new(),
                HeaderKind::NoTarget,
            ),
            Some(target) => {
                let ahead: Vec<CommitSummary> =
                    context::commits_ahead_of(repo, *head_oid, target.oid)?;
                let header = if ahead.is_empty() {
                    HeaderKind::EvenWith
                } else {
                    HeaderKind::AheadOfTarget
                };
                (
                    Some(short_name.clone()),
                    false,
                    Some(head_oid.to_hex()),
                    Some(target.display_name),
                    ahead,
                    header,
                )
            }
        },
        HeadState::Branch { short_name, .. } => (
            Some(short_name.clone()),
            false,
            None,
            None,
            Vec::new(),
            HeaderKind::Unborn,
        ),
        HeadState::Detached { oid } => (
            None,
            true,
            Some(oid.to_hex()),
            None,
            Vec::new(),
            HeaderKind::Detached,
        ),
        HeadState::Invalid => (None, false, None, None, Vec::new(), HeaderKind::Invalid),
    })
}
