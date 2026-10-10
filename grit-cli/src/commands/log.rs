//! `grit log` — the recent history reachable from HEAD.
//!
//! Deliberately minimal: it shows one page of commits (newest first) and, when
//! there's more, prints the command to fetch the next page.

use anyhow::{Context, Result};
use grit_lib::rev_list::{rev_list, RevListOptions};
use serde::Serialize;

use crate::context::{self, subject_line, CommitSummary};
use crate::markdown;
use crate::output::{CommitJson, HumanRender, MarkdownRender};
use crate::ui;

/// How many commits to show per page.
const PAGE: usize = 10;

/// Result of `grit log`: one page of commits, plus the next page's start (if any).
#[derive(Serialize)]
pub struct LogOutcome {
    pub commits: Vec<CommitJson>,
    /// Full oid to resume from (`grit log --before=<next>`), or `null` when there
    /// is no further history.
    pub next: Option<String>,
    #[serde(skip)]
    commit_rows: Vec<CommitSummary>,
}

impl HumanRender for LogOutcome {
    fn render_human(&self) {
        if self.commits.is_empty() {
            println!("No commits yet.");
            return;
        }
        for row in ui::commit_rows(&self.commit_rows) {
            println!("{row}");
        }
        if let Some(next) = &self.next {
            println!();
            println!("→ more: grit log --before={}", short_hex(next));
        }
    }
}

impl MarkdownRender for LogOutcome {
    fn render_markdown(&self) {
        if self.commit_rows.is_empty() {
            println!("No commits yet.");
            return;
        }
        markdown::print_commit_list(&self.commit_rows);
        if let Some(next) = &self.next {
            println!();
            println!("More history: run `grit log --before={}`", short_hex(next));
        }
    }
}

/// Abbreviate a full hex oid to the 7-char short form used in human output.
fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run(before: Option<String>) -> Result<LogOutcome> {
    let repo = context::discover()?;
    let start = before.unwrap_or_else(|| "HEAD".to_owned());

    let opts = RevListOptions {
        // One extra so we know whether there's a next page.
        max_count: Some(PAGE + 1),
        ..Default::default()
    };
    let result = rev_list(&repo, std::slice::from_ref(&start), &[], &opts)
        .with_context(|| format!("could not list commits from {start}"))?;

    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();

    let commit_rows = result
        .commits
        .iter()
        .take(PAGE)
        .map(|oid| {
            let commit = context::read_commit(&repo, oid)?;
            let (author, timestamp) = context::author_and_time(&commit.author);
            Ok(CommitSummary {
                oid: *oid,
                subject: subject_line(&commit.message),
                author,
                timestamp,
                author_date: crate::dates::rfc3339_from_identity_when(
                    crate::dates::identity_when_suffix(&commit.author),
                ),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let commits = commit_rows
        .iter()
        .map(|row| CommitJson::from_summary(row, now))
        .collect();

    let next = result
        .commits
        .get(PAGE)
        .map(grit_lib::objects::ObjectId::to_hex);

    Ok(LogOutcome {
        commits,
        next,
        commit_rows,
    })
}
