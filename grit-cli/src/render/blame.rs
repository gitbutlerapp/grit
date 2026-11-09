//! Outcome types and renderers for `grit blame`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

use std::collections::BTreeMap;
use std::ops::Not;

use crate::markdown;
use crate::output::Person;

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
    pub(crate) display_lines: Vec<BlameDisplayLine>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<bool>,
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
pub(crate) struct BlameDisplayLine {
    pub commit_short: String,
    pub original_line: usize,
    pub final_line: usize,
    pub content: String,
    pub author_short: String,
    pub date: String,
    pub marker: char,
}

impl HumanRender for BlameOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for line in &self.display_lines {
            let short = &line.commit_short;
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
        outln!(
            w,
            "## Blame: {}\n",
            markdown::format_inline_code(&self.file)
        );
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
            let short = &line.commit_short;
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

pub(crate) fn blame_line_marker(boundary: bool, unblamable: bool, ignored: bool) -> char {
    if boundary {
        '^'
    } else if unblamable {
        '*'
    } else if ignored {
        '?'
    } else {
        ' '
    }
}

/// Strip trailing line terminators from blamed line text for display and JSON.
pub(crate) fn normalize_blame_line_content(content: &str) -> String {
    content.trim_end_matches(['\n', '\r']).to_owned()
}
