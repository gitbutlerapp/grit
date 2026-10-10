//! `grit blame` — show who last modified each line of a file.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use anyhow::{Context, Result};
use grit_lib::blame::{blame_file, BlameOptions, BlameResult};
use grit_lib::objects::ObjectId;
use grit_lib::rev_parse::resolve_revision;
use serde::Serialize;

use crate::context::{self, relative_date_from};
use crate::output::{HumanRender, MarkdownRender};

/// Result of `grit blame`.
#[derive(Serialize)]
pub struct BlameOutcome {
    pub file: String,
    pub rev: String,
    pub lines: Vec<BlameLineJson>,
    pub commits: BTreeMap<String, BlameCommitJson>,
    #[serde(skip)]
    display_lines: Vec<BlameDisplayLine>,
}

#[derive(Serialize)]
pub struct BlameLineJson {
    pub commit: String,
    pub original_line: usize,
    pub final_line: usize,
    pub content: String,
}

#[derive(Serialize)]
pub struct BlameCommitJson {
    pub author: String,
    pub author_time: i64,
    pub summary: String,
}

#[derive(Clone)]
struct BlameDisplayLine {
    commit: ObjectId,
    original_line: usize,
    final_line: usize,
    content: String,
    author_short: String,
    date: String,
}

impl HumanRender for BlameOutcome {
    fn render_human(&self) {
        for line in &self.display_lines {
            let short = short_oid(&line.commit.to_hex());
            println!(
                "{short} ({author} {date} {final:>4}) {content}",
                author = line.author_short,
                date = line.date,
                final = line.final_line,
                content = line.content,
            );
        }
    }
}

impl MarkdownRender for BlameOutcome {
    fn render_markdown(&self) {
        println!("## Blame: `{}`\n", self.file);
        println!("Revision `{}`\n", self.rev);
        if self.lines.is_empty() {
            println!("No lines to show.\n");
            return;
        }
        println!("| Line | Commit | Author | Date | Original | Content |");
        println!("| ---: | --- | --- | --- | ---: | --- |");
        for line in &self.display_lines {
            let short = short_oid(&line.commit.to_hex());
            let content = line.content.replace('|', "\\|");
            println!(
                "| {} | `{}` | {} | {} | {} | {} |",
                line.final_line, short, line.author_short, line.date, line.original_line, content,
            );
        }
        println!();
    }
}

fn short_oid(full: &str) -> String {
    full.chars().take(7).collect()
}

pub fn run(file: String, rev: Option<String>, line_range: Option<String>) -> Result<BlameOutcome> {
    let repo = context::discover()?;
    let rev_spec = rev.unwrap_or_else(|| "HEAD".to_owned());
    let start = resolve_revision(&repo, &rev_spec)
        .with_context(|| format!("could not resolve revision '{rev_spec}'"))?;

    let range = line_range
        .as_deref()
        .map(parse_line_range)
        .transpose()
        .with_context(|| "could not parse line range")?;

    let normalized = if let Some(wt) = repo.work_tree.as_deref() {
        grit_lib::pathspec::normalize_worktree_file_path(&file, wt, None)
    } else {
        file.clone()
    };

    let result = blame_file(
        &repo,
        &file,
        &BlameOptions {
            start: Some(start),
            line_range: range,
        },
    )?;

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    Ok(outcome_from_result(normalized, start.to_hex(), result, now))
}

fn parse_line_range(spec: &str) -> Result<RangeInclusive<usize>> {
    let (start, end) = spec
        .split_once(',')
        .with_context(|| format!("line range must be START,END (got '{spec}')"))?;
    let start: usize = start
        .trim()
        .parse()
        .with_context(|| format!("invalid start line in '{spec}'"))?;
    let end: usize = end
        .trim()
        .parse()
        .with_context(|| format!("invalid end line in '{spec}'"))?;
    if start == 0 || end == 0 {
        anyhow::bail!("line numbers are 1-based and must be positive");
    }
    if end < start {
        anyhow::bail!("line range end must be >= start");
    }
    Ok(start..=end)
}

fn outcome_from_result(file: String, rev: String, result: BlameResult, now: i64) -> BlameOutcome {
    let mut commits_json = BTreeMap::new();
    for (oid, info) in &result.commits {
        commits_json.insert(
            oid.to_hex(),
            BlameCommitJson {
                author: info.author.clone(),
                author_time: info.author_time,
                summary: info.summary.clone(),
            },
        );
    }

    let mut lines_json = Vec::with_capacity(result.lines.len());
    let mut display_lines = Vec::with_capacity(result.lines.len());
    for line in result.lines {
        let commit_hex = line.commit.to_hex();
        let meta = result.commits.get(&line.commit);
        let (author_short, date) = meta
            .map(|m| {
                let (author, _) = context::author_and_time(&m.author);
                let date = relative_date_from(m.author_time, now);
                (author, date)
            })
            .unwrap_or_else(|| ("unknown".to_owned(), "?".to_owned()));

        lines_json.push(BlameLineJson {
            commit: commit_hex.clone(),
            original_line: line.original_line,
            final_line: line.final_line,
            content: line.content.clone(),
        });
        display_lines.push(BlameDisplayLine {
            commit: line.commit,
            original_line: line.original_line,
            final_line: line.final_line,
            content: line.content,
            author_short,
            date,
        });
    }

    BlameOutcome {
        file,
        rev,
        lines: lines_json,
        commits: commits_json,
        display_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_range_accepts_inclusive_bounds() {
        assert_eq!(parse_line_range("2,5").unwrap(), 2..=5);
    }
}
