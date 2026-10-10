//! `grit shortlog` — the commits on this branch that aren't on the target yet.

use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::state::{resolve_head, HeadState};
use serde::Serialize;

use crate::context::{self, CommitSummary};

/// Maximum commits to load for `grit shortlog` display (full ahead count is still reported).
const SHORTLOG_LIST_LIMIT: usize = 100;
use crate::markdown;
use crate::output::{CommitJson, HumanRender, MarkdownRender};
use crate::ui;

/// Result of `grit shortlog`.
#[derive(Serialize)]
pub struct ShortlogOutcome {
    pub branch: String,
    /// Target branch name, or `null` when none could be resolved.
    pub target: Option<String>,
    pub ahead: usize,
    pub commits: Vec<CommitJson>,
    #[serde(skip)]
    commit_rows: Vec<CommitSummary>,
}

impl HumanRender for ShortlogOutcome {
    fn render_human(&self) {
        println!("On {}", self.branch);
        let Some(target) = &self.target else {
            println!("No target branch found (tried target.branch, origin/master, origin/main, master, main).");
            return;
        };
        println!(
            "Ahead of {} by {} commit{}",
            target,
            self.ahead,
            if self.ahead == 1 { "" } else { "s" }
        );
        for row in ui::commit_rows(&self.commit_rows) {
            println!("{row}");
        }
    }
}

impl MarkdownRender for ShortlogOutcome {
    fn render_markdown(&self) {
        println!("# Branch `{}`", self.branch);
        let Some(target) = &self.target else {
            println!();
            println!(
                "No target branch found (tried `target.branch`, `origin/master`, `origin/main`, `master`, `main`)."
            );
            return;
        };
        println!();
        println!(
            "Ahead of `{target}` by **{}** commit{}.",
            self.ahead,
            if self.ahead == 1 { "" } else { "s" }
        );
        println!();
        markdown::print_commit_list(&self.commit_rows);
    }
}

pub fn run() -> Result<ShortlogOutcome> {
    let repo = context::discover()?;
    let head = resolve_head(&repo.git_dir).context("could not resolve HEAD")?;
    let (branch_name, head_oid) = current_branch_and_oid(&head)?;

    let Some(target) = context::find_target_branch(&repo)? else {
        return Ok(ShortlogOutcome {
            branch: branch_name.to_owned(),
            target: None,
            ahead: 0,
            commits: Vec::new(),
            commit_rows: Vec::new(),
        });
    };

    let ahead = context::commits_ahead_of(&repo, head_oid, target.oid, SHORTLOG_LIST_LIMIT)?;
    Ok(ShortlogOutcome {
        branch: branch_name.to_owned(),
        target: Some(target.display_name),
        ahead: ahead.total,
        commits: ahead.commits.iter().map(CommitJson::from_summary).collect(),
        commit_rows: ahead.commits,
    })
}

fn current_branch_and_oid(head: &HeadState) -> Result<(&str, ObjectId)> {
    match head {
        HeadState::Branch {
            short_name,
            oid: Some(oid),
            ..
        } => Ok((short_name.as_str(), *oid)),
        HeadState::Branch { short_name, .. } => {
            bail!("branch '{short_name}' does not have any commits yet")
        }
        HeadState::Detached { .. } => {
            bail!("HEAD is detached; grit shortlog needs a current branch")
        }
        HeadState::Invalid => bail!("HEAD is invalid"),
    }
}
