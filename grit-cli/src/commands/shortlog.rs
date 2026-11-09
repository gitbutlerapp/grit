//! `grit shortlog` — the commits on this branch that aren't on the target yet.

use crate::commands::diff::{render_log_commit_diff, DiffOutcome};
use crate::commands::log::{load_commits_with_format_for_oids, LogCommit};
use crate::context::{self, CommitSummary};
use crate::diff_format::{DiffFormatArgs, ShortlogDiffFormatArgs};
use crate::diff_render::{render_stat, use_color};
use crate::markdown;
use crate::output::RenderContext;
use crate::render::log::render_commit_diffs_markdown;
use crate::render::shortlog::ShortlogHumanCommitLine;
pub use crate::render::shortlog::ShortlogOutcome;
use crate::ui;
use anyhow::{Context, Result};
use clap::Args;
use grit_lib::objects::{CommitData, OidHashMap};
use grit_lib::refs::branch_tracking::{TargetComparisonOptions, TargetSpec};
use grit_lib::state::HeadState;

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

fn precompute_markdown_bullets(
    repo: &grit_lib::repo::Repository,
    rows: &[CommitSummary],
    now: i64,
) -> Result<Vec<String>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let sha_abbrevs = crate::log_display::abbrev_commit_summaries(repo, rows)?;
    Ok(rows
        .iter()
        .zip(&sha_abbrevs)
        .map(|(c, short)| markdown::commit_bullet_with_short(c, short, now))
        .collect())
}

fn precompute_human_lines(
    repo: &grit_lib::repo::Repository,
    rows: &[CommitSummary],
    commits: &[LogCommit],
    diffs: &[Option<DiffOutcome>],
    now: i64,
) -> Result<Vec<ShortlogHumanCommitLine>> {
    let color = use_color();
    let sha_abbrevs = crate::log_display::abbrev_commit_summaries(repo, rows)?;
    let opts = ui::CommitRowOptions {
        sha_abbrevs: Some(&sha_abbrevs),
        ..ui::CommitRowOptions::default()
    };
    let row_lines = ui::commit_rows_with(rows, now, &opts);
    let mut out = Vec::with_capacity(row_lines.len());
    for (row, (commit, diff)) in row_lines.into_iter().zip(commits.iter().zip(diffs)) {
        let mut detail_buf = Vec::new();
        let mut blank_after = false;
        if let Some(outcome) = diff {
            render_log_commit_diff(&mut detail_buf, outcome)?;
            blank_after = !outcome.files.is_empty();
        } else if let Some(stat) = &commit.stat {
            render_stat(&mut detail_buf, stat, color, None)?;
            blank_after = true;
        }
        let detail = if detail_buf.is_empty() {
            None
        } else {
            Some(String::from_utf8(detail_buf).map_err(|e| anyhow::anyhow!("{e}"))?)
        };
        out.push(ShortlogHumanCommitLine {
            row,
            detail,
            blank_after_detail: blank_after,
        });
    }
    Ok(out)
}

fn precompute_markdown_diffs(
    rows: &[CommitSummary],
    diffs: &[Option<DiffOutcome>],
    ctx: &RenderContext,
) -> Result<String> {
    let mut buf = Vec::new();
    render_commit_diffs_markdown(&mut buf, rows, diffs, ctx)?;
    String::from_utf8(buf).map_err(|e| anyhow::anyhow!("{e}"))
}

pub fn run(args: &ShortlogCliArgs, md_ctx: &RenderContext) -> Result<ShortlogOutcome> {
    let format = args.format.to_diff_format();
    let repo = context::discover()?;
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let opts = TargetComparisonOptions {
        target: args.target.as_ref().map(|t| TargetSpec(t.clone())),
        list_limit: args.max_count,
        include_behind: args.behind,
    };
    let Some(comparison) = repo
        .branches()
        .compare_with_target(&opts)
        .context("could not compare with target")?
    else {
        let head = repo.references().head().context("could not resolve HEAD")?;
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
    let human_ahead = precompute_human_lines(&repo, &commit_rows, &commits, &commit_diffs, now)?;
    let markdown_ahead_bullets = precompute_markdown_bullets(&repo, &commit_rows, now)?;
    let markdown_ahead_diffs = if format.any_format_flag() {
        precompute_markdown_diffs(&commit_rows, &commit_diffs, md_ctx)?
    } else {
        String::new()
    };

    let (behind_commits, human_behind, markdown_behind_bullets, markdown_behind_diffs) =
        if args.behind {
            let (bc, br, bd) = load_commits_with_format_for_oids(
                &repo,
                &comparison.newest_behind,
                format,
                &[],
                &mut parsed,
                true,
            )?;
            let human_behind = precompute_human_lines(&repo, &br, &bc, &bd, now)?;
            let markdown_behind_bullets = precompute_markdown_bullets(&repo, &br, now)?;
            let markdown_behind_diffs = if format.any_format_flag() {
                precompute_markdown_diffs(&br, &bd, md_ctx)?
            } else {
                String::new()
            };
            (
                Some(bc),
                human_behind,
                markdown_behind_bullets,
                markdown_behind_diffs,
            )
        } else {
            (None, Vec::new(), Vec::new(), String::new())
        };

    let ahead_side = comparison.ahead_commits();
    let behind_side = comparison.behind_commits();
    let truncated = ahead_side.truncated;
    let behind_truncated = args.behind.then_some(behind_side.truncated);

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
        human_ahead,
        human_behind,
        markdown_ahead_bullets,
        markdown_behind_bullets,
        markdown_ahead_diffs,
        markdown_behind_diffs,
        format,
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
        human_ahead: Vec::new(),
        human_behind: Vec::new(),
        markdown_ahead_bullets: Vec::new(),
        markdown_behind_bullets: Vec::new(),
        markdown_ahead_diffs: String::new(),
        markdown_behind_diffs: String::new(),
        format,
    }
}

/// Dispatch parsed CLI args for `Shortlog`.
pub fn dispatch(
    args: crate::args::ShortlogArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let shortlog = args;
    {
        emit_success(commands::shortlog::run(&shortlog, ctx)?, opts, ctx)
    }
}
