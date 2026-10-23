//! `grit shortlog` — the commits on this branch that aren't on the target yet.

use anyhow::{Context, Result};
use clap::Args;
use grit_lib::refs::branch_tracking::{
    compare_with_target_optional, TargetComparisonOptions, TargetSpec,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commits_json::CommitJsonLoader;
use crate::context::{self, CommitSummary};
use crate::markdown;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, RenderContext};
use crate::ui;

/// Maximum commits to load for `grit shortlog` display (full ahead count is still reported).
const SHORTLOG_LIST_LIMIT: usize = 100;

/// CLI flags for [`run`].
#[derive(Args, Debug, Default)]
pub struct ShortlogCliArgs {
    /// Compare against this ref or revision instead of the discovered target branch.
    #[arg(long = "target", value_name = "REF")]
    pub target: Option<String>,
}

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

pub fn run(args: &ShortlogCliArgs) -> Result<ShortlogOutcome> {
    let repo = context::discover()?;
    let opts = TargetComparisonOptions {
        target: args.target.as_ref().map(|t| TargetSpec(t.clone())),
        list_limit: SHORTLOG_LIST_LIMIT,
    };
    let Some(comparison) =
        compare_with_target_optional(&repo, &opts).context("could not compare with target")?
    else {
        let head =
            grit_lib::state::resolve_head(repo.git_dir()).context("could not resolve HEAD")?;
        let branch = match head {
            grit_lib::state::HeadState::Branch { short_name, .. } => short_name,
            _ => "HEAD".to_owned(),
        };
        return Ok(ShortlogOutcome {
            branch,
            target: None,
            ahead: 0,
            commits: Vec::new(),
            truncated: false,
            next: None,
            commit_rows: Vec::new(),
        });
    };

    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let loader = CommitJsonLoader::for_listed_commits();
    let (commits, commit_rows) =
        loader.load_with_summaries(&repo, &comparison.newest_ahead, now)?;
    let truncated = comparison.ahead > commits.len();
    Ok(ShortlogOutcome {
        branch: comparison.branch,
        target: Some(comparison.base.name),
        ahead: comparison.ahead,
        commits,
        truncated,
        next: None,
        commit_rows,
    })
}
