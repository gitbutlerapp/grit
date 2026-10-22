//! Histogram-based line alignment for diff: index repeated lines on the old side, pick stable
//! anchors in the new side, subdivide unmatched bands, and delegate ambiguous bands to Myers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const BUCKET_CHAIN_LIMIT: u32 = 64;
const MAX_REGION_SPLITS: u64 = 4096;

/// Accounting for histogram work (tests and diagnostics); not wall-clock time.
#[derive(Debug, Default, Clone, Copy)]
pub struct HistogramWork {
    /// Hash-bucket chain steps while indexing or matching.
    pub chain_steps: u64,
    /// Region subdivisions (including the initial whole file).
    pub regions: u64,
    /// New-side line scans in the anchor search.
    pub anchor_scans: u64,
}

impl HistogramWork {
    fn step_chain(&mut self) {
        self.chain_steps += 1;
    }
    fn step_region(&mut self) {
        self.regions += 1;
    }
    fn step_anchor_scan(&mut self) {
        self.anchor_scans += 1;
    }
}

/// Inclusive line index span on both sides (0-based indices into the full line slices).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MatchedBand {
    old_lo: usize,
    old_hi: usize,
    new_lo: usize,
    new_hi: usize,
}

/// Per-bucket singly linked list node describing one line's text class inside an old window.
#[derive(Clone, Copy)]
struct ClassNode {
    /// Latest old line index (absolute) represented by this node.
    head_line: usize,
    /// Number of times this text has been seen while indexing (duplicate run weight).
    weight: u32,
    next_in_bucket: Option<usize>,
}

/// Index of old-window lines for histogram matching.
struct OldWindowIndex {
    table_mask: usize,
    heads: Vec<Option<usize>>,
    nodes: Vec<ClassNode>,
    /// For absolute old line index: previous line with identical text when scanning backward.
    prior_same: Vec<Option<usize>>,
    /// For absolute old line index: node in `nodes` that owns the line.
    owner: Vec<Option<usize>>,
}

impl OldWindowIndex {
    fn try_build(
        old: &[&str],
        old_start: usize,
        old_len: usize,
        work: &mut HistogramWork,
    ) -> Result<Self, ()> {
        let table_bits = (old_len.max(1).ilog2() as usize + 1).clamp(1, 20);
        let table_size = 1usize << table_bits;
        let mask = table_size - 1;
        let old_end = old_start + old_len;
        let mut prior_same = vec![None; old.len()];
        let mut owner = vec![None; old.len()];
        let mut heads: Vec<Option<usize>> = vec![None; table_size];
        let mut nodes: Vec<ClassNode> = Vec::new();

        for line in (old_start..old_end).rev() {
            let bucket = hash_line(old[line]) as usize & mask;
            let mut chain = 0u32;
            let mut cursor = heads[bucket];
            let mut merged = false;
            while let Some(ni) = cursor {
                work.step_chain();
                let head = nodes[ni].head_line;
                if old[head] == old[line] {
                    prior_same[line] = Some(head);
                    nodes[ni].head_line = line;
                    nodes[ni].weight = nodes[ni].weight.saturating_add(1);
                    owner[line] = Some(ni);
                    merged = true;
                    break;
                }
                chain += 1;
                cursor = nodes[ni].next_in_bucket;
            }
            if chain >= BUCKET_CHAIN_LIMIT {
                return Err(());
            }
            if merged {
                continue;
            }
            let ni = nodes.len();
            nodes.push(ClassNode {
                head_line: line,
                weight: 1,
                next_in_bucket: heads[bucket],
            });
            heads[bucket] = Some(ni);
            owner[line] = Some(ni);
        }
        Ok(Self {
            table_mask: mask,
            heads,
            nodes,
            prior_same,
            owner,
        })
    }

    fn lines_equal(&self, old: &[&str], o: usize, new: &[&str], n: usize) -> bool {
        old[o] == new[n]
    }
}

fn hash_line(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

struct AnchorSearch<'a> {
    old: &'a [&'a str],
    new: &'a [&'a str],
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
    index: &'a OldWindowIndex,
    work: &'a mut HistogramWork,
    best: Option<MatchedBand>,
    ambiguity_floor: u32,
    saw_match: bool,
}

impl AnchorSearch<'_> {
    fn run(&mut self) -> Result<(bool, Option<MatchedBand>), ()> {
        self.ambiguity_floor = BUCKET_CHAIN_LIMIT + 1;
        self.best = None;
        self.saw_match = false;
        for new_line in self.new_start..self.new_end {
            self.work.step_anchor_scan();
            self.consider_new_line(new_line);
        }
        let myers = self.saw_match && BUCKET_CHAIN_LIMIT < self.ambiguity_floor;
        Ok((myers, self.best))
    }

    fn consider_new_line(&mut self, new_line: usize) {
        let bucket = hash_line(self.new[new_line]) as usize & self.index.table_mask;
        let mut node_idx = self.index.heads[bucket];
        let mut next_new_hint = new_line + 1;
        while let Some(ni) = node_idx {
            self.work.step_chain();
            let node = &self.index.nodes[ni];
            if node.weight > self.ambiguity_floor {
                if !self.saw_match
                    && self
                        .index
                        .lines_equal(self.old, node.head_line, self.new, new_line)
                {
                    self.saw_match = true;
                }
                node_idx = node.next_in_bucket;
                continue;
            }
            let old_line = node.head_line;
            if !self
                .index
                .lines_equal(self.old, old_line, self.new, new_line)
            {
                node_idx = node.next_in_bucket;
                continue;
            }
            self.saw_match = true;
            let mut anchor_old = old_line;
            loop {
                let mut weight = node.weight;
                let (mut o_lo, mut n_lo) = (anchor_old, new_line);
                let (mut o_hi, mut n_hi) = (anchor_old, new_line);
                while self.old_start < o_lo
                    && self.new_start < n_lo
                    && self
                        .index
                        .lines_equal(self.old, o_lo - 1, self.new, n_lo - 1)
                {
                    o_lo -= 1;
                    n_lo -= 1;
                    if weight > 1 {
                        weight = weight.min(self.weight_at(o_lo));
                    }
                }
                while o_hi + 1 < self.old_end
                    && n_hi + 1 < self.new_end
                    && self
                        .index
                        .lines_equal(self.old, o_hi + 1, self.new, n_hi + 1)
                {
                    o_hi += 1;
                    n_hi += 1;
                    if weight > 1 {
                        weight = weight.min(self.weight_at(o_hi));
                    }
                }
                if next_new_hint <= n_hi {
                    next_new_hint = n_hi + 1;
                }
                let span = o_hi.saturating_sub(o_lo);
                let best_span = self
                    .best
                    .map(|b| b.old_hi.saturating_sub(b.old_lo))
                    .unwrap_or(0);
                if best_span < span || weight < self.ambiguity_floor {
                    self.best = Some(MatchedBand {
                        old_lo: o_lo,
                        old_hi: o_hi,
                        new_lo: n_lo,
                        new_hi: n_hi,
                    });
                    self.ambiguity_floor = weight;
                }
                let mut next_anchor = self.index.prior_same.get(anchor_old).copied().flatten();
                while let Some(p) = next_anchor {
                    if p > o_hi {
                        break;
                    }
                    next_anchor = self.index.prior_same.get(p).copied().flatten();
                }
                let Some(next_anchor) = next_anchor else {
                    break;
                };
                if next_anchor < self.old_start {
                    break;
                }
                anchor_old = next_anchor;
            }
            node_idx = node.next_in_bucket;
        }
        let _ = next_new_hint;
    }

    fn weight_at(&self, old_line: usize) -> u32 {
        self.index
            .owner
            .get(old_line)
            .copied()
            .flatten()
            .map(|ni| self.index.nodes[ni].weight)
            .unwrap_or(1)
    }
}

fn mark_myers<F>(
    changed1: &mut [bool],
    changed2: &mut [bool],
    fallback: &mut F,
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
) where
    F: FnMut(&mut [bool], &mut [bool], usize, usize, usize, usize),
{
    fallback(
        changed1,
        changed2,
        old_start,
        old_start + old_len,
        new_start,
        new_start + new_len,
    );
}

fn mark_all_changed(
    changed1: &mut [bool],
    changed2: &mut [bool],
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
) {
    if old_len > 0 {
        changed1[old_start..old_start + old_len].fill(true);
    }
    if new_len > 0 {
        changed2[new_start..new_start + new_len].fill(true);
    }
}

#[derive(Clone, Copy)]
struct TextRange {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
}

fn align_region<F>(
    old: &[&str],
    new: &[&str],
    changed1: &mut [bool],
    changed2: &mut [bool],
    fallback: &mut F,
    range: TextRange,
    work: &mut HistogramWork,
) where
    F: FnMut(&mut [bool], &mut [bool], usize, usize, usize, usize),
{
    let TextRange {
        old_start,
        old_len,
        new_start,
        new_len,
    } = range;
    work.step_region();
    if work.regions > MAX_REGION_SPLITS {
        mark_myers(
            changed1, changed2, fallback, old_start, old_len, new_start, new_len,
        );
        return;
    }
    if old_len == 0 && new_len == 0 {
        return;
    }
    if old_len == 0 {
        mark_all_changed(changed1, changed2, old_start, 0, new_start, new_len);
        return;
    }
    if new_len == 0 {
        mark_all_changed(changed1, changed2, old_start, old_len, new_start, 0);
        return;
    }

    let index = match OldWindowIndex::try_build(old, old_start, old_len, work) {
        Ok(idx) => idx,
        Err(()) => {
            mark_myers(
                changed1, changed2, fallback, old_start, old_len, new_start, new_len,
            );
            return;
        }
    };

    let old_end = old_start + old_len;
    let new_end = new_start + new_len;
    let mut search = AnchorSearch {
        old,
        new,
        old_start,
        old_end,
        new_start,
        new_end,
        index: &index,
        work,
        best: None,
        ambiguity_floor: 0,
        saw_match: false,
    };
    let (myers, band) = match search.run() {
        Ok(v) => v,
        Err(()) => {
            mark_myers(
                changed1, changed2, fallback, old_start, old_len, new_start, new_len,
            );
            return;
        }
    };

    if myers {
        mark_myers(
            changed1, changed2, fallback, old_start, old_len, new_start, new_len,
        );
        return;
    }

    let Some(band) = band else {
        mark_all_changed(changed1, changed2, old_start, old_len, new_start, new_len);
        return;
    };

    if band.new_lo > new_start || band.old_lo > old_start {
        align_region(
            old,
            new,
            changed1,
            changed2,
            fallback,
            TextRange {
                old_start,
                old_len: band.old_lo.saturating_sub(old_start),
                new_start,
                new_len: band.new_lo.saturating_sub(new_start),
            },
            work,
        );
    }
    let tail_old_start = band.old_hi + 1;
    let tail_old_len = old_end.saturating_sub(tail_old_start);
    let tail_new_start = band.new_hi + 1;
    let tail_new_len = new_end.saturating_sub(tail_new_start);
    if tail_old_len > 0 || tail_new_len > 0 {
        align_region(
            old,
            new,
            changed1,
            changed2,
            fallback,
            TextRange {
                old_start: tail_old_start,
                old_len: tail_old_len,
                new_start: tail_new_start,
                new_len: tail_new_len,
            },
            work,
        );
    }
}

/// Mark changed lines using histogram alignment with Myers fallback on ambiguous regions.
pub fn histogram_changed_flags<F>(
    old: &[&str],
    new: &[&str],
    mut myers_fallback: F,
) -> (Vec<bool>, Vec<bool>)
where
    F: FnMut(&mut [bool], &mut [bool], usize, usize, usize, usize),
{
    histogram_changed_flags_with_work(old, new, &mut myers_fallback, &mut HistogramWork::default())
}

/// Like [`histogram_changed_flags`], but fills `work` with deterministic operation counts.
pub fn histogram_changed_flags_with_work<F>(
    old: &[&str],
    new: &[&str],
    myers_fallback: &mut F,
    work: &mut HistogramWork,
) -> (Vec<bool>, Vec<bool>)
where
    F: FnMut(&mut [bool], &mut [bool], usize, usize, usize, usize),
{
    let mut changed1 = vec![false; old.len()];
    let mut changed2 = vec![false; new.len()];
    if old.is_empty() && new.is_empty() {
        return (changed1, changed2);
    }
    align_region(
        old,
        new,
        &mut changed1,
        &mut changed2,
        myers_fallback,
        TextRange {
            old_start: 0,
            old_len: old.len(),
            new_start: 0,
            new_len: new.len(),
        },
        work,
    );
    (changed1, changed2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frobnitz-sized inputs must stay within a modest histogram budget (no runaway chains).
    const FROBNITZ_MAX_CHAIN_STEPS: u64 = 8_000_000;
    const FROBNITZ_MAX_REGIONS: u64 = 50_000;
    const FROBNITZ_MAX_ANCHOR_SCANS: u64 = 2_000_000;

    #[test]
    fn frobnitz_histogram_work_bounded() {
        let old = include_str!("../../tests/support/data/diff_frobnitz_old.c");
        let new = include_str!("../../tests/support/data/diff_frobnitz_new.c");
        let ol: Vec<&str> = old.lines().collect();
        let nl: Vec<&str> = new.lines().collect();
        let mut work = HistogramWork::default();
        let _ = histogram_changed_flags_with_work(
            &ol,
            &nl,
            &mut |c1, c2, o0, o1, n0, n1| {
                for i in o0..o1 {
                    c1[i] = true;
                }
                for j in n0..n1 {
                    c2[j] = true;
                }
            },
            &mut work,
        );
        assert!(
            work.chain_steps < FROBNITZ_MAX_CHAIN_STEPS,
            "chain_steps={}",
            work.chain_steps
        );
        assert!(
            work.regions < FROBNITZ_MAX_REGIONS,
            "regions={}",
            work.regions
        );
        assert!(
            work.anchor_scans < FROBNITZ_MAX_ANCHOR_SCANS,
            "anchor_scans={}",
            work.anchor_scans
        );
    }
}
