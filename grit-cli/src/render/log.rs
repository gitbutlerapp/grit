//! Display options and graph layout helpers for `grit log`.

use std::collections::HashMap;

use grit_lib::objects::ObjectId;
use grit_lib::revwalk::{
    GraphCharset, GraphCommitInput, GraphLayout, GraphLineKind, GraphRenderedLine,
};

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
