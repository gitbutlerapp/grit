//! `grit shortlog` — the commits on this branch that aren't on the target yet.

use anyhow::{Context, Result};
use clap::Args;
use grit_lib::objects::{CommitData, OidHashMap};
use grit_lib::refs::branch_tracking::{
    compare_with_target_optional, TargetComparisonOptions, TargetSpec,
};
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::diff::{render_log_commit_diff, DiffOutcome};
use crate::commands::log::{
    load_commits_with_format_for_oids, render_commit_diffs_markdown, LogCommit,
};
use crate::context::{self, CommitSummary};
use crate::diff_format::{DiffFormatArgs, ShortlogDiffFormatArgs};
use crate::diff_render::{render_stat, use_color};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ui;

/// CLI flags for [`run`].
#[derive(Args, Debug, Default)]
pub struct ShortlogCliArgs {
    /// Compare against this ref or revision instead of the discovered target branch.
    #[arg(long = "target", value_name = "REF")]
    pub target: Option<String>,
    /// Maximum commits to list on each side (`ahead`, and with `--behind` the behind list too).
    #[arg(short = 'n', long = "max-count", default_value = "100")]
    pub max_count: usize,
    /// Also list commits on the target that are not on this branch yet.
    #[arg(long = "behind")]
    pub behind: bool,
    #[command(flatten)]
    pub format: ShortlogDiffFormatArgs,
}

/// Result of `grit shortlog`.
#[derive(Serialize, JsonSchema)]
pub struct ShortlogOutcome {
    pub branch: String,
    /// Target branch name, or `null` when none could be resolved.
    pub target: Option<String>,
    pub ahead: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    pub commits: Vec<LogCommit>,
    /// True when more ahead commits exist than listed (`ahead` exceeds `commits.len()`).
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind_commits: Option<Vec<LogCommit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind_truncated: Option<bool>,
    /// Continuation cursor when the list is resumable; always `null` for `shortlog` today.
    pub next: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    #[schemars(skip)]
    behind_commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    #[schemars(skip)]
    commit_diffs: Vec<Option<DiffOutcome>>,
    #[serde(skip)]
    #[schemars(skip)]
    behind_commit_diffs: Vec<Option<DiffOutcome>>,
    #[serde(skip)]
    #[schemars(skip)]
    _format: DiffFormatArgs,
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
        self.render_commit_block(w, ctx, &self.commit_rows, &self.commits, &self.commit_diffs)?;
        if let (Some(behind), Some(commits)) = (self.behind, self.behind_commits.as_ref()) {
            outln!(w);
            outln!(
                w,
                "Behind {} by {} commit{}",
                target,
                behind,
                if behind == 1 { "" } else { "s" }
            );
            self.render_commit_block(
                w,
                ctx,
                &self.behind_commit_rows,
                commits,
                &self.behind_commit_diffs,
            )?;
        }
        Ok(())
    }
}

impl ShortlogOutcome {
    fn render_commit_block(
        &self,
        w: &mut dyn Write,
        ctx: &RenderContext,
        rows: &[CommitSummary],
        commits: &[LogCommit],
        diffs: &[Option<DiffOutcome>],
    ) -> Result<()> {
        let color = use_color();
        let repo = crate::context::discover()?;
        let sha_abbrevs = crate::log_display::abbrev_commit_summaries(&repo, rows)?;
        let opts = ui::CommitRowOptions {
            sha_abbrevs: Some(&sha_abbrevs),
            ..ui::CommitRowOptions::default()
        };
        for (row, (commit, diff)) in ui::commit_rows_with(rows, ctx.now.unix_timestamp(), &opts)
            .iter()
            .zip(commits.iter().zip(diffs))
        {
            outln!(w, "{row}");
            if let Some(outcome) = diff {
                render_log_commit_diff(w, outcome)?;
            } else if let Some(stat) = &commit.stat {
                render_stat(w, stat, color, None)?;
            }
            if diff.is_some() || commit.stat.is_some() {
                outln!(w);
            }
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
        if self._format.any_format_flag() {
            render_commit_diffs_markdown(w, &self.commit_rows, &self.commit_diffs, ctx)?;
        }
        if let (Some(behind), true) = (self.behind, self.behind_commits.is_some()) {
            outln!(w);
            outln!(
                w,
                "Behind `{target}` by **{}** commit{}.",
                behind,
                if behind == 1 { "" } else { "s" }
            );
            outln!(w);
            markdown::print_commit_list(w, &self.behind_commit_rows, ctx)?;
            if self._format.any_format_flag() {
                render_commit_diffs_markdown(
                    w,
                    &self.behind_commit_rows,
                    &self.behind_commit_diffs,
                    ctx,
                )?;
            }
        }
        Ok(())
    }
}

pub fn run(args: &ShortlogCliArgs) -> Result<ShortlogOutcome> {
    let format = args.format.to_diff_format();
    let repo = context::discover()?;
    let opts = TargetComparisonOptions {
        target: args.target.as_ref().map(|t| TargetSpec(t.clone())),
        list_limit: args.max_count,
        include_behind: args.behind,
    };
    let Some(comparison) =
        compare_with_target_optional(&repo, &opts).context("could not compare with target")?
    else {
        let head = resolve_head(repo.git_dir()).context("could not resolve HEAD")?;
        let branch = match head {
            HeadState::Branch { short_name, .. } => short_name,
            _ => "HEAD".to_owned(),
        };
        return Ok(empty_outcome(&branch, format));
    };

    let mut parsed = OidHashMap::<CommitData>::default();
    let (commits, commit_rows, commit_diffs) = load_commits_with_format_for_oids(
        &repo,
        &comparison.newest_ahead,
        format,
        &[],
        &mut parsed,
        true,
    )?;

    let (behind_commits, behind_commit_rows, behind_commit_diffs) = if args.behind {
        let (bc, br, bd) = load_commits_with_format_for_oids(
            &repo,
            &comparison.newest_behind,
            format,
            &[],
            &mut parsed,
            true,
        )?;
        (Some(bc), br, bd)
    } else {
        (None, Vec::new(), Vec::new())
    };

    let truncated = comparison.ahead > commits.len();
    let behind_truncated = args
        .behind
        .then_some(comparison.behind > comparison.newest_behind.len());

    Ok(ShortlogOutcome {
        branch: comparison.branch,
        target: Some(comparison.base.name),
        ahead: comparison.ahead,
        behind: Some(comparison.behind),
        commits,
        truncated,
        behind_commits,
        behind_truncated,
        next: None,
        commit_rows,
        behind_commit_rows,
        commit_diffs,
        behind_commit_diffs,
        _format: format,
    })
}

fn empty_outcome(branch: &str, format: DiffFormatArgs) -> ShortlogOutcome {
    ShortlogOutcome {
        branch: branch.to_owned(),
        target: None,
        ahead: 0,
        behind: None,
        commits: Vec::new(),
        truncated: false,
        behind_commits: None,
        behind_truncated: None,
        next: None,
        commit_rows: Vec::new(),
        behind_commit_rows: Vec::new(),
        commit_diffs: Vec::new(),
        behind_commit_diffs: Vec::new(),
        _format: format,
    }
}
