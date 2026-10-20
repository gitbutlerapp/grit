//! `grit log` — the recent history reachable from HEAD.
//!
//! Deliberately minimal: it shows one page of commits (newest first) and, when
//! there's more, prints the command to fetch the next page.

use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::log_walk::{LogWalkCursor, LogWalkQuery};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::diff::diff_of_commit;
use crate::commands::show::{diffstat, render_stat, use_color, DiffStat};
use crate::context::{self, subject_line, CommitSummary};
use crate::markdown;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, RenderContext};
use crate::rev_select::RevSelectArgs;
use crate::ui;

/// How many commits to show per page when the caller does not set `-n` / `--max-count`.
const PAGE: usize = 10;

/// Result of `grit log`: one page of commits, plus the next page's start (if any).
#[derive(Serialize, JsonSchema)]
pub struct LogOutcome {
    pub commits: Vec<LogCommit>,
    /// Opaque continuation cursor for `--cursor`, or `null` when there is no further history.
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
            outln!(w, "→ more: grit log --cursor={next}");
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
            outln!(w, "More history: run `grit log --cursor={next}`");
        }
        Ok(())
    }
}

pub fn run(rev: RevSelectArgs, cursor: Option<String>, stat: bool) -> Result<LogOutcome> {
    let repo = context::discover()?;
    let env = repo.environment();
    let mut deprecated_cursor = None;
    let mut deprecated_warning = false;
    let opts = rev.to_rev_walk_options(
        env,
        &["HEAD"],
        &mut deprecated_cursor,
        &mut deprecated_warning,
    )?;
    if deprecated_warning {
        eprintln!(
            "warning: passing a log cursor to `--before` is deprecated; use `--cursor` instead"
        );
    }
    let continuation = cursor.or(deprecated_cursor);
    if let Some(token) = continuation.as_deref() {
        if !rev.paths.is_empty() {
            bail!(
                "`grit log` cannot combine `--cursor` with path filters; drop the cursor or the paths"
            );
        }
        if rev.reverse || rev.first_parent || rev.merges || rev.no_merges || rev.skip > 0 {
            bail!(
                "`grit log` cannot combine `--cursor` with `--reverse`, `--first-parent`, \
                 `--merges`, `--no-merges`, or `--skip`"
            );
        }
        let resume = LogWalkCursor::decode(token).context("invalid log continuation cursor")?;
        let page_size = rev.max_count.unwrap_or(PAGE);
        let page = LogWalkCursor::step(&repo, resume.query(), Some(&resume), page_size)
            .context("could not list commits")?;
        let next = page.next.map(|c| c.encode()).transpose()?;
        return build_outcome(&repo, &page.commits, stat, next);
    }

    let token_refs = rev.rev_tokens(&["HEAD"]);
    let page_size = rev.max_count.unwrap_or(PAGE);

    let use_rev_walk = !rev.paths.is_empty()
        || rev.max_count.is_some()
        || LogWalkQuery::from_rev_walk(&repo, &token_refs, &opts)?.is_none();

    if use_rev_walk {
        if rev.max_count == Some(0) {
            return build_outcome(&repo, &[], stat, None);
        }
        let walk = repo.rev_walk();
        let oids = walk
            .commits(&token_refs, &opts)
            .context("could not list commits")?;
        return build_outcome(&repo, &oids, stat, None);
    }

    let query = LogWalkQuery::from_rev_walk(&repo, &token_refs, &opts)?
        .context("internal error: log walk query missing")?;

    let page =
        LogWalkCursor::step(&repo, &query, None, page_size).context("could not list commits")?;
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
