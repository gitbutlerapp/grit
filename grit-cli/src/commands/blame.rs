//! `grit blame` — show who last modified each line of a file.

use std::collections::BTreeMap;
use std::io::Write;
use std::ops::RangeInclusive;

use anyhow::{Context, Result};
use grit_lib::blame::{blame_file, BlameOptions, BlameResult};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context::{self, relative_date_from};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, Person, RenderContext};

/// Result of `grit blame`.
#[derive(Serialize, JsonSchema)]
pub struct BlameOutcome {
    pub file: String,
    pub rev: String,
    pub lines: Vec<BlameLineJson>,
    pub commits: BTreeMap<String, BlameCommitJson>,
    #[serde(skip)]
    #[schemars(skip)]
    display_lines: Vec<BlameDisplayLine>,
}

#[derive(Serialize, JsonSchema)]
pub struct BlameLineJson {
    pub commit: String,
    pub original_line: usize,
    pub final_line: usize,
    pub content: String,
}

#[derive(Serialize, JsonSchema)]
pub struct BlameCommitJson {
    pub author: Person,
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
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for line in &self.display_lines {
            let short = short_oid(&line.commit.to_hex());
            outln!(
                w,
                "{short} ({author} {date} {final:>4}) {content}",
                author = line.author_short,
                date = line.date,
                final = line.final_line,
                content = line.content,
            );
        }
        Ok(())
    }
}

impl MarkdownRender for BlameOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Blame: `{}`\n", self.file);
        outln!(w, "Revision `{}`\n", self.rev);
        if self.lines.is_empty() {
            outln!(w, "No lines to show.\n");
            return Ok(());
        }
        outln!(w, "| Line | Commit | Author | Date | Original | Content |");
        outln!(w, "| ---: | --- | --- | --- | ---: | --- |");
        for line in &self.display_lines {
            let short = short_oid(&line.commit.to_hex());
            let content = markdown::escape_table_cell(&line.content);
            outln!(
                w,
                "| {} | `{}` | {} | {} | {} | {} |",
                line.final_line,
                short,
                line.author_short,
                line.date,
                line.original_line,
                content,
            );
        }
        outln!(w);
        Ok(())
    }
}

fn short_oid(full: &str) -> String {
    full.chars().take(7).collect()
}

/// Strip trailing line terminators from blamed line text for display and JSON.
fn normalize_blame_line_content(content: String) -> String {
    content.trim_end_matches(['\n', '\r']).to_owned()
}

pub fn run(
    file: String,
    rev: Option<String>,
    line_range: Option<String>,
    first_parent: bool,
    now: time::OffsetDateTime,
) -> Result<BlameOutcome> {
    let repo = context::discover()?;
    let rev_spec = rev.unwrap_or_else(|| "HEAD".to_owned());
    let start = resolve_revision_as_commit(&repo, &rev_spec)
        .with_context(|| format!("could not resolve revision '{rev_spec}'"))?;

    let range = line_range
        .as_deref()
        .map(parse_line_range)
        .transpose()
        .with_context(|| "could not parse line range")?;

    let normalized = if let Some(wt) = repo.work_tree() {
        grit_lib::worktree::pathspec::normalize_worktree_file_path(&file, wt, None)
    } else {
        file.clone()
    };

    let result = blame_file(
        &repo,
        &file,
        &BlameOptions {
            start: Some(start),
            line_range: range,
            first_parent_only: first_parent,
        },
    )?;

    Ok(outcome_from_result(
        normalized,
        start.to_hex(),
        result,
        now.unix_timestamp(),
    ))
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
                author: Person::from_identity_line(&info.author),
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

        let content = normalize_blame_line_content(line.content);
        lines_json.push(BlameLineJson {
            commit: commit_hex.clone(),
            original_line: line.original_line,
            final_line: line.final_line,
            content: content.clone(),
        });
        display_lines.push(BlameDisplayLine {
            commit: line.commit,
            original_line: line.original_line,
            final_line: line.final_line,
            content,
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

    #[test]
    fn normalize_blame_line_content_strips_trailing_crlf() {
        assert_eq!(normalize_blame_line_content("x\r\n".to_owned()), "x");
        assert_eq!(normalize_blame_line_content("y\n".to_owned()), "y");
        assert_eq!(normalize_blame_line_content("z".to_owned()), "z");
    }
}
