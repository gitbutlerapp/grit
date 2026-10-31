//! `grit blame` — show who last modified each line of a file.

use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Not;

use anyhow::{bail, Context, Result};
use std::fmt;

/// Invalid `grit blame` arguments (maps to exit code 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlameUsageError;

use grit_lib::blame::{
    blame_file, resolve_blame_line_range_with_options, BlameFinalSource, BlameOptions, BlameResult,
    CopyDetection, IgnoreRevs, IgnoreRevsSources,
};
use grit_lib::diff::WhitespaceMode;

impl fmt::Display for BlameUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("blame copy detection: at most three -C flags are supported")
    }
}

impl std::error::Error for BlameUsageError {}
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<BlameLineJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<BlameHunkJson>>,
    pub commits: BTreeMap<String, BlameCommitJson>,
    #[serde(skip)]
    #[schemars(skip)]
    display_lines: Vec<BlameDisplayLine>,
}

#[derive(Serialize, JsonSchema)]
pub struct BlameLineJson {
    pub commit: String,
    pub original_line: usize,
    pub original_path: String,
    pub final_line: usize,
    pub content: String,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub encoding_lossy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unblamable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_committed: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
pub struct BlameHunkJson {
    pub commit: String,
    pub original_path: String,
    pub original_start: usize,
    pub final_start: usize,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unblamable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_committed: Option<bool>,
    pub lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_encoding_lossy: Option<Vec<bool>>,
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
    marker: char,
}

impl HumanRender for BlameOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for line in &self.display_lines {
            let short = short_oid(&line.commit.to_hex());
            let prefix = if line.marker == ' ' {
                String::new()
            } else {
                format!("{}", line.marker)
            };
            outln!(
                w,
                "{prefix}{short} ({author} {date} {final:>4}) {content}",
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
        if self.display_lines.is_empty() {
            outln!(w, "No lines to show.\n");
            return Ok(());
        }
        outln!(
            w,
            "| Line | Mark | Commit | Author | Date | Original | Content |"
        );
        outln!(w, "| ---: | :---: | --- | --- | --- | ---: | --- |");
        for line in &self.display_lines {
            let short = short_oid(&line.commit.to_hex());
            let content = markdown::escape_table_cell(&line.content);
            let mark = match line.marker {
                ' ' => String::new(),
                c => c.to_string(),
            };
            outln!(
                w,
                "| {} | {} | `{}` | {} | {} | {} | {} |",
                line.final_line,
                mark,
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

fn blame_line_marker(unblamable: bool, ignored: bool) -> char {
    if unblamable {
        '*'
    } else if ignored {
        '?'
    } else {
        ' '
    }
}

/// Strip trailing line terminators from blamed line text for display and JSON.
fn normalize_blame_line_content(content: String) -> String {
    content.trim_end_matches(['\n', '\r']).to_owned()
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    rev_pos: Option<String>,
    file: Option<String>,
    rev: Option<String>,
    line_range: Option<String>,
    first_parent: bool,
    ignore_rev: Vec<String>,
    ignore_revs_file: Vec<String>,
    group: bool,
    ignore_all_space: bool,
    copy_detection: u8,
    contents: Option<String>,
    now: time::OffsetDateTime,
) -> Result<BlameOutcome> {
    let (rev_pos, file) = match (rev_pos, file) {
        (None, Some(file)) => (None, file),
        (Some(rev), Some(file)) => (Some(rev), file),
        (Some(only), None) => (None, only),
        (None, None) => bail!("missing file path"),
    };
    if rev.is_some() && rev_pos.is_some() {
        bail!("cannot use both `--rev` and a positional revision before `--`");
    }
    let explicit_rev = rev.or(rev_pos);
    let rev_spec = explicit_rev.as_deref().unwrap_or("HEAD");

    let repo = context::discover()?;
    let start = resolve_revision_as_commit(&repo, rev_spec)
        .with_context(|| format!("could not resolve revision '{rev_spec}'"))?;

    let final_source = match contents.as_deref() {
        Some("-") => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)
                .context("could not read --contents from stdin")?;
            BlameFinalSource::Contents(buf)
        }
        Some(path) => BlameFinalSource::Contents(
            std::fs::read(path).with_context(|| format!("could not read --contents '{path}'"))?,
        ),
        None if explicit_rev.is_none() && repo.work_tree().is_some() => BlameFinalSource::Worktree,
        None => BlameFinalSource::Revision,
    };
    let mut options = BlameOptions {
        start: Some(start),
        final_source,
        now: Some(now),
        ..BlameOptions::default()
    };

    let blame_path = if repo.work_tree().is_some() {
        repo.resolve_worktree_pathspec(&file, repo.pathspec_argv_prefix(&file))
            .with_context(|| format!("could not resolve path '{file}'"))?
    } else {
        file.clone()
    };

    options.line_range = line_range
        .as_deref()
        .map(|spec| resolve_blame_line_range_with_options(&repo, &blame_path, spec, &options))
        .transpose()
        .with_context(|| "could not parse line range")?;

    let ignore = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            files: ignore_revs_file,
            revs: ignore_rev,
            use_config: true,
        },
    )?;

    let copy_detection = CopyDetection::from_count(copy_detection as usize)
        .map_err(|_| anyhow::Error::from(BlameUsageError))?;

    let mut whitespace = WhitespaceMode::default();
    if ignore_all_space {
        whitespace = WhitespaceMode::ignore_all_space();
    }

    options.first_parent_only = first_parent;
    options.ignore = ignore;
    options.whitespace = whitespace;
    options.copy_detection = copy_detection;
    let mut result = blame_file(&repo, &blame_path, &options)?;
    result.name_uncommitted_author(&options.final_source);

    Ok(outcome_from_result(
        blame_path,
        start.to_hex(),
        result,
        group,
        now.unix_timestamp(),
    ))
}

fn outcome_from_result(
    file: String,
    rev: String,
    result: BlameResult,
    group: bool,
    now: i64,
) -> BlameOutcome {
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
    for line in &result.lines {
        let commit_hex = line.commit.to_hex();
        let meta = result.commits.get(&line.commit);
        let (author_short, date) = meta
            .map(|m| {
                let (author, _) = context::author_and_time(&m.author);
                let date = relative_date_from(m.author_time, now);
                (author, date)
            })
            .unwrap_or_else(|| ("unknown".to_owned(), "?".to_owned()));

        let content = normalize_blame_line_content(line.content.clone());
        let marker = blame_line_marker(line.unblamable, line.ignored);
        lines_json.push(BlameLineJson {
            commit: commit_hex.clone(),
            original_line: line.original_line,
            final_line: line.final_line,
            content: content.clone(),
            original_path: line.original_path.clone(),
            encoding_lossy: line.encoding_lossy,
            ignored: line.ignored.then_some(true),
            unblamable: line.unblamable.then_some(true),
            not_committed: line.not_committed.then_some(true),
        });
        display_lines.push(BlameDisplayLine {
            commit: line.commit,
            original_line: line.original_line,
            final_line: line.final_line,
            content,
            author_short,
            date,
            marker,
        });
    }

    let (lines, hunks) = if group {
        let hunks_json = result
            .hunks()
            .into_iter()
            .map(|hunk| {
                let hunk_lines = result.hunk_lines(&hunk);
                let line_encoding_lossy = hunk_lines
                    .iter()
                    .any(|line| line.encoding_lossy)
                    .then(|| hunk_lines.iter().map(|line| line.encoding_lossy).collect());
                BlameHunkJson {
                    commit: hunk.commit.to_hex(),
                    original_start: hunk.original_start,
                    final_start: hunk.final_start,
                    count: hunk.count,
                    ignored: hunk.ignored.then_some(true),
                    unblamable: hunk.unblamable.then_some(true),
                    not_committed: hunk.not_committed.then_some(true),
                    lines: hunk_lines
                        .iter()
                        .map(|line| normalize_blame_line_content(line.content.clone()))
                        .collect(),
                    line_encoding_lossy,
                    original_path: hunk.original_path,
                }
            })
            .collect();
        (None, Some(hunks_json))
    } else {
        (Some(lines_json), None)
    };

    BlameOutcome {
        file,
        rev,
        lines,
        hunks,
        commits: commits_json,
        display_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_blame_line_content_strips_trailing_crlf() {
        assert_eq!(normalize_blame_line_content("x\r\n".to_owned()), "x");
        assert_eq!(normalize_blame_line_content("y\n".to_owned()), "y");
        assert_eq!(normalize_blame_line_content("z".to_owned()), "z");
    }
}
