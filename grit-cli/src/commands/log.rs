//! `grit log` — the recent history reachable from HEAD.
//!
//! Deliberately minimal: it shows one page of commits (newest first) and, when
//! there's more, prints the command to fetch the next page.

use anyhow::{bail, Context, Result};
use grit_lib::diff::{CommitFilesManyOptions, DiffOptions};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::log_walk::LogWalkCursor;
use grit_lib::revwalk::RevWalkListOptions;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::diff::{
    cli_outcome_from_lib, render_log_commit_diff, DiffTotalsJson, FileDiff,
};
use crate::context::{self, subject_line, CommitSummary};
use crate::diff_format::DiffFormatArgs;
use crate::diff_render::{diffstat_from_files, render_stat, use_color, DiffStat};
use crate::markdown;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, RenderContext};
use crate::ui;

/// How many commits to show per page when the caller does not set `--limit`.
const PAGE: usize = 10;

/// Result of `grit log`: one page of commits, plus the next page's start (if any).
#[derive(Serialize, JsonSchema)]
pub struct LogOutcome {
    pub commits: Vec<LogCommit>,
    /// Opaque continuation cursor for `--before`, or `null` when there is no further history.
    pub next: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    #[schemars(skip)]
    format: DiffFormatArgs,
    #[serde(skip)]
    #[schemars(skip)]
    commit_diffs: Vec<Option<crate::commands::diff::DiffOutcome>>,
}

impl HumanRender for LogOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.commits.is_empty() {
            outln!(w, "No commits yet.");
            return Ok(());
        }
        let now = ctx.now.unix_timestamp();
        let color = use_color();
        for (row, (commit, diff)) in ui::commit_rows(&self.commit_rows, now)
            .iter()
            .zip(self.commits.iter().zip(&self.commit_diffs))
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
        if let Some(next) = &self.next {
            outln!(w);
            outln!(w, "→ more: grit log --before={next}");
        }
        Ok(())
    }
}

/// One commit in `grit log` JSON output.
#[derive(Serialize, JsonSchema)]
pub struct LogCommit {
    #[serde(flatten)]
    pub commit: CommitJson,
    /// Per-file diff detail when `-p`, `--stat`, `--name-only`, or `--name-status` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileDiff>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<DiffTotalsJson>,
    /// The files the commit changed, only with `--stat` (same shape as [`grit show`](../show/)).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<DiffStat>,
}

impl MarkdownRender for LogOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.commit_rows.is_empty() {
            outln!(w, "No commits yet.");
            return Ok(());
        }
        markdown::print_commit_list(w, &self.commit_rows, ctx)?;
        if self.format.any_format_flag() {
            for (row, diff) in self.commit_rows.iter().zip(&self.commit_diffs) {
                if let Some(outcome) = diff {
                    if outcome.files.is_empty() {
                        continue;
                    }
                    outln!(w);
                    let oid = row.oid.to_hex();
                    outln!(w, "### `{oid}`");
                    outcome.render_markdown(w, ctx)?;
                }
            }
        }
        if let Some(next) = &self.next {
            outln!(w);
            outln!(w, "More history: run `grit log --before={next}`");
        }
        Ok(())
    }
}

pub fn run(
    before: Option<String>,
    format: DiffFormatArgs,
    limit: Option<usize>,
    paths: &[String],
) -> Result<LogOutcome> {
    if before.is_some() && !paths.is_empty() {
        bail!(
            "`grit log` cannot combine `--before` with path filters; drop `--before` or the paths"
        );
    }

    let repo = context::discover()?;
    let page_size = limit.unwrap_or(PAGE);

    let use_rev_walk = !paths.is_empty() || (limit.is_some() && before.is_none());
    if use_rev_walk {
        let walk = repo.rev_walk();
        let opts = RevWalkListOptions {
            limit,
            first_parent: false,
            paths: paths.to_vec(),
            ..RevWalkListOptions::default()
        };
        let oids = walk
            .commits(&["HEAD"], &opts)
            .context("could not list commits")?;
        return build_outcome(&repo, &oids, format, paths, None);
    }

    let cursor = before
        .as_deref()
        .map(LogWalkCursor::decode)
        .transpose()
        .context("invalid log continuation cursor")?;

    let page =
        LogWalkCursor::step(&repo, cursor.as_ref(), page_size).context("could not list commits")?;
    let next = page.next.map(|c| c.encode()).transpose()?;
    build_outcome(&repo, &page.commits, format, paths, next)
}

fn build_outcome(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    format: DiffFormatArgs,
    path_args: &[String],
    next: Option<String>,
) -> Result<LogOutcome> {
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let detail = format.detail_for_log();

    let commit_rows = oids
        .iter()
        .map(|oid| {
            let commit = context::read_commit(repo, oid)?;
            let (author, timestamp) = context::author_and_time(&commit.author);
            Ok(CommitSummary {
                oid: *oid,
                subject: subject_line(&commit.message),
                author,
                author_ident: commit.author.clone(),
                timestamp,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let pathspecs = if path_args.is_empty() {
        Vec::new()
    } else {
        crate::commands::diff::resolve_worktree_diff_pathspecs(repo, path_args)?
    };

    let many_results = if let Some(detail) = detail {
        let mut diff_options = DiffOptions::from_config(config.as_ref());
        format.apply_context(&mut diff_options);
        diff_options.word_level_inline = false;
        let opts = CommitFilesManyOptions {
            diff_options,
            detail,
            skip_merge_patches: true,
        };
        Some((
            detail,
            repo.diff()
                .commit_files_many(oids, &pathspecs, opts, config.as_ref(), &thread_policy)
                .context("could not diff commits")?,
        ))
    } else {
        None
    };

    let mut commits = Vec::with_capacity(oids.len());
    let mut commit_diffs = Vec::with_capacity(oids.len());
    for (i, row) in commit_rows.iter().enumerate() {
        if let Some((detail, ref results)) = many_results {
            let outcome = cli_outcome_from_lib(
                repo,
                results[i].output.clone(),
                detail,
                format,
            )?;
            let stat = if format.stat {
                Some(diffstat_from_files(&outcome.files))
            } else {
                None
            };
            commits.push(LogCommit {
                commit: CommitJson::from_summary(row, now),
                files: Some(outcome.files.clone()),
                totals: outcome.totals,
                stat,
            });
            commit_diffs.push(Some(outcome));
        } else {
            commits.push(LogCommit {
                commit: CommitJson::from_summary(row, now),
                files: None,
                totals: None,
                stat: None,
            });
            commit_diffs.push(None);
        }
    }

    Ok(LogOutcome {
        commits,
        next,
        commit_rows,
        format,
        commit_diffs,
    })
}
