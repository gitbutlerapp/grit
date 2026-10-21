//! `grit log` — the recent history reachable from HEAD.
//!
//! Deliberately minimal: it shows one page of commits (newest first) and, when
//! there's more, prints the command to fetch the next page.

use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::log_walk::LogWalkCursor;
use grit_lib::revwalk::RevWalkListOptions;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::diff::diff_of_commit;
use crate::commands::show::diffstat;
use crate::context::{self, subject_line, CommitSummary};
use crate::diff_render::{render_stat, use_color, DiffStat};
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
}

impl HumanRender for LogOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if self.commits.is_empty() {
            outln!(w, "No commits yet.");
            return Ok(());
        }
        let now = ctx.now.unix_timestamp();
        let color = use_color();
        for (row, commit) in ui::commit_rows(&self.commit_rows, now)
            .iter()
            .zip(&self.commits)
        {
            outln!(w, "{row}");
            if let Some(stat) = &commit.stat {
                render_stat(w, stat, color, None)?;
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
    /// The files the commit changed, only with `--stat`.
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
        if let Some(next) = &self.next {
            outln!(w);
            outln!(w, "More history: run `grit log --before={next}`");
        }
        Ok(())
    }
}

pub fn run(
    before: Option<String>,
    stat: bool,
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
        // Omitted `--limit` means no cap (required for path history); default page
        // size applies only to the LogWalkCursor branch below.
        let opts = RevWalkListOptions {
            limit,
            first_parent: false,
            paths: paths.to_vec(),
            ..RevWalkListOptions::default()
        };
        let oids = walk
            .commits(&["HEAD"], &opts)
            .context("could not list commits")?;
        return build_outcome(&repo, &oids, stat, None);
    }

    let cursor = before
        .as_deref()
        .map(LogWalkCursor::decode)
        .transpose()
        .context("invalid log continuation cursor")?;

    let page =
        LogWalkCursor::step(&repo, cursor.as_ref(), page_size).context("could not list commits")?;
    let next = page.next.map(|c| c.encode()).transpose()?;
    build_outcome(&repo, &page.commits, stat, next)
}

fn build_outcome(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    stat: bool,
    next: Option<String>,
) -> Result<LogOutcome> {
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();

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
    let commits = commit_rows
        .iter()
        .map(|row| {
            Ok(LogCommit {
                commit: CommitJson::from_summary(row, now),
                stat: if stat {
                    Some(diffstat(&diff_of_commit(repo, &row.oid)?))
                } else {
                    None
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(LogOutcome {
        commits,
        next,
        commit_rows,
    })
}
