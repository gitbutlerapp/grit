//! `grit shortlog` — the commits on this branch that aren't on the target yet.

use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::refs::branch_tracking::target_branch;
use grit_lib::revwalk::merge_base::ahead_of_target_commits;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context::{self, CommitSummary};

/// Maximum commits to load for `grit shortlog` display (full ahead count is still reported).
const SHORTLOG_LIST_LIMIT: usize = 100;
use crate::commits_json::CommitJsonLoader;
use crate::markdown;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, RenderContext};
use crate::ui;

/// Result of `grit shortlog`.
#[derive(Serialize, JsonSchema)]
pub struct ShortlogOutcome {
    pub branch: String,
    /// Target branch name, or `null` when none could be resolved.
    pub target: Option<String>,
    pub ahead: usize,
    pub commits: Vec<CommitJson>,
    /// True when more ahead commits exist than listed (`ahead` exceeds `commits.len()`).
    pub truncated: bool,
    /// Continuation cursor when the list is resumable; always `null` for `shortlog` today.
    pub next: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    commit_rows: Vec<CommitSummary>,
}

impl HumanRender for ShortlogOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        outln!(w, "On {}", self.branch);
        let Some(target) = &self.target else {
            outln!(w, "No target branch found (tried target.branch, origin/master, origin/main, master, main).");
            return Ok(());
        };
        outln!(
            w,
            "Ahead of {} by {} commit{}",
            target,
            self.ahead,
            if self.ahead == 1 { "" } else { "s" }
        );
        for row in ui::commit_rows(&self.commit_rows, ctx.now.unix_timestamp()) {
            outln!(w, "{row}");
        }
        Ok(())
    }
}

impl MarkdownRender for ShortlogOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        outln!(w, "# Branch `{}`", self.branch);
        let Some(target) = &self.target else {
            outln!(w);
            outln!(
                w,
                "No target branch found (tried `target.branch`, `origin/master`, `origin/main`, `master`, `main`)."
            );
            return Ok(());
        };
        outln!(w);
        outln!(
            w,
            "Ahead of `{target}` by **{}** commit{}.",
            self.ahead,
            if self.ahead == 1 { "" } else { "s" }
        );
        outln!(w);
        markdown::print_commit_list(w, &self.commit_rows, ctx)?;
        Ok(())
    }
}

pub fn run() -> Result<ShortlogOutcome> {
    let repo = context::discover()?;
    let head = resolve_head(repo.git_dir()).context("could not resolve HEAD")?;
    let (branch_name, head_oid) = current_branch_and_oid(&head)?;

    let Some(target) = target_branch(&repo).context("could not resolve the target branch")? else {
        return Ok(ShortlogOutcome {
            branch: branch_name.to_owned(),
            target: None,
            ahead: 0,
            commits: Vec::new(),
            truncated: false,
            next: None,
            commit_rows: Vec::new(),
        });
    };

    let (ahead_count, newest) =
        ahead_of_target_commits(&repo, head_oid, target.oid, SHORTLOG_LIST_LIMIT)?;
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let loader = CommitJsonLoader::for_listed_commits();
    let (commits, commit_rows) = loader.load_with_summaries(&repo, &newest, now)?;
    let truncated = ahead_count > commits.len();
    Ok(ShortlogOutcome {
        branch: branch_name.to_owned(),
        target: Some(target.name),
        ahead: ahead_count,
        commits,
        truncated,
        next: None,
        commit_rows,
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
