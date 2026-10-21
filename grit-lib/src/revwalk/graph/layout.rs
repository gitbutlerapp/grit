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
    /// Each lane tracks the commit id flowing through it (`None` = empty slot).
    lanes: Vec<Option<ObjectId>>,
    lane_ids: Vec<usize>,
    next_lane_id: usize,
    /// Width in terminal columns (each lane uses two columns).
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
            lane_ids: Vec::new(),
            next_lane_id: 0,
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
    ///
    /// Each [`GraphCommitInput`] yields one commit line plus optional padding lines.
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
        let commit_lane = self.find_or_open_lane(commit.id);
        let mut lines = Vec::new();

        // Commit line
        let prefix = self.format_row(commit_lane, parents, true);
        self.width = self.width.max(prefix.len());
        let topology = self.record_topology(commit_lane, parents);
        self.topologies.push(topology);
        lines.push(GraphRenderedLine {
            prefix,
            kind: GraphLineKind::Commit,
        });

        // Post-merge padding when there are multiple parents (Git draws `\` rails on the next line).
        if parents.len() > 1 {
            let pad = self.format_post_merge(commit_lane, parents);
            self.width = self.width.max(pad.len());
            lines.push(GraphRenderedLine {
                prefix: pad,
                kind: GraphLineKind::Padding,
            });
        }

        self.advance_lanes(commit.id, parents, commit_lane);
        lines
    }

    fn find_or_open_lane(&mut self, id: ObjectId) -> usize {
        if let Some(idx) = self.lanes.iter().position(|slot| *slot == Some(id)) {
            return idx;
        }
        self.lanes.push(Some(id));
        let lane_id = self.next_lane_id;
        self.next_lane_id += 1;
        self.lane_ids.push(lane_id);
        self.lanes.len() - 1
    }

    fn format_row(&self, commit_lane: usize, parents: &[ObjectId], is_commit: bool) -> String {
        let lane_count = self.lanes.len().max(commit_lane + 1);
        let mut parts = Vec::with_capacity(lane_count);
        for lane in 0..lane_count {
            let glyph = if lane == commit_lane && is_commit {
                if parents.len() > 1 {
                    format!("{}   ", self.charset.commit)
                } else {
                    format!("{} ", self.charset.commit)
                }
            } else if self.lane_needed_for_parents(lane, parents, commit_lane)
                || lane < self.lanes.len() && self.lanes[lane].is_some()
            {
                format!("{} ", self.charset.pipe)
            } else {
                "  ".to_owned()
            };
            parts.push(glyph);
        }
        parts.join("")
    }

    fn format_post_merge(&self, commit_lane: usize, parents: &[ObjectId]) -> String {
        let lane_count = self.lanes.len().max(commit_lane + 1);
        let mut parts = Vec::with_capacity(lane_count);
        for lane in 0..lane_count {
            let glyph = if lane == commit_lane {
                format!("{} ", self.charset.merge_right)
            } else if self.lane_needed_for_parents(lane, parents, commit_lane)
                || lane < self.lanes.len() && self.lanes[lane].is_some()
            {
                format!("{} ", self.charset.pipe)
            } else {
                "  ".to_owned()
            };
            parts.push(glyph);
        }
        parts.join("")
    }

    fn lane_needed_for_parents(
        &self,
        lane: usize,
        parents: &[ObjectId],
        commit_lane: usize,
    ) -> bool {
        if lane == commit_lane {
            return false;
        }
        let Some(slot) = self.lanes.get(lane).and_then(|s| *s) else {
            return false;
        };
        parents.contains(&slot)
    }

    fn advance_lanes(&mut self, _id: ObjectId, parents: &[ObjectId], commit_lane: usize) {
        if parents.is_empty() {
            if commit_lane < self.lanes.len() {
                self.lanes[commit_lane] = None;
            }
            self.compact_lanes();
            return;
        }

        if commit_lane < self.lanes.len() {
            self.lanes[commit_lane] = Some(parents[0]);
        }

        for &parent in parents.iter().skip(1) {
            if self.lanes.contains(&Some(parent)) {
                continue;
            }
            if let Some(empty) = self.lanes.iter().position(|slot| slot.is_none()) {
                self.lanes[empty] = Some(parent);
            } else {
                self.lanes.push(Some(parent));
                let lane_id = self.next_lane_id;
                self.next_lane_id += 1;
                self.lane_ids.push(lane_id);
            }
        }

        // Drop lanes that no longer track anything relevant.
        self.compact_lanes();
    }

    fn compact_lanes(&mut self) {
        let mut i = 0;
        while i < self.lanes.len() {
            if self.lanes[i].is_none() {
                self.lanes.remove(i);
                self.lane_ids.remove(i);
            } else {
                i += 1;
            }
        }
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

    fn render_ids(blocks: &[Vec<GraphRenderedLine>]) -> Vec<String> {
        blocks
            .iter()
            .flat_map(|b| b.iter().map(|l| l.prefix.clone()))
            .collect()
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
        let lines = render_ids(&blocks);
        assert!(lines[0].starts_with("* "));
        assert_eq!(layout.topologies().len(), 3);
        assert_eq!(layout.topologies()[0].commit_column, 0);
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
        assert_eq!(blocks[0].len(), 2, "merge emits commit + padding line");
        assert!(blocks[0][0].prefix.contains('*'));
        assert_eq!(layout.topologies()[0].commit_column, 0);
    }
}
