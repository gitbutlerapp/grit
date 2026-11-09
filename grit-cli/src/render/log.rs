//! Outcome types and renderers for `grit log` (pure graph layout helpers).

use std::collections::HashMap;
use std::io::Write;

use anyhow::Result;
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::{
    GraphCharset, GraphCommitInput, GraphLayout, GraphLineKind, GraphRenderedLine,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::diff_format::DiffFormatArgs;
use crate::log_display::{render_author_groups_human, render_author_groups_markdown};
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, RenderContext};
use crate::render::diff::DiffStat;
use crate::render::diff::{DiffTotalsJson, FileDiff};
use crate::stdio;
use crate::ui;

/// Group commits for summary output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogGroupBy {
    Author,
}

/// Display flags for human / Markdown rendering (not part of `--json`).
#[derive(Clone, Copy, Debug, Default)]
pub struct LogDisplayOptions {
    pub oneline: bool,
    pub decorate: bool,
    pub group_by: Option<LogGroupBy>,
    pub group_count_only: bool,
    pub graph: bool,
    pub graph_ascii: bool,
    pub line_counts: bool,
    pub columns: Option<ui::CommitColumns>,
    pub no_commits_in_repo: bool,
}

/// Result of `grit log`: one page of commits, plus the next page's start (if any).
#[derive(Serialize, JsonSchema)]
pub struct LogOutcome {
    #[serde(default)]
    pub commits: Vec<LogCommit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<LogAuthorGroupJson>>,
    pub next: Option<String>,
    pub truncated: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) author_group_rows: Vec<(usize, String, String)>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) display: LogDisplayOptions,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) oneline_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) graph_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) graph_note: Option<&'static str>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) continuation_args: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) human_body: Vec<u8>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) markdown_body: Vec<u8>,
}

/// Author aggregate in `--json` output when `--group-by author` is set.
#[derive(Serialize, JsonSchema)]
pub struct LogAuthorGroupJson {
    pub name: String,
    pub email: String,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<CommitJson>>,
}

/// One commit in `grit log` JSON output.
#[derive(Serialize, JsonSchema)]
pub struct LogCommit {
    #[serde(flatten)]
    pub commit: CommitJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileDiff>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<DiffTotalsJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<DiffStat>,
}

pub(crate) fn format_log_cursor_command(rev_args: Option<&str>, cursor: &str) -> String {
    match rev_args.filter(|s| !s.is_empty()) {
        Some(revs) => format!("grit log {revs} --cursor={cursor}"),
        None => format!("grit log --cursor={cursor}"),
    }
}

pub(crate) fn continuation_hint(outcome: &LogOutcome) -> Option<String> {
    if !outcome.truncated {
        return None;
    }
    Some(match &outcome.next {
        Some(next) => format_log_cursor_command(outcome.continuation_args.as_deref(), next),
        None => "rerun with a larger `-n`".to_owned(),
    })
}

impl HumanRender for LogOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if !self.human_body.is_empty() {
            stdio::io_result(w.write_all(&self.human_body))
                .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
            return Ok(());
        }
        if self.display.group_by.is_some() {
            if self.author_group_rows.is_empty() && self.display.no_commits_in_repo {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            if self.author_group_rows.is_empty() {
                return Ok(());
            }
            return render_author_groups_human(w, &self.author_group_rows);
        }
        if self.display.graph {
            if self.graph_lines.is_empty() && self.display.no_commits_in_repo {
                outln!(w, "No commits yet.");
            } else {
                for line in &self.graph_lines {
                    outln!(w, "{line}");
                }
            }
        } else if self.display.oneline {
            for line in &self.oneline_lines {
                outln!(w, "{line}");
            }
        }
        if let Some(hint) = continuation_hint(self) {
            outln!(w);
            outln!(w, "→ more: {hint}");
        }
        Ok(())
    }
}

impl MarkdownRender for LogOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if !self.markdown_body.is_empty() {
            stdio::io_result(w.write_all(&self.markdown_body))
                .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
            return Ok(());
        }
        if self.display.group_by.is_some() {
            if self.author_group_rows.is_empty() && self.display.no_commits_in_repo {
                outln!(w, "No commits yet.");
                return Ok(());
            }
            if self.author_group_rows.is_empty() {
                return Ok(());
            }
            return render_author_groups_markdown(w, &self.author_group_rows);
        }
        if let Some(note) = self.graph_note {
            outln!(w, "_{note}_");
            outln!(w);
        }
        if self.display.oneline {
            for line in &self.oneline_lines {
                outln!(w, "- `{line}`");
            }
        }
        Ok(())
    }
}

/// Lay out the graph for `oids` and return each commit's block of rendered lines.
pub(crate) fn graph_blocks(
    oids: &[ObjectId],
    parent_map: &HashMap<ObjectId, Vec<ObjectId>>,
    display: LogDisplayOptions,
) -> Vec<Vec<GraphRenderedLine>> {
    let charset = if display.graph_ascii {
        GraphCharset::ASCII
    } else {
        GraphCharset::UNICODE
    };
    let inputs: Vec<GraphCommitInput> = oids
        .iter()
        .map(|&id| GraphCommitInput {
            id,
            parents: parent_map.get(&id).cloned().unwrap_or_default(),
        })
        .collect();
    GraphLayout::new_without_topology_recording(charset).render(&inputs)
}

/// Columns the commit line's graph prefix takes in each block.
pub(crate) fn commit_prefix_widths(blocks: &[Vec<GraphRenderedLine>]) -> Vec<usize> {
    blocks
        .iter()
        .map(|block| {
            block
                .iter()
                .find(|line| line.kind != GraphLineKind::Padding)
                .map_or(0, |line| ui::display_width(&line.prefix))
        })
        .collect()
}

/// Join each block's graph prefixes to its commit text.
pub(crate) fn join_graph_blocks(
    blocks: Vec<Vec<GraphRenderedLine>>,
    texts: Vec<String>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(blocks.len() * 2);
    for (block, text) in blocks.into_iter().zip(texts) {
        for graph_line in block {
            if graph_line.kind == GraphLineKind::Padding {
                out.push(graph_line.prefix.trim_end().to_owned());
            } else {
                out.push(format!("{}{text}", graph_line.prefix));
            }
        }
    }
    out
}
