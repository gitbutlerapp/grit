//! Lane-based graph layout (ASCII / Unicode) for commit history.

use std::collections::HashMap;

use crate::objects::ObjectId;

/// Character set for graph rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphCharset {
    /// Marker on the commit's lane (`*` or `●`).
    pub commit: char,
    /// Vertical lane continuation.
    pub pipe: char,
    /// Merge rail angling toward a secondary parent.
    pub merge_right: char,
    /// Merge rail angling from a secondary parent.
    pub merge_left: char,
    /// Reserved for octopus merge junctions.
    pub merge_middle: char,
}

impl GraphCharset {
    /// Git-compatible ASCII glyphs.
    pub const ASCII: Self = Self {
        commit: '*',
        pipe: '|',
        merge_right: '\\',
        merge_left: '/',
        merge_middle: '.',
    };

    /// Unicode box-drawing glyphs (when the terminal supports UTF-8).
    pub const UNICODE: Self = Self {
        commit: '●',
        pipe: '│',
        merge_right: '╲',
        merge_left: '╱',
        merge_middle: '┼',
    };
}

/// One commit row fed into [`GraphLayout`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCommitInput {
    /// Commit id for this row.
    pub id: ObjectId,
    /// Display parents (already rewritten); newest-first walk uses parents stored oldest-first.
    pub parents: Vec<ObjectId>,
}

/// Kind of graph line relative to a commit entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphLineKind {
    /// The line that carries the commit subject (contains the `*` marker).
    Commit,
    /// Extra graph padding before the next commit (merge rails).
    Padding,
}

/// A rendered graph prefix (two columns per lane: glyph + space), without commit text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRenderedLine {
    /// Graph prefix; always ends with a space when non-empty.
    pub prefix: String,
    /// Whether this line is the commit line or padding.
    pub kind: GraphLineKind,
}

/// Abstract lane topology for tests (column indices and parent edges).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRowTopology {
    /// Column holding the commit marker for this row.
    pub commit_column: usize,
    /// Active lane indices after processing this row.
    pub active_lanes: Vec<usize>,
    /// Directed edges from child lane to parent lane for this commit.
    pub edges: Vec<(usize, usize)>,
}

/// Incremental graph layout state.
#[derive(Clone, Debug)]
pub struct GraphLayout {
    charset: GraphCharset,
    /// Each lane tracks the next commit id flowing through it.
    lanes: Vec<Option<ObjectId>>,
    width: usize,
    topologies: Vec<GraphRowTopology>,
}

impl Default for GraphLayout {
    fn default() -> Self {
        Self::new(GraphCharset::ASCII)
    }
}

impl GraphLayout {
    /// New layout using `charset`.
    #[must_use]
    pub fn new(charset: GraphCharset) -> Self {
        Self {
            charset,
            lanes: Vec::new(),
            width: 0,
            topologies: Vec::new(),
        }
    }

    /// Topologies recorded for each commit line (for unit tests).
    #[must_use]
    pub fn topologies(&self) -> &[GraphRowTopology] {
        &self.topologies
    }

    /// Current graph width in columns (excluding trailing space after the last lane).
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Consume commits in display order (newest first) and return graph lines grouped per commit.
    #[must_use]
    pub fn render(&mut self, commits: &[GraphCommitInput]) -> Vec<Vec<GraphRenderedLine>> {
        let mut blocks = Vec::with_capacity(commits.len());
        for commit in commits {
            blocks.push(self.render_one(commit));
        }
        blocks
    }

    fn render_one(&mut self, commit: &GraphCommitInput) -> Vec<GraphRenderedLine> {
        let parents = &commit.parents;
        let mut lines = Vec::new();

        if self.lanes_matching(commit.id) > 1 {
            let pad = self.format_convergence();
            self.width = self.width.max(pad.len());
            lines.push(GraphRenderedLine {
                prefix: pad,
                kind: GraphLineKind::Padding,
            });
            self.collapse_lanes_to(commit.id);
        }

        let commit_lane = self.find_or_open_lane(commit.id);
        let prefix = self.format_commit_row(commit_lane, parents);
        self.width = self.width.max(prefix.len());
        self.topologies
            .push(self.record_topology(commit_lane, parents));
        lines.push(GraphRenderedLine {
            prefix,
            kind: GraphLineKind::Commit,
        });

        if parents.len() > 1 {
            let pad = self.format_post_merge(commit_lane);
            self.width = self.width.max(pad.len());
            lines.push(GraphRenderedLine {
                prefix: pad,
                kind: GraphLineKind::Padding,
            });
        }

        self.advance_lanes(parents, commit_lane);
        lines
    }

    fn lanes_matching(&self, id: ObjectId) -> usize {
        self.lanes.iter().filter(|slot| **slot == Some(id)).count()
    }

    fn collapse_lanes_to(&mut self, id: ObjectId) {
        let mut kept = false;
        self.lanes.retain(|slot| {
            if *slot == Some(id) {
                if kept {
                    return false;
                }
                kept = true;
                return true;
            }
            slot.is_some()
        });
        if !kept {
            self.lanes.push(Some(id));
        }
    }

    fn find_or_open_lane(&mut self, id: ObjectId) -> usize {
        if let Some(idx) = self.lanes.iter().position(|slot| *slot == Some(id)) {
            return idx;
        }
        self.lanes.push(Some(id));
        self.lanes.len() - 1
    }

    fn format_commit_row(&self, commit_lane: usize, parents: &[ObjectId]) -> String {
        let cols = self.lane_count().max(commit_lane + 1);
        let mut parts = Vec::with_capacity(cols);
        for lane in 0..cols {
            let cell = if lane == commit_lane {
                if parents.len() > 1 {
                    format!("{}   ", self.charset.commit)
                } else {
                    format!("{} ", self.charset.commit)
                }
            } else if self.lane_active(lane) {
                format!("{} ", self.charset.pipe)
            } else {
                "  ".to_owned()
            };
            parts.push(cell);
        }
        parts.join("")
    }

    fn format_post_merge(&self, commit_lane: usize) -> String {
        let cols = self.lane_count().max(commit_lane + 2);
        let mut parts = Vec::with_capacity(cols);
        for lane in 0..cols {
            let cell = if lane == commit_lane {
                format!("{} ", self.charset.pipe)
            } else if lane == commit_lane + 1 {
                format!("{} ", self.charset.merge_right)
            } else if self.lane_active(lane) {
                format!("{} ", self.charset.pipe)
            } else {
                "  ".to_owned()
            };
            parts.push(cell);
        }
        parts.join("")
    }

    fn format_convergence(&self) -> String {
        let cols = self.lane_count();
        let mut parts = Vec::with_capacity(cols);
        for lane in 0..cols {
            let cell = if lane == 0 {
                format!("{}{} ", self.charset.pipe, self.charset.merge_left)
            } else {
                "  ".to_owned()
            };
            parts.push(cell);
        }
        parts.join("")
    }

    fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    fn lane_active(&self, lane: usize) -> bool {
        self.lanes.get(lane).is_some_and(|s| s.is_some())
    }

    fn advance_lanes(&mut self, parents: &[ObjectId], commit_lane: usize) {
        if parents.is_empty() {
            if commit_lane < self.lanes.len() {
                self.lanes[commit_lane] = None;
            }
            self.compact_lanes();
            return;
        }

        let first = parents[0];
        if commit_lane < self.lanes.len() {
            self.lanes[commit_lane] = Some(first);
        }

        for &parent in parents.iter().skip(1) {
            if self.lanes.contains(&Some(parent)) {
                continue;
            }
            if let Some(empty) = self.lanes.iter().position(|slot| slot.is_none()) {
                self.lanes[empty] = Some(parent);
            } else {
                self.lanes.push(Some(parent));
            }
        }
        self.compact_lanes();
    }

    fn compact_lanes(&mut self) {
        self.lanes.retain(|slot| slot.is_some());
    }

    fn record_topology(&self, commit_lane: usize, parents: &[ObjectId]) -> GraphRowTopology {
        let mut edges = Vec::new();
        let parent_to_lane: HashMap<ObjectId, usize> = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.map(|oid| (oid, i)))
            .collect();
        for parent in parents {
            if let Some(&parent_lane) = parent_to_lane.get(parent) {
                edges.push((commit_lane, parent_lane));
            }
        }
        GraphRowTopology {
            commit_column: commit_lane,
            active_lanes: (0..self.lanes.len()).collect(),
            edges,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectId;

    fn oid(n: u8) -> ObjectId {
        ObjectId::from_bytes(&[n; 20]).expect("test oid")
    }

    fn flat_prefixes(blocks: &[Vec<GraphRenderedLine>]) -> String {
        blocks
            .iter()
            .flat_map(|b| b.iter().map(|l| l.prefix.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn graph_layout_linear() {
        let c2 = oid(2);
        let c1 = oid(1);
        let c0 = oid(0);
        let inputs = [
            GraphCommitInput {
                id: c2,
                parents: vec![c1],
            },
            GraphCommitInput {
                id: c1,
                parents: vec![c0],
            },
            GraphCommitInput {
                id: c0,
                parents: vec![],
            },
        ];
        let mut layout = GraphLayout::new(GraphCharset::ASCII);
        let blocks = layout.render(&inputs);
        assert!(flat_prefixes(&blocks).starts_with("* "));
        assert_eq!(layout.topologies().len(), 3);
    }

    #[test]
    fn graph_layout_merge_fork() {
        let root = oid(0);
        let main = oid(1);
        let topic = oid(2);
        let merge = oid(3);
        let inputs = [
            GraphCommitInput {
                id: merge,
                parents: vec![main, topic],
            },
            GraphCommitInput {
                id: topic,
                parents: vec![root],
            },
            GraphCommitInput {
                id: main,
                parents: vec![root],
            },
            GraphCommitInput {
                id: root,
                parents: vec![],
            },
        ];
        let mut layout = GraphLayout::new(GraphCharset::ASCII);
        let blocks = layout.render(&inputs);
        let art = flat_prefixes(&blocks);
        assert!(
            art.contains('\\'),
            "merge should draw a backslash rail: {art}"
        );
        assert!(art.contains('/'), "convergence should draw slash: {art}");
        assert!(
            art.contains("| *"),
            "secondary branch should use pipe column: {art}"
        );
    }
}
