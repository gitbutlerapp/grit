//! `grit blame` outcomes and rendering.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;
use std::ops::Not;

use crate::output::outln;
use crate::output::Person;
use crate::render::list::format_inline_code;
use crate::render::table::escape_table_cell;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

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
    pub(crate) commit_short: String,
    pub(crate) original_line: usize,
    pub(crate) final_line: usize,
    pub(crate) content: String,
    pub(crate) author_short: String,
    pub(crate) date: String,
    pub(crate) marker: char,
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
        outln!(w, "## Blame: {}\n", format_inline_code(&self.file));
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
            let content = escape_table_cell(&line.content);
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
