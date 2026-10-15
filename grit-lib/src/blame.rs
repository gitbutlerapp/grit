//! Blame line-mapping algorithm.
//!
//! Pure line-correspondence machinery used by `git blame` / `annotate`: given an
//! old and a new version of a file (as line slices), produce a per-new-line map
//! back to the originating old line. Both an exact map (via the configured diff
//! algorithm, honoring the indent heuristic) and a fuzzy fallback (for lines a
//! parent rewrote) are provided. No I/O, no repository access — strings in,
//! mappings out. [`compute_blame`] and [`blame_file`] drive the per-commit walk and
//! call these helpers.
//!
//! Library guide: [blame](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! # Ok(())
//! # }
//! ```

use std::ops::RangeInclusive;

use similar::Algorithm as SimilarAlgorithm;

/// Diff algorithm used when mapping lines between blob revisions during blame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlameDiffAlgorithm {
    /// Default line diff (Myers); used when config does not specify an algorithm.
    Myers,
    /// Histogram-style blame diff (mapped to patience in the `similar` crate).
    Histogram,
    /// Patience diff for blame line correspondence.
    Patience,
    /// Minimal / LCS-style diff for blame line correspondence.
    Minimal,
}

impl BlameDiffAlgorithm {
    /// Map this blame diff setting to a [`similar::Algorithm`] instance.
    pub fn to_similar(self) -> SimilarAlgorithm {
        match self {
            // The `similar` crate doesn't expose histogram/minimal directly.
            // These mappings are chosen to match expected blame behavior in
            // upstream t8015 parity tests.
            BlameDiffAlgorithm::Myers => SimilarAlgorithm::Myers,
            BlameDiffAlgorithm::Histogram => SimilarAlgorithm::Patience,
            BlameDiffAlgorithm::Patience => SimilarAlgorithm::Patience,
            BlameDiffAlgorithm::Minimal => SimilarAlgorithm::Lcs,
        }
    }
}

/// Parse a config-style diff algorithm name (`myers`, `histogram`, …).
pub fn parse_diff_algorithm_name(name: &str) -> Option<BlameDiffAlgorithm> {
    match name.to_ascii_lowercase().as_str() {
        "myers" | "default" => Some(BlameDiffAlgorithm::Myers),
        "histogram" => Some(BlameDiffAlgorithm::Histogram),
        "patience" => Some(BlameDiffAlgorithm::Patience),
        "minimal" => Some(BlameDiffAlgorithm::Minimal),
        _ => None,
    }
}

/// Whether Myers blame should drop a trailing equal-line match (Git tie-break parity).
pub fn should_drop_tail_match_for_myers(
    diff_algorithm: BlameDiffAlgorithm,
    parent_idx: usize,
    current_idx: usize,
    parent_lines: &[&str],
) -> bool {
    if diff_algorithm != BlameDiffAlgorithm::Myers {
        return false;
    }
    if parent_lines.is_empty() || parent_idx + 1 != parent_lines.len() {
        return false;
    }
    // Preserve common append-at-end behavior. We only drop matches where the
    // final parent line got shifted to the right in the child.
    if current_idx <= parent_idx {
        return false;
    }
    // Restrict this heuristic to duplicated low-information tail lines, which
    // are the cases where xdiff/myers tie-breaking differs from `similar`.
    let tail = parent_lines[parent_idx];
    parent_lines.iter().filter(|line| **line == tail).count() >= 2
}

fn common_affix_trim<'a>(
    old: &'a [&'a str],
    new: &'a [&'a str],
) -> (usize, usize, &'a [&'a str], &'a [&'a str]) {
    let mut prefix = 0usize;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old.len().saturating_sub(prefix)
        && suffix < new.len().saturating_sub(prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    (prefix, suffix, old_mid, new_mid)
}

/// Map each line in `new` to its origin line index in `old` (if any).
pub fn build_line_map(
    old: &[&str],
    new: &[&str],
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
) -> Vec<Option<usize>> {
    build_line_map_with_changed_flags(old, new, diff_algorithm, indent_heuristic, false).0
}

/// Like [`build_line_map`], optionally returning xdiff-style changed flags for the new side
/// from the same compacted diff ops (avoids a second full diff on blame hot paths).
fn build_line_map_with_changed_flags(
    old: &[&str],
    new: &[&str],
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
    with_changed_flags: bool,
) -> (Vec<Option<usize>>, Vec<bool>) {
    let (prefix, suffix, old_mid, new_mid) = common_affix_trim(old, new);
    let mut map = vec![None; new.len()];
    let mut flags = vec![false; new.len()];
    for (i, slot) in map.iter_mut().take(prefix).enumerate() {
        *slot = Some(i);
    }
    for i in 0..suffix {
        let new_idx = new.len() - suffix + i;
        let old_idx = old.len() - suffix + i;
        map[new_idx] = Some(old_idx);
    }
    if old_mid.is_empty() && new_mid.is_empty() {
        return (map, flags);
    }
    // `--indent-heuristic` / `diff.indentHeuristic` shifts ambiguous add/delete
    // runs like Git's xdiff, which changes which lines blame attributes (t4061).
    let ops = if diff_algorithm == BlameDiffAlgorithm::Myers {
        crate::diff::line_diff_ops_git_xdiff_compacted(old_mid, new_mid, indent_heuristic)
    } else {
        crate::diff_indent_heuristic::diff_slice_ops_compacted(
            old_mid,
            new_mid,
            diff_algorithm.to_similar(),
            indent_heuristic,
        )
    };
    let mid_map = crate::diff_indent_heuristic::map_new_to_old_from_ops(&ops, new_mid.len());
    for (j, parent) in mid_map.into_iter().enumerate() {
        map[prefix + j] = parent.map(|idx| idx + prefix);
    }
    if with_changed_flags {
        let mid_flags =
            crate::diff_indent_heuristic::new_line_changed_flags_from_ops(&ops, new_mid.len());
        for (j, flag) in mid_flags.into_iter().enumerate() {
            flags[prefix + j] = flag;
        }
    }
    (map, flags)
}

/// Greedily map unresolved new lines to identical old lines (lowest use count, then distance).
fn fill_exact_text_relocate(
    old: &[&str],
    new: &[&str],
    map: &mut [Option<usize>],
    used_old: &mut [usize],
) {
    let mut by_content: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, line) in old.iter().enumerate() {
        by_content.entry(*line).or_default().push(idx);
    }

    let mut windowed: Vec<usize> = Vec::new();
    for new_idx in 0..map.len() {
        if map[new_idx].is_some() {
            continue;
        }
        let Some(candidates) = by_content.get(new[new_idx]) else {
            continue;
        };
        let mut best: Option<(usize, usize, usize)> = None;
        let scan: &[usize] = if candidates.len() > 32 {
            windowed.clear();
            const WINDOW: usize = 768;
            let lo = new_idx.saturating_sub(WINDOW);
            let hi = new_idx.saturating_add(WINDOW);
            windowed.extend(
                candidates
                    .iter()
                    .copied()
                    .filter(|&idx| (lo..=hi).contains(&idx)),
            );
            if windowed.is_empty() {
                candidates
            } else {
                &windowed
            }
        } else {
            candidates
        };
        for &old_idx in scan {
            let candidate = (used_old[old_idx], old_idx.abs_diff(new_idx), old_idx);
            if best.is_none_or(|b| candidate < b) {
                best = Some(candidate);
            }
        }
        if let Some((_, _, old_idx)) = best {
            map[new_idx] = Some(old_idx);
            used_old[old_idx] += 1;
        }
    }
}

/// Recover line mappings by exact text match when Myers left a line unresolved.
///
/// Used during blame to follow moved but unchanged lines without pairing rewrites.
pub fn build_exact_relocate_line_map(
    old: &[&str],
    new: &[&str],
    exact_map: &[Option<usize>],
) -> Vec<Option<usize>> {
    let mut fuzzy_map = exact_map.to_vec();
    let mut used_old = vec![0usize; old.len()];
    for old_idx in fuzzy_map.iter().flatten() {
        if *old_idx < used_old.len() {
            used_old[*old_idx] += 1;
        }
    }

    fill_exact_text_relocate(old, new, &mut fuzzy_map, &mut used_old);

    fuzzy_map
}

/// Decide which child lines stop blame on the current commit (system `git blame` parity).
///
/// Uses the compacted diff equal-map, exact-text relocation, LIS reposition tie-break, and
/// the rule that lines re-inserted at the same index without a diff equal mapping stay on
/// the current commit.
fn blame_stops_on_current_commit(
    diff_map: &[Option<usize>],
    relocate_map: &[Option<usize>],
    xdiff_changed: &[bool],
) -> Vec<bool> {
    let reposition = blame_reposition_on_current_commit(relocate_map);
    let mut stop = vec![false; diff_map.len()];
    for child_idx in 0..diff_map.len() {
        if reposition.get(child_idx).copied().unwrap_or(false) {
            stop[child_idx] = true;
            continue;
        }
        if xdiff_changed.get(child_idx).copied().unwrap_or(false) {
            if let Some(parent_idx) = diff_map[child_idx] {
                if parent_idx == child_idx {
                    stop[child_idx] = true;
                }
            }
        }
        if stop[child_idx] {
            continue;
        }
        if diff_map[child_idx].is_some() {
            continue;
        }
        match relocate_map.get(child_idx).and_then(|p| *p) {
            None => stop[child_idx] = true,
            Some(parent_idx) if parent_idx == child_idx => {
                stop[child_idx] = xdiff_changed.get(child_idx).copied().unwrap_or(false);
            }
            Some(_) => {}
        }
    }
    stop
}

/// Prefix maximum on a 1-indexed Fenwick tree (max `len`/`arg` pairs on `[1, idx]`).
#[allow(clippy::manual_isolate_lowest_one)]
fn fenwick_max_prefix(len: &[usize], arg: &[usize], idx: usize) -> (usize, usize) {
    let mut pos = idx;
    let mut best_len = 0usize;
    let mut best_i = usize::MAX;
    while pos > 0 {
        if len[pos] > best_len {
            best_len = len[pos];
            best_i = arg[pos];
        }
        pos &= pos - 1;
    }
    (best_len, best_i)
}

#[allow(clippy::manual_isolate_lowest_one)]
fn fenwick_max_set(len: &mut [usize], arg: &mut [usize], idx: usize, value: usize, i: usize) {
    let mut pos = idx;
    while pos < len.len() {
        if value > len[pos] {
            len[pos] = value;
            arg[pos] = i;
        }
        pos += pos & pos.wrapping_neg();
    }
}

/// Longest-increasing-subsequence tie-break for lines that moved but stayed text-identical.
fn blame_reposition_on_current_commit(line_map: &[Option<usize>]) -> Vec<bool> {
    let n = line_map.len();
    let pairs: Vec<(usize, usize)> = line_map
        .iter()
        .enumerate()
        .filter_map(|(child, parent)| parent.map(|p| (child, p)))
        .collect();

    if pairs.is_empty() || pairs.iter().all(|&(c, p)| c == p) {
        return vec![false; n];
    }

    let mut in_lis = vec![false; n];
    if !pairs.is_empty() {
        let len = pairs.len();
        let mut lis_len = vec![1usize; len];
        let mut pred = vec![usize::MAX; len];
        let max_parent = pairs.iter().map(|(_, p)| *p).max().unwrap_or(0);
        let fw_size = max_parent + 2;
        let mut fw_len = vec![0usize; fw_size];
        let mut fw_idx = vec![usize::MAX; fw_size];
        for i in 0..len {
            let p = pairs[i].1;
            let (prev_len, prev_i) = fenwick_max_prefix(&fw_len, &fw_idx, p);
            if prev_len > 0 {
                pred[i] = prev_i;
            }
            lis_len[i] = prev_len + 1;
            fenwick_max_set(&mut fw_len, &mut fw_idx, p + 1, lis_len[i], i);
        }
        let max_lis = *lis_len.iter().max().unwrap_or(&1);
        let end = (0..len)
            .filter(|&i| lis_len[i] == max_lis)
            .max_by_key(|&i| pairs[i].1)
            .unwrap_or(0);

        let mut cur = end;
        loop {
            in_lis[pairs[cur].0] = true;
            if pred[cur] == usize::MAX {
                break;
            }
            cur = pred[cur];
        }
    }

    let mut blame_current = vec![false; n];
    for (child_idx, parent_idx) in line_map
        .iter()
        .enumerate()
        .filter_map(|(c, p)| p.map(|p| (c, p)))
    {
        if parent_idx == child_idx {
            continue;
        }
        if !in_lis[child_idx] {
            blame_current[child_idx] = true;
        }
    }
    blame_current
}

/// Like [`build_line_map`], but recover additional correspondences for moved/rewritten lines.
pub fn build_fuzzy_line_map(
    old: &[&str],
    new: &[&str],
    exact_map: &[Option<usize>],
) -> Vec<Option<usize>> {
    let mut fuzzy_map = exact_map.to_vec();
    let mut used_old = vec![0usize; old.len()];
    for old_idx in fuzzy_map.iter().flatten() {
        if *old_idx < used_old.len() {
            used_old[*old_idx] += 1;
        }
    }

    // First, greedily recover exact-text matches among unresolved lines.
    // This is important for reorders where Myers may not anchor every moved
    // line and fuzzy similarity can otherwise pair the wrong include.
    fill_exact_text_relocate(old, new, &mut fuzzy_map, &mut used_old);

    let mut anchors: Vec<(usize, usize)> = exact_map
        .iter()
        .enumerate()
        .filter_map(|(new_idx, old_idx)| old_idx.map(|old| (new_idx, old)))
        .collect();
    anchors.sort_unstable();

    let mut prev_new = usize::MAX;
    let mut prev_old = usize::MAX;
    for (next_new, next_old) in anchors
        .iter()
        .copied()
        .chain(std::iter::once((new.len(), old.len())))
    {
        let new_start = if prev_new == usize::MAX {
            0
        } else {
            prev_new + 1
        };
        let new_end = next_new;
        let old_start = if prev_old == usize::MAX {
            0
        } else {
            prev_old + 1
        };
        let old_end = next_old;

        if new_start < new_end && old_start < old_end {
            let segment_matches =
                fuzzy_match_segment(old, old_start, old_end, new, new_start, new_end);
            for (new_idx, old_idx) in segment_matches {
                if fuzzy_map[new_idx].is_none() {
                    fuzzy_map[new_idx] = Some(old_idx);
                    used_old[old_idx] += 1;
                }
            }
        }

        prev_new = next_new;
        prev_old = next_old;
    }

    // Context-aware recovery for split/expanded lines:
    // if an unresolved line sits between mapped neighbors, prefer mapping
    // to those neighboring source lines when there is meaningful overlap.
    for new_idx in 0..fuzzy_map.len() {
        if fuzzy_map[new_idx].is_some() {
            continue;
        }
        let prev_old = (0..new_idx).rev().find_map(|i| fuzzy_map[i]);
        let next_old = ((new_idx + 1)..fuzzy_map.len()).find_map(|i| fuzzy_map[i]);

        let mut candidates = Vec::new();
        if let Some(o) = prev_old {
            candidates.push(o);
        }
        if let Some(o) = next_old {
            if candidates.last().copied() != Some(o) {
                candidates.push(o);
            }
        }

        let mut best: Option<(f64, usize)> = None;
        for old_idx in candidates {
            if old_idx >= old.len() {
                continue;
            }
            let (sim, lcs) = line_similarity_and_lcs(old[old_idx], new[new_idx]);
            let exact_text = old[old_idx].trim() == new[new_idx].trim();
            // Keep this narrow: strong overlap or exact text only.
            if !exact_text && lcs < 6 && !(sim >= 0.35 && lcs >= 3) {
                continue;
            }

            let mut score = lcs as f64 + sim * 10.0;
            score -= 0.2 * used_old[old_idx] as f64;

            if let (Some(lo), Some(hi)) = (prev_old, next_old) {
                if lo <= hi && (old_idx < lo || old_idx > hi) {
                    score -= 3.0;
                }
            }

            if best.is_none_or(|(best_score, best_old)| {
                score > best_score
                    || ((score - best_score).abs() < 1e-9
                        && old_idx.abs_diff(new_idx) < best_old.abs_diff(new_idx))
            }) {
                best = Some((score, old_idx));
            }
        }

        if let Some((_, old_idx)) = best {
            fuzzy_map[new_idx] = Some(old_idx);
            used_old[old_idx] += 1;
        }
    }

    // Final best-effort fill for unresolved lines. This handles cases where
    // anchor segmentation leaves an empty old-range but we still want to
    // pass through ignored commits by similarity.
    for new_idx in 0..fuzzy_map.len() {
        if fuzzy_map[new_idx].is_some() {
            continue;
        }
        let prev_old = (0..new_idx).rev().find_map(|i| fuzzy_map[i]);
        let next_old = ((new_idx + 1)..fuzzy_map.len()).find_map(|i| fuzzy_map[i]);

        let mut best: Option<(f64, usize)> = None;
        for old_idx in 0..old.len() {
            let (sim, lcs) = line_similarity_and_lcs(old[old_idx], new[new_idx]);
            let exact_text = old[old_idx].trim() == new[new_idx].trim();
            let new_len = new[new_idx].trim().chars().count();
            if !exact_text && (new_len < 3 || sim < 0.45 || lcs < 2) {
                continue;
            }

            let mut score = sim;
            score -= 0.004 * old_idx.abs_diff(new_idx) as f64;
            score -= 0.08 * used_old[old_idx] as f64;

            // Encourage monotonic local ordering when neighboring anchors
            // are themselves ordered; avoid forcing this for reorders.
            if let (Some(lo), Some(hi)) = (prev_old, next_old) {
                if lo <= hi {
                    if old_idx < lo {
                        score -= 0.20 * (lo - old_idx) as f64;
                    } else if old_idx > hi {
                        score -= 0.20 * (old_idx - hi) as f64;
                    }
                }
            } else if let Some(lo) = prev_old {
                if old_idx < lo {
                    score -= 0.20 * (lo - old_idx) as f64;
                }
            } else if let Some(hi) = next_old {
                if old_idx > hi {
                    score -= 0.20 * (old_idx - hi) as f64;
                }
            }

            if best.is_none_or(|(best_score, best_idx)| {
                score > best_score
                    || ((score - best_score).abs() < 1e-9
                        && old_idx.abs_diff(new_idx) < best_idx.abs_diff(new_idx))
            }) {
                best = Some((score, old_idx));
            }
        }

        if let Some((_, old_idx)) = best {
            fuzzy_map[new_idx] = Some(old_idx);
            used_old[old_idx] += 1;
        }
    }

    fuzzy_map
}

fn fuzzy_match_segment(
    old: &[&str],
    old_start: usize,
    old_end: usize,
    new: &[&str],
    new_start: usize,
    new_end: usize,
) -> Vec<(usize, usize)> {
    let m = old_end.saturating_sub(old_start);
    let n = new_end.saturating_sub(new_start);
    if m == 0 || n == 0 {
        return Vec::new();
    }

    // DP over new-lines where the state tracks the last matched old-line.
    // State 0 means "no old line selected yet", state s>0 means old index s-1.
    // Transitions keep order (non-decreasing old index), but allow reusing
    // the same old line for multiple split lines in the new content.
    let states = m + 1;
    let neg_inf = f64::NEG_INFINITY;
    let mut dp = vec![neg_inf; states];
    dp[0] = 0.0;

    let mut back_prev = vec![vec![usize::MAX; states]; n + 1];
    let mut back_pick = vec![vec![None; states]; n + 1];

    for j in 0..n {
        let mut next_dp = vec![neg_inf; states];

        for state in 0..states {
            let base = dp[state];
            if !base.is_finite() {
                continue;
            }

            // Option 1: do not match this new line.
            if base > next_dp[state] {
                next_dp[state] = base;
                back_prev[j + 1][state] = state;
                back_pick[j + 1][state] = None;
            }

            // Option 2: match this new line to some old line >= last matched.
            let start_k = if state == 0 { 0 } else { state - 1 };
            for k in start_k..m {
                let old_idx = old_start + k;
                let new_idx = new_start + j;
                let (sim, lcs) = line_similarity_and_lcs(old[old_idx], new[new_idx]);
                let exact_text = old[old_idx].trim() == new[new_idx].trim();
                let new_len = new[new_idx].trim().chars().count();
                if !exact_text && (new_len < 3 || sim < 0.45 || lcs < 2) {
                    continue;
                }

                // Slight locality bias to stabilize tie-breaking.
                let distance = k.abs_diff(j) as f64;
                let score = base + sim - 0.002 * distance;
                let next_state = k + 1;
                if score > next_dp[next_state] {
                    next_dp[next_state] = score;
                    back_prev[j + 1][next_state] = state;
                    back_pick[j + 1][next_state] = Some(k);
                }
            }
        }

        dp = next_dp;
    }

    let mut best_state = 0usize;
    for state in 1..states {
        if dp[state] > dp[best_state] {
            best_state = state;
        }
    }

    let mut matches = Vec::new();
    let mut state = best_state;
    for j in (1..=n).rev() {
        if let Some(k) = back_pick[j][state] {
            matches.push((new_start + (j - 1), old_start + k));
        }
        let prev = back_prev[j][state];
        if prev == usize::MAX {
            break;
        }
        state = prev;
    }
    matches.reverse();
    matches
}

fn line_similarity_and_lcs(a: &str, b: &str) -> (f64, usize) {
    let a = a.trim();
    let b = b.trim();
    if a.is_empty() || b.is_empty() {
        return (0.0, 0);
    }
    if a == b {
        return (1.0, a.chars().count());
    }

    let a = a.to_ascii_lowercase();
    let b = b.to_ascii_lowercase();
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = b_chars.len();
    if a_chars.is_empty() || b_chars.is_empty() {
        return (0.0, 0);
    }

    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];
    for i in 1..=a_chars.len() {
        for j in 1..=n {
            curr[j] = if a_chars[i - 1] == b_chars[j - 1] {
                prev[j - 1] + 1
            } else {
                prev[j].max(curr[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
        curr.fill(0);
    }

    let lcs = prev[n];
    let sim = (2.0 * lcs as f64) / (a_chars.len() as f64 + b_chars.len() as f64);
    (sim, lcs)
}

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use crate::commit_graph_file::{BloomPrecheck, CommitGraphChain};
use crate::config::ConfigSet;
use crate::crlf::{
    convert_to_worktree_eager, get_file_attrs, load_gitattributes, load_gitattributes_from_index,
    ConversionConfig, GitAttributes,
};
use crate::diff::{detect_renames, diff_trees, DiffEntry, DiffStatus};
use crate::diff_rename_parallel::{diff_rename_parallel_options, parallel_map_indices};
use crate::error::{Error as LibError, Result};
use crate::objects::{
    parse_commit, parse_commit_tree_and_parents, parse_tree, Object, ObjectId, ObjectKind,
};
use crate::odb::Odb;
use crate::parallel::{ParallelOptions, ThreadPolicy};
use crate::repo::Repository;
use crate::wildmatch::wildmatch;

/// Cached copy-detection blame lookup: source path, lines by content, use counts.
type CopySourceBlameCache = (
    String,
    HashMap<String, Vec<BlameLine>>,
    HashMap<String, usize>,
);

/// Hook the CLI installs so blame can lazily hydrate missing objects from a
/// promisor remote (partial clone). The lib does no transport itself; the CLI
/// supplies a function that performs the fetch. Install via
/// [`RepoCaches::set_promisor_hydrate_hook`](crate::repo_caches::RepoCaches::set_promisor_hydrate_hook).
pub type PromisorHydrateHook = fn(&Repository, ObjectId);

/// A single line attribution produced by [`compute_blame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// Commit that last modified this line.
    pub oid: ObjectId,
    /// 1-based line number in the final file.
    pub final_lineno: usize,
    /// 1-based line number in the originating commit's version of the file.
    pub orig_lineno: usize,
    /// Text of the line in the final file.
    pub content: String,
    /// Source path when copy detection attributed the line from another file.
    pub source_file: Option<String>,
    /// Line attribution passed through an ignored revision.
    pub ignored: bool,
    /// Line could not be blamed past an ignored revision.
    pub unblamable: bool,
    /// Line content came from an external overlay rather than the blamed blob.
    pub external_contents: bool,
}

/// Resolve a file path through nested trees to get the blob OID + mode.
fn resolve_path_in_tree_entry(
    odb: &Odb,
    tree_oid: &ObjectId,
    path: &str,
) -> Result<Option<(ObjectId, u32)>> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut current = *tree_oid;

    for (i, part) in parts.iter().enumerate() {
        let obj = read_object_for_blame(odb, &current, None)?;
        let entries = parse_tree(&obj.data)?;
        match entries
            .iter()
            .find(|e| String::from_utf8_lossy(&e.name) == *part)
        {
            Some(e) if i == parts.len() - 1 => {
                if e.mode == 0o040000 {
                    return Ok(None);
                }
                return Ok(Some((e.oid, e.mode)));
            }
            Some(e) if e.mode == 0o040000 => current = e.oid,
            Some(_) => return Ok(None),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Split content into lines. A final line without a trailing newline is still a line
/// (matches git blame / `wc -l` + 1 semantics in upstream tests).
fn content_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&str> = s.split('\n').collect();
    if out.last() == Some(&"") {
        out.pop();
    }
    out
}

/// Each line being tracked through history.
/// `final_lineno` is the 1-based line number in the target file.
/// `current_idx` is the 0-based index in the current version being examined.
#[derive(Debug, Clone)]
struct TrackedLine {
    final_lineno: usize,
    current_idx: usize,
    ignored: bool,
    source_path: Option<String>,
}

/// Cached config and attributes for optional textconv during blame.
#[derive(Debug, Clone)]
pub struct BlameTextconvContext {
    /// Repository configuration snapshot.
    pub config: ConfigSet,
    conversion: ConversionConfig,
    /// Parsed `.gitattributes` rules for the repository.
    pub attrs: GitAttributes,
    diff_attrs: Vec<DiffAttrRule>,
    filter_process: std::sync::Arc<crate::repo_caches::RepoCaches>,
}

impl BlameTextconvContext {
    /// Build textconv context from the open repository (config, attributes, filters).
    pub fn new(repo: &Repository) -> Self {
        let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
        let conversion = ConversionConfig::from_config(&config);
        let attrs = load_attr_rules(repo);
        let diff_attrs = load_diff_attr_rules(repo);
        Self {
            config,
            conversion,
            attrs,
            diff_attrs,
            filter_process: std::sync::Arc::clone(repo.caches()),
        }
    }

    fn filters(&self) -> &crate::filter_process::FilterProcessState {
        self.filter_process.filters()
    }
}

#[derive(Debug, Clone)]
struct DiffAttrRule {
    pattern: String,
    value: DiffAttrValue,
}

#[derive(Debug, Clone)]
enum DiffAttrValue {
    Unset,
    Set,
    Driver(String),
}

fn load_attr_rules(repo: &Repository) -> GitAttributes {
    if let Some(work_tree) = repo.work_tree() {
        let rules = load_gitattributes(work_tree);
        if !rules.is_empty() {
            return rules;
        }
    }

    if let Ok(index) = repo.load_index() {
        return load_gitattributes_from_index(&index, repo.odb());
    }

    Vec::new()
}

fn load_diff_attr_rules(repo: &Repository) -> Vec<DiffAttrRule> {
    let mut rules = Vec::new();

    if let Some(work_tree) = repo.work_tree() {
        parse_diff_attr_file(&work_tree.join(".gitattributes"), &mut rules);
        parse_diff_attr_file(&work_tree.join(".git/info/attributes"), &mut rules);
    }

    if rules.is_empty() {
        if let Ok(index) = repo.load_index() {
            if let Some(entry) = index.get(b".gitattributes", 0) {
                if let Ok(obj) = repo.odb().read(&entry.oid) {
                    if let Ok(content) = String::from_utf8(obj.data) {
                        parse_diff_attr_content(&content, &mut rules);
                    }
                }
            }
        }
        parse_diff_attr_file(&repo.git_dir().join("info/attributes"), &mut rules);
    }

    rules
}

/// Read an object for blame, optionally invoking a promisor hydration hook on miss.
pub fn read_object_for_blame(
    odb: &Odb,
    oid: &ObjectId,
    repo: Option<&Repository>,
) -> Result<Object> {
    match odb.read(oid) {
        Ok(obj) => Ok(obj),
        Err(LibError::ObjectNotFound(_)) => {
            if let Some(repo) = repo {
                if let Some(hook) = repo.promisor_hydrate_hook() {
                    hook(repo, *oid);
                    return odb.read(oid);
                }
            }
            Err(LibError::ObjectNotFound(oid.to_hex()))
        }
        Err(err) => Err(err),
    }
}

fn parse_diff_attr_file(path: &Path, rules: &mut Vec<DiffAttrRule>) {
    if let Ok(content) = std::fs::read_to_string(path) {
        parse_diff_attr_content(&content, rules);
    }
}

fn parse_diff_attr_content(content: &str, rules: &mut Vec<DiffAttrRule>) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(pattern) = parts.next() else {
            continue;
        };

        let mut value: Option<DiffAttrValue> = None;
        for token in parts {
            if token == "binary" || token == "-diff" {
                value = Some(DiffAttrValue::Unset);
            } else if token == "diff" {
                value = Some(DiffAttrValue::Set);
            } else if let Some(driver) = token.strip_prefix("diff=") {
                value = Some(DiffAttrValue::Driver(driver.to_owned()));
            }
        }

        if let Some(value) = value {
            rules.push(DiffAttrRule {
                pattern: pattern.to_owned(),
                value,
            });
        }
    }
}

fn resolve_textconv_command(ctx: &BlameTextconvContext, path: &str) -> Option<String> {
    let mut selected: Option<DiffAttrValue> = None;
    for rule in &ctx.diff_attrs {
        if diff_attr_pattern_matches(&rule.pattern, path) {
            selected = Some(rule.value.clone());
        }
    }

    match selected {
        Some(DiffAttrValue::Driver(driver)) => ctx.config.get(&format!("diff.{driver}.textconv")),
        _ => None,
    }
}

fn diff_attr_pattern_matches(pattern: &str, path: &str) -> bool {
    if pattern.contains('/') {
        return wildmatch(pattern.as_bytes(), path.as_bytes(), 0);
    }
    let basename = path.rsplit('/').next().unwrap_or(path);
    wildmatch(pattern.as_bytes(), basename.as_bytes(), 0)
}

fn is_regular_mode(mode: u32) -> bool {
    mode & 0o170000 == 0o100000
}

fn run_textconv_command(command: &str, input_data: &[u8]) -> Result<Vec<u8>> {
    use crate::command_runner::{run_sh_dash_c, system_command_runner, CommandStdin, CommandStdio};
    let temp_path = create_temp_textconv_file(input_data)?;
    let quoted = shell_quote(temp_path.to_string_lossy().as_ref());
    let shell_command = format!("{command} {quoted}");

    let output = run_sh_dash_c(
        system_command_runner().as_ref(),
        &shell_command,
        None,
        CommandStdin::Null,
        CommandStdio::Inherit,
    )
    .map_err(|e| LibError::Message(format!("running textconv command '{command}': {e}")))?;

    let _ = std::fs::remove_file(&temp_path);

    if !output.status.success {
        return Err(LibError::Message(format!(
            "textconv command exited with status {}",
            output.status.code.unwrap_or(-1)
        )));
    }

    Ok(output.stdout)
}

fn create_temp_textconv_file(data: &[u8]) -> Result<std::path::PathBuf> {
    let pid = std::process::id();
    for attempt in 0..32u32 {
        let now = crate::git_date::tm::process_wall_clock_sec() as u128;
        let path = std::env::temp_dir().join(format!("grit-blame-textconv-{pid}-{now}-{attempt}"));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                file.write_all(data)?;
                return Ok(path);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(LibError::Message(
        "failed to create temporary textconv input file".to_string(),
    ))
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn read_blob_content_for_blame(
    odb: &Odb,
    oid: &ObjectId,
    path: &str,
    mode: u32,
    textconv_ctx: Option<&BlameTextconvContext>,
    use_textconv: bool,
) -> Result<String> {
    let obj = read_object_for_blame(odb, oid, None)?;
    if obj.kind != ObjectKind::Blob {
        return Err(LibError::Message("expected blob object".to_string()));
    }

    if !use_textconv || !is_regular_mode(mode) {
        return Ok(String::from_utf8_lossy(&obj.data).into_owned());
    }

    let Some(ctx) = textconv_ctx else {
        return Ok(String::from_utf8_lossy(&obj.data).into_owned());
    };
    let Some(command) = resolve_textconv_command(ctx, path) else {
        return Ok(String::from_utf8_lossy(&obj.data).into_owned());
    };

    let attrs = get_file_attrs(&ctx.attrs, path, false, &ctx.config);
    let oid_hex = oid.to_string();
    let worktree_data = convert_to_worktree_eager(
        &obj.data,
        path,
        &ctx.conversion,
        &attrs,
        Some(&oid_hex),
        None,
        Some(ctx.filters()),
    )
    .map_err(LibError::Filter)?;
    let converted = run_textconv_command(&command, &worktree_data)
        .or_else(|_| run_textconv_command(&command, &obj.data))?;
    Ok(String::from_utf8_lossy(&converted).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BlobCacheKey {
    oid: ObjectId,
    path: String,
    mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PathCacheKey {
    tree: ObjectId,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PairMapCacheKey {
    parent: ObjectId,
    parent_path: String,
    parent_mode: u32,
    child: ObjectId,
    child_path: String,
    child_mode: u32,
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
    fuzzy: bool,
    include_stop_blame: bool,
}

#[derive(Clone)]
struct PairMapCacheValue {
    diff_line_map: Arc<Vec<Option<usize>>>,
    line_map: Arc<Vec<Option<usize>>>,
    stop_blame_on_current: Arc<Vec<bool>>,
}

/// Per-blame caches for blob lines, tree path resolution, and parent/child line maps.
struct BlameSession<'a> {
    odb: &'a Odb,
    textconv_ctx: Option<&'a BlameTextconvContext>,
    use_textconv: bool,
    copy_score_parallel: ParallelOptions,
    commit_graph: Option<Arc<CommitGraphChain>>,
    bloom_pathspec: Vec<String>,
    /// Blob and mode at the blamed path for commits already visited in this walk.
    blob_at_commit: HashMap<ObjectId, (ObjectId, u32)>,
    blob_lines: HashMap<BlobCacheKey, Arc<Vec<String>>>,
    path_entries: HashMap<PathCacheKey, Option<(ObjectId, u32)>>,
    pair_maps: HashMap<PairMapCacheKey, PairMapCacheValue>,
}

impl<'a> BlameSession<'a> {
    fn new(
        odb: &'a Odb,
        textconv_ctx: Option<&'a BlameTextconvContext>,
        use_textconv: bool,
        thread_policy: &ThreadPolicy,
    ) -> Self {
        let config = textconv_ctx
            .map(|ctx| ctx.config.clone())
            .unwrap_or_default();
        Self {
            odb,
            textconv_ctx,
            use_textconv,
            copy_score_parallel: diff_rename_parallel_options(&config, thread_policy),
            commit_graph: None,
            bloom_pathspec: Vec::new(),
            blob_at_commit: HashMap::new(),
            blob_lines: HashMap::new(),
            path_entries: HashMap::new(),
            pair_maps: HashMap::new(),
        }
    }

    fn set_bloom_path(&mut self, path: &str) {
        if self.bloom_pathspec.len() == 1 && self.bloom_pathspec[0] == path {
            return;
        }
        self.bloom_pathspec.clear();
        self.bloom_pathspec.push(path.to_owned());
        self.blob_at_commit.clear();
    }

    fn record_commit_blob(&mut self, commit: ObjectId, blob: ObjectId, mode: u32) {
        self.blob_at_commit.insert(commit, (blob, mode));
    }

    fn blob_at_commit_path(&self, commit: ObjectId) -> Option<(ObjectId, u32)> {
        self.blob_at_commit.get(&commit).copied()
    }

    fn resolve_blob_at_commit(
        &mut self,
        odb: &Odb,
        commit: ObjectId,
        path: &str,
        commit_cache: &mut HashMap<ObjectId, Arc<BlameCommitMeta>>,
    ) -> Result<Option<(ObjectId, u32)>> {
        if let Some(entry) = self.blob_at_commit_path(commit) {
            return Ok(Some(entry));
        }
        let meta = get_blame_commit(odb, commit, commit_cache)?;
        let resolved = self.resolve_path_in_tree(meta.tree, path)?;
        if let Some((blob, mode)) = resolved {
            self.record_commit_blob(commit, blob, mode);
        }
        Ok(resolved)
    }

    fn line_refs(lines: &Arc<Vec<String>>) -> Vec<&str> {
        lines.iter().map(String::as_str).collect()
    }

    fn load_blob_lines(
        &mut self,
        oid: ObjectId,
        path: &str,
        mode: u32,
    ) -> Result<Arc<Vec<String>>> {
        let key = BlobCacheKey {
            oid,
            path: path.to_owned(),
            mode,
        };
        if let Some(lines) = self.blob_lines.get(&key) {
            return Ok(Arc::clone(lines));
        }
        let content = read_blob_content_for_blame(
            self.odb,
            &oid,
            path,
            mode,
            self.textconv_ctx,
            self.use_textconv,
        )?;
        let parsed: Vec<String> = content_lines(&content)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let lines = Arc::new(parsed);
        self.blob_lines.insert(key, Arc::clone(&lines));
        Ok(lines)
    }

    fn resolve_path_in_tree(
        &mut self,
        tree_oid: ObjectId,
        path: &str,
    ) -> Result<Option<(ObjectId, u32)>> {
        let key = PathCacheKey {
            tree: tree_oid,
            path: path.to_owned(),
        };
        if let Some(entry) = self.path_entries.get(&key) {
            return Ok(*entry);
        }
        let resolved = resolve_path_in_tree_entry(self.odb, &tree_oid, path)?;
        self.path_entries.insert(key, resolved);
        Ok(resolved)
    }

    #[allow(clippy::too_many_arguments)]
    fn parent_child_line_maps(
        &mut self,
        parent_oid: ObjectId,
        parent_path: &str,
        parent_mode: u32,
        child_oid: ObjectId,
        child_path: &str,
        child_mode: u32,
        par_lines: &Arc<Vec<String>>,
        cur_lines: &Arc<Vec<String>>,
        diff_algorithm: BlameDiffAlgorithm,
        indent_heuristic: bool,
        is_ignored: bool,
        include_stop_blame: bool,
    ) -> PairMapCacheValue {
        let key = PairMapCacheKey {
            parent: parent_oid,
            parent_path: parent_path.to_owned(),
            parent_mode,
            child: child_oid,
            child_path: child_path.to_owned(),
            child_mode,
            diff_algorithm,
            indent_heuristic,
            fuzzy: is_ignored,
            include_stop_blame,
        };
        if let Some(maps) = self.pair_maps.get(&key) {
            return maps.clone();
        }

        let identity_map = parent_oid == child_oid && par_lines.len() == cur_lines.len();
        let par_refs = Self::line_refs(par_lines);
        let cur_refs = Self::line_refs(cur_lines);
        let (diff_line_map, xdiff_changed) = if identity_map {
            (
                (0..cur_lines.len()).map(Some).collect(),
                vec![false; cur_lines.len()],
            )
        } else if include_stop_blame {
            build_line_map_with_changed_flags(
                &par_refs,
                &cur_refs,
                diff_algorithm,
                indent_heuristic,
                true,
            )
        } else {
            (
                build_line_map(&par_refs, &cur_refs, diff_algorithm, indent_heuristic),
                Vec::new(),
            )
        };
        let (line_map, stop_blame_on_current) = if include_stop_blame {
            let mut line_map = build_exact_relocate_line_map(&par_refs, &cur_refs, &diff_line_map);
            if is_ignored {
                line_map = build_fuzzy_line_map(&par_refs, &cur_refs, &line_map);
            }
            let stop = if par_lines.len() == cur_lines.len() {
                blame_stops_on_current_commit(&diff_line_map, &line_map, &xdiff_changed)
            } else {
                Vec::new()
            };
            (line_map, stop)
        } else {
            (Vec::new(), Vec::new())
        };
        let value = PairMapCacheValue {
            diff_line_map: Arc::new(diff_line_map),
            line_map: Arc::new(line_map),
            stop_blame_on_current: Arc::new(stop_blame_on_current),
        };
        self.pair_maps.insert(key, value.clone());
        value
    }
}

/// Rename similarity threshold for blame history walks (Git default `-M` percentage).
const BLAME_RENAME_SIMILARITY: u32 = 50;

/// Tree and parent linkage for a commit, cached during blame walks.
#[derive(Debug, Clone)]
struct BlameCommitMeta {
    tree: ObjectId,
    parents: Vec<ObjectId>,
}

/// Walk history and attribute each line in a blob at `start_oid` to a commit.
///
/// When `line_range` is set, only lines whose 1-based final line number lies in the range
/// are walked and returned.
///
/// Lower-level building block; prefer [`blame_file`] for repository callers.
#[expect(clippy::too_many_arguments)]
pub fn compute_blame(
    odb: &Odb,
    start_oid: ObjectId,
    file_path: &str,
    ignore_revs: &HashSet<ObjectId>,
    diff_algorithm: BlameDiffAlgorithm,
    textconv_ctx: Option<&BlameTextconvContext>,
    use_textconv: bool,
    copy_depth: usize,
    first_parent_only: bool,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    indent_heuristic: bool,
    line_range: Option<RangeInclusive<usize>>,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<BlameLine>> {
    let mut session = BlameSession::new(odb, textconv_ctx, use_textconv, thread_policy);
    compute_blame_with_session(
        odb,
        start_oid,
        file_path,
        ignore_revs,
        diff_algorithm,
        textconv_ctx,
        use_textconv,
        copy_depth,
        first_parent_only,
        grafts,
        indent_heuristic,
        line_range,
        &mut session,
    )
}

/// Advance `start_oid` along first parents while `path` resolves to `blob_oid`.
fn fast_forward_same_blob(
    odb: &Odb,
    session: &mut BlameSession<'_>,
    mut oid: ObjectId,
    blob_oid: ObjectId,
    path: &str,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    commit_cache: &mut HashMap<ObjectId, Arc<BlameCommitMeta>>,
) -> Result<ObjectId> {
    loop {
        let parents = commit_parents_for_blame(odb, oid, grafts, commit_cache)?;
        let Some(parent_oid) = parents.first().copied() else {
            break;
        };
        if parents.len() == 1
            && session.commit_graph.is_some()
            && bloom_may_skip_to_first_parent(
                session,
                odb,
                oid,
                parent_oid,
                path,
                grafts,
                commit_cache,
            )?
        {
            oid = parent_oid;
            continue;
        }
        let Some((p_blob, _mode)) =
            session.resolve_blob_at_commit(odb, parent_oid, path, commit_cache)?
        else {
            break;
        };
        if p_blob != blob_oid {
            break;
        }
        oid = parent_oid;
    }
    Ok(oid)
}

#[expect(clippy::too_many_arguments)]
fn compute_blame_with_session(
    odb: &Odb,
    start_oid: ObjectId,
    file_path: &str,
    ignore_revs: &HashSet<ObjectId>,
    diff_algorithm: BlameDiffAlgorithm,
    textconv_ctx: Option<&BlameTextconvContext>,
    use_textconv: bool,
    copy_depth: usize,
    first_parent_only: bool,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    indent_heuristic: bool,
    line_range: Option<RangeInclusive<usize>>,
    session: &mut BlameSession<'_>,
) -> Result<Vec<BlameLine>> {
    let mut commit_cache: HashMap<ObjectId, Arc<BlameCommitMeta>> = HashMap::new();
    let start_commit = get_blame_commit(odb, start_oid, &mut commit_cache)?;

    let (blob_oid, blob_mode) = session
        .resolve_path_in_tree(start_commit.tree, file_path)?
        .ok_or_else(|| LibError::Message(format!("file '{file_path}' not found in revision")))?;
    let start_lines = session.load_blob_lines(blob_oid, file_path, blob_mode)?;
    let num_lines = start_lines.len();

    if num_lines == 0 {
        return Ok(Vec::new());
    }

    // Lines still needing attribution
    let mut pending: Vec<TrackedLine> = match &line_range {
        Some(range) => range
            .clone()
            .filter_map(|lineno| {
                if lineno == 0 || lineno > num_lines {
                    return None;
                }
                Some(TrackedLine {
                    final_lineno: lineno,
                    current_idx: lineno - 1,
                    ignored: false,
                    source_path: None,
                })
            })
            .collect(),
        None => (0..num_lines)
            .map(|i| TrackedLine {
                final_lineno: i + 1,
                current_idx: i,
                ignored: false,
                source_path: None,
            })
            .collect(),
    };

    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let mut result: Vec<BlameLine> = Vec::with_capacity(num_lines);
    // Store final content for output
    let final_lines: Vec<String> = (*start_lines).clone();

    let mut current_oid = start_oid;
    let mut current_blob_oid = blob_oid;
    let mut current_blob_mode = blob_mode;
    let mut current_path = file_path.to_string();
    let mut deferred: VecDeque<(ObjectId, ObjectId, u32, String, Vec<TrackedLine>)> =
        VecDeque::new();

    session.set_bloom_path(&current_path);

    'blame_loop: loop {
        if pending.is_empty() {
            if let Some((oid, blob, mode, path, lines)) = deferred.pop_front() {
                current_oid = oid;
                current_blob_oid = blob;
                current_blob_mode = mode;
                current_path = path.clone();
                session.set_bloom_path(&current_path);
                pending = lines;
                continue;
            }
            break;
        }

        session.record_commit_blob(current_oid, current_blob_oid, current_blob_mode);
        let commit = get_blame_commit(odb, current_oid, &mut commit_cache)?;
        let mut parents = commit_parents_for_blame(odb, current_oid, grafts, &mut commit_cache)?;

        if !first_parent_only && !ignore_revs.contains(&current_oid) && parents.len() >= 2 {
            let mut parent_blob_ids: Vec<ObjectId> = Vec::with_capacity(parents.len());
            let mut all_resolved = true;
            for parent_oid in &parents {
                let parent_commit = get_blame_commit(odb, *parent_oid, &mut commit_cache)?;
                match session.resolve_path_in_tree(parent_commit.tree, &current_path)? {
                    Some((blob, _mode)) => parent_blob_ids.push(blob),
                    None => {
                        all_resolved = false;
                        break;
                    }
                }
            }
            if all_resolved
                && parent_blob_ids.len() >= 2
                && parent_blob_ids.windows(2).all(|w| w[0] == w[1])
            {
                parents = vec![parents[0]];
            }
        }

        let is_ignored = ignore_revs.contains(&current_oid);

        // If an ignored merge commit is encountered, try to continue blame
        // through the parent that actually contributed each line.
        if is_ignored && parents.len() > 1 {
            let cur_lines_arc =
                session.load_blob_lines(current_blob_oid, &current_path, current_blob_mode)?;
            let cur_lines = BlameSession::line_refs(&cur_lines_arc);

            let mut parent_lines: Vec<Option<Vec<String>>> = Vec::new();
            let mut parent_blames: Vec<Option<Vec<BlameLine>>> = Vec::new();
            for parent_oid in &parents {
                let parent_commit = get_blame_commit(odb, *parent_oid, &mut commit_cache)?;
                if let Some((p_blob_oid, p_blob_mode)) =
                    session.resolve_path_in_tree(parent_commit.tree, &current_path)?
                {
                    let p_lines_arc =
                        session.load_blob_lines(p_blob_oid, &current_path, p_blob_mode)?;
                    let p_lines = (*p_lines_arc).clone();
                    let p_blame = compute_blame_with_session(
                        odb,
                        *parent_oid,
                        &current_path,
                        ignore_revs,
                        diff_algorithm,
                        textconv_ctx,
                        use_textconv,
                        copy_depth,
                        first_parent_only,
                        grafts,
                        indent_heuristic,
                        None,
                        session,
                    )?;
                    parent_lines.push(Some(p_lines));
                    parent_blames.push(Some(p_blame));
                } else {
                    parent_lines.push(None);
                    parent_blames.push(None);
                }
            }

            for t in pending.drain(..) {
                let idx = t.current_idx;
                let Some(cur_line) = cur_lines.get(idx).copied() else {
                    result.push(BlameLine {
                        oid: current_oid,
                        final_lineno: t.final_lineno,
                        orig_lineno: idx + 1,
                        content: final_lines[t.final_lineno - 1].clone(),
                        source_file: None,
                        ignored: t.ignored,
                        unblamable: true,
                        external_contents: false,
                    });
                    continue;
                };

                let mut picked: Option<BlameLine> = None;
                for i in 0..parents.len() {
                    let Some(lines) = parent_lines.get(i).and_then(|v| v.as_ref()) else {
                        continue;
                    };
                    if idx >= lines.len() || lines[idx] != cur_line {
                        continue;
                    }
                    let Some(blames) = parent_blames.get(i).and_then(|v| v.as_ref()) else {
                        continue;
                    };
                    if let Some(line_blame) = blames.iter().find(|b| b.final_lineno == idx + 1) {
                        picked = Some(line_blame.clone());
                        break;
                    }
                }

                if let Some(pb) = picked {
                    result.push(BlameLine {
                        oid: pb.oid,
                        final_lineno: t.final_lineno,
                        orig_lineno: pb.orig_lineno,
                        content: final_lines[t.final_lineno - 1].clone(),
                        source_file: pb.source_file,
                        ignored: true,
                        unblamable: pb.unblamable,
                        external_contents: false,
                    });
                } else {
                    result.push(BlameLine {
                        oid: current_oid,
                        final_lineno: t.final_lineno,
                        orig_lineno: idx + 1,
                        content: final_lines[t.final_lineno - 1].clone(),
                        source_file: None,
                        ignored: t.ignored,
                        unblamable: true,
                        external_contents: false,
                    });
                }
            }
            if !deferred.is_empty() {
                continue 'blame_loop;
            }
            break 'blame_loop;
        }

        if parents.is_empty() {
            // Root commit — attribute all remaining lines
            for t in pending.drain(..) {
                result.push(BlameLine {
                    oid: current_oid,
                    final_lineno: t.final_lineno,
                    orig_lineno: t.current_idx + 1,
                    content: final_lines[t.final_lineno - 1].clone(),
                    source_file: None,
                    ignored: t.ignored,
                    unblamable: false,
                    external_contents: false,
                });
            }
            if !deferred.is_empty() {
                continue 'blame_loop;
            }
            break 'blame_loop;
        }

        // Merge commits (2+ parents): sequential `pass_blame_to_parent` order from git/blame.c —
        // each line is attributed to the first parent whose version matches after line mapping.
        // Octopus merges and `.git/info/grafts` with many synthetic parents use the same rule.

        // git/blame.c `pass_whole_blame`: when any parent has the same blob as the merge
        // result, pass all remaining lines to that parent without merge diffs.
        if !first_parent_only && !is_ignored && parents.len() >= 2 {
            for parent_oid in &parents {
                if let Some((blob, mode)) = session.resolve_blob_at_commit(
                    odb,
                    *parent_oid,
                    &current_path,
                    &mut commit_cache,
                )? {
                    if blob == current_blob_oid {
                        current_oid = fast_forward_same_blob(
                            odb,
                            session,
                            *parent_oid,
                            current_blob_oid,
                            &current_path,
                            grafts,
                            &mut commit_cache,
                        )?;
                        current_blob_mode = mode;
                        continue 'blame_loop;
                    }
                }
            }
        }

        if !first_parent_only && !is_ignored && parents.len() >= 2 {
            let cur_lines_arc =
                session.load_blob_lines(current_blob_oid, &current_path, current_blob_mode)?;
            let line_count = cur_lines_arc.len();

            let mut parent_blobs: Vec<Option<(ObjectId, ObjectId, u32)>> =
                vec![None; parents.len()];
            let mut par_lines_vec: Vec<Option<Arc<Vec<String>>>> = vec![None; parents.len()];
            let mut maps: Vec<Option<Arc<Vec<Option<usize>>>>> = vec![None; parents.len()];

            let mut buckets: Vec<Vec<TrackedLine>> = vec![Vec::new(); parents.len()];
            let mut attributed: Vec<BlameLine> = Vec::new();

            let mut used_in_parent: Vec<Vec<bool>> = vec![Vec::new(); parents.len()];

            let mut remaining = std::mem::take(&mut pending);
            let mut any_parent_has_path = false;
            for i in 0..parents.len() {
                if remaining.is_empty() {
                    break;
                }
                let p = parents[i];
                let Some((blob, mode)) =
                    session.resolve_blob_at_commit(odb, p, &current_path, &mut commit_cache)?
                else {
                    continue;
                };
                any_parent_has_path = true;
                parent_blobs[i] = Some((p, blob, mode));
                if blob == current_blob_oid {
                    par_lines_vec[i] = Some(Arc::clone(&cur_lines_arc));
                    maps[i] = Some(Arc::new((0..line_count).map(Some).collect::<Vec<_>>()));
                    used_in_parent[i].resize(line_count, false);
                    let mut next_remaining: Vec<TrackedLine> = Vec::new();
                    for t in remaining {
                        if t.current_idx < line_count && !used_in_parent[i][t.current_idx] {
                            used_in_parent[i][t.current_idx] = true;
                            buckets[i].push(TrackedLine {
                                final_lineno: t.final_lineno,
                                current_idx: t.current_idx,
                                ignored: t.ignored,
                                source_path: t.source_path.clone(),
                            });
                        } else {
                            next_remaining.push(t);
                        }
                    }
                    remaining = next_remaining;
                    continue;
                }
                let pl_arc = session.load_blob_lines(blob, &current_path, mode)?;
                let map_algo = diff_algorithm;
                let pair = session.parent_child_line_maps(
                    blob,
                    &current_path,
                    mode,
                    current_blob_oid,
                    &current_path,
                    current_blob_mode,
                    &pl_arc,
                    &cur_lines_arc,
                    map_algo,
                    indent_heuristic,
                    false,
                    false,
                );
                par_lines_vec[i] = Some(pl_arc);
                maps[i] = Some(pair.diff_line_map);

                let mut next_remaining: Vec<TrackedLine> = Vec::new();
                let Some(map) = maps[i].as_ref() else {
                    return Err(LibError::Message(
                        "internal: missing merge parent line map".to_string(),
                    ));
                };
                let Some(par_lines) = par_lines_vec[i].as_ref() else {
                    return Err(LibError::Message(
                        "internal: missing merge parent lines".to_string(),
                    ));
                };
                let parent_line_count = par_lines.len();
                if used_in_parent[i].len() != parent_line_count {
                    used_in_parent[i].resize(parent_line_count, false);
                }
                for t in remaining {
                    let idx = t.current_idx;
                    let cur_line = cur_lines_arc.get(idx).map(String::as_str);
                    let m = map.get(idx).copied().flatten();
                    if let Some(p_idx) = m {
                        let text_ok = par_lines.get(p_idx).map(String::as_str) == cur_line;
                        if text_ok && p_idx < parent_line_count && !used_in_parent[i][p_idx] {
                            used_in_parent[i][p_idx] = true;
                            buckets[i].push(TrackedLine {
                                final_lineno: t.final_lineno,
                                current_idx: p_idx,
                                ignored: t.ignored,
                                source_path: t.source_path.clone(),
                            });
                        } else {
                            next_remaining.push(t);
                        }
                    } else {
                        next_remaining.push(t);
                    }
                }
                remaining = next_remaining;
            }

            if any_parent_has_path {
                for t in remaining {
                    let idx = t.current_idx;
                    attributed.push(BlameLine {
                        oid: current_oid,
                        final_lineno: t.final_lineno,
                        orig_lineno: idx + 1,
                        content: final_lines[t.final_lineno - 1].clone(),
                        source_file: None,
                        ignored: t.ignored,
                        unblamable: false,
                        external_contents: false,
                    });
                }

                for bl in attributed {
                    result.push(bl);
                }

                for i in 1..parents.len() {
                    if buckets[i].is_empty() {
                        continue;
                    }
                    let Some((p, blob, mode)) = parent_blobs[i] else {
                        continue;
                    };
                    deferred.push_back((
                        p,
                        blob,
                        mode,
                        current_path.clone(),
                        std::mem::take(&mut buckets[i]),
                    ));
                }

                if !buckets[0].is_empty() {
                    let Some((p, blob, mode)) = parent_blobs[0] else {
                        continue 'blame_loop;
                    };
                    current_oid = p;
                    current_blob_oid = blob;
                    current_blob_mode = mode;
                    pending = std::mem::take(&mut buckets[0]);
                } else if deferred.is_empty() {
                    break 'blame_loop;
                }
                continue;
            }
        }

        if parents.len() == 1 && session.commit_graph.is_some() {
            let walk_start = current_oid;
            while bloom_may_skip_to_first_parent(
                session,
                odb,
                current_oid,
                parents[0],
                &current_path,
                grafts,
                &mut commit_cache,
            )? {
                current_oid = parents[0];
                parents = commit_parents_for_blame(odb, current_oid, grafts, &mut commit_cache)?;
                if parents.len() != 1 {
                    break;
                }
            }
            if current_oid != walk_start {
                continue 'blame_loop;
            }
        }
        let parent_oid = parents[0];
        let parent_commit = get_blame_commit(odb, parent_oid, &mut commit_cache)?;
        let mut parent_blob_entry =
            session.resolve_blob_at_commit(odb, parent_oid, &current_path, &mut commit_cache)?;
        let mut parent_blob_path = current_path.clone();

        if parent_blob_entry.is_none() && !is_ignored {
            if let Some((renamed_path, renamed_mode)) = find_path_by_oid_in_tree(
                odb,
                &parent_commit.tree,
                &commit.tree,
                &current_blob_oid,
                &current_path,
            )? {
                current_path = renamed_path;
                session.set_bloom_path(&current_path);
                current_oid = parent_oid;
                current_blob_mode = renamed_mode;
                continue;
            }
            if let Some(entry) = find_rename_entry_for_path(
                odb,
                &parent_commit.tree,
                &commit.tree,
                &current_path,
                BLAME_RENAME_SIMILARITY,
            )? {
                let old_path = entry.old_path.clone().unwrap_or_default();
                let p_blob_oid = entry.old_oid;
                let p_blob_mode = mode_from_diff_entry_mode(&entry.old_mode);
                if p_blob_oid == current_blob_oid {
                    current_path = old_path;
                    session.set_bloom_path(&current_path);
                    current_oid = parent_oid;
                    current_blob_mode = p_blob_mode;
                    continue;
                }
                parent_blob_entry = Some((p_blob_oid, p_blob_mode));
                parent_blob_path = old_path;
            }
        }

        match parent_blob_entry {
            None if !is_ignored => {
                // If copy detection is enabled, try to track lines to source
                // files in the parent tree.
                if copy_depth >= 1 {
                    let cur_lines_arc = session.load_blob_lines(
                        current_blob_oid,
                        &current_path,
                        current_blob_mode,
                    )?;
                    let cur_lines = BlameSession::line_refs(&cur_lines_arc);
                    let mut entries = Vec::new();
                    collect_tree_file_entries(odb, &parent_commit.tree, "", &mut entries)?;
                    let mut by_content: HashMap<String, Vec<(String, BlameLine)>> = HashMap::new();
                    for (path, oid, mode) in entries {
                        if path == current_path || !is_regular_mode(mode) {
                            continue;
                        }
                        // Single -C only searches for files that disappeared
                        // in the current commit. Deeper -C levels may search
                        // broader history.
                        if copy_depth == 1
                            && session.resolve_path_in_tree(commit.tree, &path)?.is_some()
                        {
                            continue;
                        }

                        let source_lines_arc = session.load_blob_lines(oid, &path, mode)?;
                        let source_lines = BlameSession::line_refs(&source_lines_arc);
                        let overlap = cur_lines
                            .iter()
                            .filter(|line| source_lines.iter().any(|src| src == *line))
                            .count();
                        if overlap == 0 {
                            continue;
                        }

                        let source_blame = compute_blame_with_session(
                            odb,
                            parent_oid,
                            &path,
                            ignore_revs,
                            diff_algorithm,
                            textconv_ctx,
                            use_textconv,
                            copy_depth.saturating_sub(1),
                            first_parent_only,
                            grafts,
                            indent_heuristic,
                            None,
                            session,
                        )?;
                        for line in source_blame {
                            by_content
                                .entry(line.content.clone())
                                .or_default()
                                .push((path.clone(), line));
                        }
                    }

                    if !by_content.is_empty() {
                        let mut used: HashMap<String, usize> = HashMap::new();
                        for t in pending.drain(..) {
                            let line_text = cur_lines.get(t.current_idx).copied().unwrap_or("");
                            let used_key = line_text.to_owned();
                            let used_count = used.get(&used_key).copied().unwrap_or(0);
                            if let Some((source_path, pb)) = by_content
                                .get(line_text)
                                .and_then(|candidates| candidates.get(used_count))
                            {
                                used.insert(used_key, used_count + 1);
                                result.push(BlameLine {
                                    oid: pb.oid,
                                    final_lineno: t.final_lineno,
                                    orig_lineno: pb.orig_lineno,
                                    content: final_lines[t.final_lineno - 1].clone(),
                                    source_file: pb
                                        .source_file
                                        .clone()
                                        .or_else(|| Some(source_path.clone())),
                                    ignored: pb.ignored || t.ignored,
                                    unblamable: pb.unblamable,
                                    external_contents: false,
                                });
                            } else {
                                result.push(BlameLine {
                                    oid: current_oid,
                                    final_lineno: t.final_lineno,
                                    orig_lineno: t.current_idx + 1,
                                    content: final_lines[t.final_lineno - 1].clone(),
                                    source_file: None,
                                    ignored: t.ignored,
                                    unblamable: false,
                                    external_contents: false,
                                });
                            }
                        }
                        if !deferred.is_empty() {
                            continue 'blame_loop;
                        }
                        break 'blame_loop;
                    }
                }

                // No rename/copy source found — attribute to current commit.
                for t in pending.drain(..) {
                    result.push(BlameLine {
                        oid: current_oid,
                        final_lineno: t.final_lineno,
                        orig_lineno: t.current_idx + 1,
                        content: final_lines[t.final_lineno - 1].clone(),
                        source_file: None,
                        ignored: t.ignored,
                        unblamable: false,
                        external_contents: false,
                    });
                }
                if !deferred.is_empty() {
                    continue 'blame_loop;
                }
                break 'blame_loop;
            }
            None => {
                // Ignored commit but file doesn't exist in parent.
                // Attribute to current anyway (can't go further back).
                for t in pending.drain(..) {
                    result.push(BlameLine {
                        oid: current_oid,
                        final_lineno: t.final_lineno,
                        orig_lineno: t.current_idx + 1,
                        content: final_lines[t.final_lineno - 1].clone(),
                        source_file: None,
                        ignored: t.ignored,
                        unblamable: true,
                        external_contents: false,
                    });
                }
                if !deferred.is_empty() {
                    continue 'blame_loop;
                }
                break 'blame_loop;
            }
            Some((p_blob_oid, p_blob_mode)) if p_blob_oid == current_blob_oid => {
                current_oid = fast_forward_same_blob(
                    odb,
                    session,
                    parent_oid,
                    current_blob_oid,
                    &current_path,
                    grafts,
                    &mut commit_cache,
                )?;
                current_blob_mode = p_blob_mode;
                continue;
            }
            Some((p_blob_oid, p_blob_mode)) => {
                // Diff current vs parent
                let cur_lines_arc =
                    session.load_blob_lines(current_blob_oid, &current_path, current_blob_mode)?;
                let par_lines_arc =
                    session.load_blob_lines(p_blob_oid, &parent_blob_path, p_blob_mode)?;
                let cur_lines = BlameSession::line_refs(&cur_lines_arc);
                let par_lines = BlameSession::line_refs(&par_lines_arc);
                let maps = session.parent_child_line_maps(
                    p_blob_oid,
                    &parent_blob_path,
                    p_blob_mode,
                    current_blob_oid,
                    &current_path,
                    current_blob_mode,
                    &par_lines_arc,
                    &cur_lines_arc,
                    diff_algorithm,
                    indent_heuristic,
                    is_ignored,
                    true,
                );
                let line_map = maps.line_map.as_slice();
                let stop_blame_on_current = maps.stop_blame_on_current.as_slice();
                let mut inserted_copy_source: Option<CopySourceBlameCache> = None;
                if copy_depth >= 3 {
                    if let Some((source_path, source_blame)) = find_copy_source_blame(
                        odb,
                        parent_oid,
                        &parent_commit.tree,
                        &current_path,
                        &cur_lines,
                        ignore_revs,
                        diff_algorithm,
                        textconv_ctx,
                        use_textconv,
                        copy_depth - 1,
                        false,
                        first_parent_only,
                        grafts,
                        indent_heuristic,
                        session,
                    )? {
                        let mut by_content: HashMap<String, Vec<BlameLine>> = HashMap::new();
                        for line in source_blame {
                            by_content
                                .entry(line.content.clone())
                                .or_default()
                                .push(line);
                        }
                        inserted_copy_source = Some((source_path, by_content, HashMap::new()));
                    }
                }

                let mut still_pending = Vec::new();
                for t in pending.drain(..) {
                    if t.current_idx < line_map.len() {
                        if let Some(parent_idx) = line_map[t.current_idx] {
                            if should_drop_tail_match_for_myers(
                                diff_algorithm,
                                parent_idx,
                                t.current_idx,
                                &par_lines,
                            ) {
                                if is_ignored {
                                    still_pending.push(TrackedLine {
                                        final_lineno: t.final_lineno,
                                        current_idx: t.current_idx,
                                        ignored: true,
                                        source_path: t.source_path.clone(),
                                    });
                                } else {
                                    result.push(BlameLine {
                                        oid: current_oid,
                                        final_lineno: t.final_lineno,
                                        orig_lineno: t.current_idx + 1,
                                        content: final_lines[t.final_lineno - 1].clone(),
                                        source_file: None,
                                        ignored: t.ignored,
                                        unblamable: false,
                                        external_contents: false,
                                    });
                                }
                                continue;
                            }
                            // Line came from parent — keep tracking
                            let carried_ignored = if is_ignored {
                                let unchanged = parent_idx == t.current_idx
                                    && par_lines
                                        .get(parent_idx)
                                        .zip(cur_lines.get(t.current_idx))
                                        .is_some_and(|(p, c)| p == c);
                                if unchanged {
                                    t.ignored
                                } else {
                                    true
                                }
                            } else {
                                t.ignored
                            };
                            if stop_blame_on_current
                                .get(t.current_idx)
                                .copied()
                                .unwrap_or(false)
                                && !is_ignored
                            {
                                result.push(BlameLine {
                                    oid: current_oid,
                                    final_lineno: t.final_lineno,
                                    orig_lineno: t.current_idx + 1,
                                    content: final_lines[t.final_lineno - 1].clone(),
                                    source_file: None,
                                    ignored: t.ignored,
                                    unblamable: false,
                                    external_contents: false,
                                });
                                continue;
                            }
                            still_pending.push(TrackedLine {
                                final_lineno: t.final_lineno,
                                current_idx: parent_idx,
                                ignored: carried_ignored,
                                source_path: t.source_path.clone(),
                            });
                        } else if is_ignored {
                            // Best-effort pass-through through ignored revisions:
                            // only keep walking when the same-slot parent line
                            // is text-identical; otherwise keep blame on the
                            // ignored commit and mark as unblamable.
                            let cur_line = cur_lines.get(t.current_idx).copied();
                            if t.current_idx < par_lines.len()
                                && cur_line.is_some_and(|line| line == par_lines[t.current_idx])
                            {
                                still_pending.push(TrackedLine {
                                    final_lineno: t.final_lineno,
                                    current_idx: t.current_idx,
                                    ignored: t.ignored,
                                    source_path: t.source_path.clone(),
                                });
                            } else {
                                result.push(BlameLine {
                                    oid: current_oid,
                                    final_lineno: t.final_lineno,
                                    orig_lineno: t.current_idx + 1,
                                    content: final_lines[t.final_lineno - 1].clone(),
                                    source_file: None,
                                    ignored: t.ignored,
                                    unblamable: true,
                                    external_contents: false,
                                });
                            }
                        } else {
                            if let Some((source_path, by_content, used)) =
                                inserted_copy_source.as_mut()
                            {
                                let line_text = cur_lines.get(t.current_idx).copied().unwrap_or("");
                                let used_key = line_text.to_owned();
                                let used_count = used.get(&used_key).copied().unwrap_or(0);
                                if let Some(pb) = by_content
                                    .get(line_text)
                                    .and_then(|candidates| candidates.get(used_count))
                                {
                                    used.insert(used_key, used_count + 1);
                                    result.push(BlameLine {
                                        oid: pb.oid,
                                        final_lineno: t.final_lineno,
                                        orig_lineno: pb.orig_lineno,
                                        content: final_lines[t.final_lineno - 1].clone(),
                                        source_file: pb
                                            .source_file
                                            .clone()
                                            .or_else(|| Some(source_path.clone())),
                                        ignored: pb.ignored || t.ignored,
                                        unblamable: pb.unblamable,
                                        external_contents: false,
                                    });
                                    continue;
                                }
                            }
                            // Line was introduced in current commit
                            result.push(BlameLine {
                                oid: current_oid,
                                final_lineno: t.final_lineno,
                                orig_lineno: t.current_idx + 1,
                                content: final_lines[t.final_lineno - 1].clone(),
                                source_file: None,
                                ignored: t.ignored,
                                unblamable: false,
                                external_contents: false,
                            });
                        }
                    } else if is_ignored {
                        result.push(BlameLine {
                            oid: current_oid,
                            final_lineno: t.final_lineno,
                            orig_lineno: t.current_idx + 1,
                            content: final_lines[t.final_lineno - 1].clone(),
                            source_file: None,
                            ignored: t.ignored,
                            unblamable: true,
                            external_contents: false,
                        });
                    } else {
                        // Out of range — attribute to current
                        result.push(BlameLine {
                            oid: current_oid,
                            final_lineno: t.final_lineno,
                            orig_lineno: t.current_idx + 1,
                            content: final_lines[t.final_lineno - 1].clone(),
                            source_file: None,
                            ignored: t.ignored,
                            unblamable: false,
                            external_contents: false,
                        });
                    }
                }

                pending = still_pending;
                current_oid = parent_oid;
                current_blob_oid = p_blob_oid;
                current_blob_mode = p_blob_mode;
                if parent_blob_path != current_path {
                    current_path = parent_blob_path;
                    session.set_bloom_path(&current_path);
                }
            }
        }
    }

    result.sort_by_key(|b| b.final_lineno);
    Ok(result)
}

fn mode_from_diff_entry_mode(mode: &str) -> u32 {
    u32::from_str_radix(mode, 8).unwrap_or(0o100644)
}

fn find_rename_entry_for_path(
    odb: &Odb,
    parent_tree: &ObjectId,
    commit_tree: &ObjectId,
    new_path: &str,
    threshold: u32,
) -> Result<Option<DiffEntry>> {
    if threshold >= 100 {
        return Ok(None);
    }
    let entries = diff_trees(odb, Some(parent_tree), Some(commit_tree), "")?;
    let entries = detect_renames(odb, None, entries, threshold);
    Ok(entries
        .into_iter()
        .find(|e| e.status == DiffStatus::Renamed && e.new_path.as_deref() == Some(new_path)))
}

fn find_path_by_oid_in_tree(
    odb: &Odb,
    tree_oid: &ObjectId,
    current_tree_oid: &ObjectId,
    needle_oid: &ObjectId,
    exclude_path: &str,
) -> Result<Option<(String, u32)>> {
    let mut entries = Vec::new();
    collect_tree_file_entries(odb, tree_oid, "", &mut entries)?;
    for (path, oid, mode) in entries {
        if path != exclude_path
            && &oid == needle_oid
            && resolve_path_in_tree_entry(odb, current_tree_oid, &path)?.is_none()
        {
            return Ok(Some((path, mode)));
        }
    }
    Ok(None)
}

#[expect(clippy::too_many_arguments)]
fn find_copy_source_blame(
    odb: &Odb,
    parent_oid: ObjectId,
    parent_tree_oid: &ObjectId,
    exclude_path: &str,
    current_lines: &[&str],
    ignore_revs: &HashSet<ObjectId>,
    diff_algorithm: BlameDiffAlgorithm,
    textconv_ctx: Option<&BlameTextconvContext>,
    use_textconv: bool,
    copy_depth: usize,
    include_current_path: bool,
    first_parent_only: bool,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    indent_heuristic: bool,
    session: &mut BlameSession<'_>,
) -> Result<Option<(String, Vec<BlameLine>)>> {
    let mut entries = Vec::new();
    collect_tree_file_entries(odb, parent_tree_oid, "", &mut entries)?;

    let mut candidates: Vec<(String, ObjectId, u32)> = Vec::new();
    for (path, oid, mode) in entries {
        if (!include_current_path && path == exclude_path) || !is_regular_mode(mode) {
            continue;
        }
        candidates.push((path, oid, mode));
    }

    let est_bytes = current_lines
        .iter()
        .map(|line| line.len())
        .sum::<usize>()
        .saturating_mul(candidates.len());

    let scored: Vec<(String, usize)> = parallel_map_indices(
        odb,
        candidates.len(),
        session.copy_score_parallel,
        est_bytes,
        |ci| -> std::result::Result<(String, usize), LibError> {
            let (path, oid, mode) = &candidates[ci];
            let content =
                read_blob_content_for_blame(odb, oid, path, *mode, textconv_ctx, use_textconv)?;
            let parsed: Vec<&str> = content_lines(&content);
            let score = current_lines
                .iter()
                .filter(|line| parsed.iter().any(|src| src == *line))
                .count();
            Ok((path.clone(), score))
        },
    )?;

    let mut best_path: Option<String> = None;
    let mut best_score = 0usize;
    for (path, score) in scored {
        if score > best_score {
            best_score = score;
            best_path = Some(path);
        }
    }

    let Some(source_path) = best_path else {
        return Ok(None);
    };
    if best_score == 0 {
        return Ok(None);
    }

    let source_blame = compute_blame_with_session(
        odb,
        parent_oid,
        &source_path,
        ignore_revs,
        diff_algorithm,
        textconv_ctx,
        use_textconv,
        copy_depth,
        first_parent_only,
        grafts,
        indent_heuristic,
        None,
        session,
    )?;
    Ok(Some((source_path, source_blame)))
}

fn collect_tree_file_entries(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    out: &mut Vec<(String, ObjectId, u32)>,
) -> Result<()> {
    let obj = read_object_for_blame(odb, tree_oid, None)?;
    if obj.kind != ObjectKind::Tree {
        return Err(LibError::Message("expected tree".to_string()));
    }
    let entries = parse_tree(&obj.data)?;
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == 0o040000 {
            collect_tree_file_entries(odb, &entry.oid, &path, out)?;
        } else {
            out.push((path, entry.oid, entry.mode));
        }
    }
    Ok(())
}

fn get_blame_commit(
    odb: &Odb,
    oid: ObjectId,
    cache: &mut HashMap<ObjectId, Arc<BlameCommitMeta>>,
) -> Result<Arc<BlameCommitMeta>> {
    if let Some(c) = cache.get(&oid) {
        return Ok(Arc::clone(c));
    }
    let obj = read_object_for_blame(odb, &oid, None)?;
    let (tree, parents) = parse_commit_tree_and_parents(&obj.data)?;
    let meta = Arc::new(BlameCommitMeta { tree, parents });
    cache.insert(oid, Arc::clone(&meta));
    Ok(meta)
}

/// Commit-graph Bloom filters describe path changes vs the commit object's first parent.
/// Skip the shortcut when grafts replace parents or the walk target is not that parent.
fn bloom_may_skip_to_first_parent(
    session: &mut BlameSession<'_>,
    odb: &Odb,
    current_oid: ObjectId,
    parent_oid: ObjectId,
    path: &str,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    commit_cache: &mut HashMap<ObjectId, Arc<BlameCommitMeta>>,
) -> Result<bool> {
    if grafts.contains_key(&current_oid) {
        return Ok(false);
    }
    let object_parents = commit_object_parents(session, odb, current_oid, commit_cache)?;
    if object_parents.first() != Some(&parent_oid) {
        return Ok(false);
    }
    let Some(chain) = session.commit_graph.as_ref() else {
        return Ok(false);
    };
    if session.bloom_pathspec.is_empty() {
        session.bloom_pathspec.push(path.to_owned());
    }
    Ok(matches!(
        chain.bloom_precheck_for_paths(
            odb,
            current_oid,
            session.bloom_pathspec.as_slice(),
            None,
            -1,
            true,
        ),
        Ok(BloomPrecheck::DefinitelyNot)
    ))
}

/// Parent OIDs stored in the commit object (or commit-graph), ignoring grafts.
fn commit_object_parents(
    session: &BlameSession<'_>,
    odb: &Odb,
    oid: ObjectId,
    cache: &mut HashMap<ObjectId, Arc<BlameCommitMeta>>,
) -> Result<Vec<ObjectId>> {
    if let Some(chain) = session.commit_graph.as_ref() {
        if let Some((parents, _commit_time)) = chain.graph_commit(&oid) {
            return Ok(parents);
        }
    }
    Ok(get_blame_commit(odb, oid, cache)?.parents.clone())
}

/// Parent list for a commit, honoring `.git/info/grafts` (same rules as `git rev-list`).
fn commit_parents_for_blame(
    odb: &Odb,
    oid: ObjectId,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    cache: &mut HashMap<ObjectId, Arc<BlameCommitMeta>>,
) -> Result<Vec<ObjectId>> {
    if let Some(p) = grafts.get(&oid) {
        return Ok(p.clone());
    }
    Ok(get_blame_commit(odb, oid, cache)?.parents.clone())
}

/// Options for [`blame_file`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlameOptions {
    /// Commit to blame at. When `None`, uses the current `HEAD` commit.
    pub start: Option<ObjectId>,
    /// When set, only include lines whose 1-based final line number lies in this range.
    pub line_range: Option<RangeInclusive<usize>>,
    /// When true, follow only the first parent at merge commits (Git's `--first-parent`).
    pub first_parent_only: bool,
}

/// One blamed line returned by [`blame_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameAttributedLine {
    /// Commit that last modified this line.
    pub commit: ObjectId,
    /// 1-based line number in the commit where this line originated.
    pub original_line: usize,
    /// 1-based line number in the blamed revision's file.
    pub final_line: usize,
    /// Line text in the blamed revision.
    pub content: String,
}

/// Metadata for a commit referenced by [`BlameResult::lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommitInfo {
    /// Author identity line (`Name <email> <epoch> <tz>`).
    pub author: String,
    /// Author timestamp (Unix seconds).
    pub author_time: i64,
    /// First line of the commit message.
    pub summary: String,
}

/// Full blame result for one file at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameResult {
    /// Blamed lines in final-line order (after any line-range filter).
    pub lines: Vec<BlameAttributedLine>,
    /// Commit metadata keyed by object id for every line in `lines`.
    pub commits: HashMap<ObjectId, BlameCommitInfo>,
}

/// Blame `path` at `options.start` (or `HEAD`) and return per-line attributions.
///
/// `path` is repository-relative when the repository has a work tree.
///
/// # Errors
///
/// Returns an error when `HEAD` is unborn, the revision or path cannot be resolved,
/// or object data cannot be read.
pub fn blame_file(repo: &Repository, path: &str, options: &BlameOptions) -> Result<BlameResult> {
    let start_oid = resolve_blame_start(repo, options.start)?;
    let file_path = normalize_blame_path(repo, path);
    let config = repo.config().map(|cfg| (*cfg).clone()).unwrap_or_default();
    let grafts = crate::rev_parse::load_graft_parents(repo.git_dir());
    let diff_algorithm = config
        .get("diff.algorithm")
        .as_ref()
        .and_then(|name| parse_diff_algorithm_name(name))
        .unwrap_or(BlameDiffAlgorithm::Myers);
    let indent_heuristic = config
        .get_bool("diff.indentHeuristic")
        .and_then(|v| v.ok())
        .unwrap_or(false);
    let ignore_revs = HashSet::new();

    let thread_policy = repo.thread_policy();
    let mut session = BlameSession::new(repo.odb(), None, false, &thread_policy);
    session.commit_graph = repo.caches().commit_graph_chain(repo.odb().objects_dir());
    let raw = compute_blame_with_session(
        repo.odb(),
        start_oid,
        &file_path,
        &ignore_revs,
        diff_algorithm,
        None,
        false,
        0,
        options.first_parent_only,
        &grafts,
        indent_heuristic,
        options.line_range.clone(),
        &mut session,
    )?;

    let lines: Vec<BlameAttributedLine> = raw
        .into_iter()
        .map(|line| BlameAttributedLine {
            commit: line.oid,
            original_line: line.orig_lineno,
            final_line: line.final_lineno,
            content: line.content,
        })
        .collect();

    let mut commits = HashMap::new();
    for line in &lines {
        commits
            .entry(line.commit)
            .or_insert_with(|| load_blame_commit_info(repo.odb(), line.commit));
    }

    Ok(BlameResult { lines, commits })
}

fn resolve_blame_start(repo: &Repository, start: Option<ObjectId>) -> Result<ObjectId> {
    if let Some(oid) = start {
        return Ok(oid);
    }
    use crate::state::{resolve_head, HeadState};
    match resolve_head(repo.git_dir())? {
        HeadState::Detached { oid } => Ok(oid),
        HeadState::Branch { oid: Some(oid), .. } => Ok(oid),
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => Err(LibError::Message(
            "cannot blame: HEAD does not point to a commit".to_string(),
        )),
    }
}

fn normalize_blame_path(repo: &Repository, path: &str) -> String {
    if let Some(work_tree) = repo.work_tree() {
        crate::pathspec::normalize_worktree_file_path_with_git_dir(
            path,
            work_tree,
            None,
            Some(repo.git_dir()),
        )
    } else {
        path.to_string()
    }
}

fn load_blame_commit_info(odb: &Odb, oid: ObjectId) -> BlameCommitInfo {
    let fallback = BlameCommitInfo {
        author: String::new(),
        author_time: 0,
        summary: String::new(),
    };
    let Ok(obj) = read_object_for_blame(odb, &oid, None) else {
        return fallback;
    };
    let Ok(commit) = parse_commit(&obj.data) else {
        return fallback;
    };
    BlameCommitInfo {
        author: commit.author.trim().to_string(),
        author_time: author_epoch_from_identity(&commit.author),
        summary: commit
            .message
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string(),
    }
}

fn author_epoch_from_identity(identity: &str) -> i64 {
    let parts: Vec<&str> = identity.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse().unwrap_or(0)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_line_map_maps_unchanged_lines_and_drops_replaced() {
        let old = ["alpha", "beta", "gamma"];
        let new = ["alpha", "BETA", "gamma"];
        let map = build_line_map(&old, &new, BlameDiffAlgorithm::Myers, false);
        // unchanged lines map back to their old index; the replaced line is None.
        assert_eq!(map, vec![Some(0), None, Some(2)]);
    }

    #[test]
    fn build_line_map_identity_for_equal_files() {
        let lines = ["one", "two", "three"];
        let map = build_line_map(&lines, &lines, BlameDiffAlgorithm::Histogram, false);
        assert_eq!(map, vec![Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn myers_does_not_map_case_changed_line_to_parent() {
        let par = ["alpha one", "beta two", " gamma three"];
        let cur = ["alpha ONE", "beta two", " gamma three"];
        let map = build_line_map(&par, &cur, BlameDiffAlgorithm::Myers, false);
        assert_eq!(map[0], None);
    }

    #[test]
    fn reposition_blame_flags_rotated_head_line_only() {
        let par = ["a", "b", "c", "d"];
        let cur = ["d", "a", "b", "c"];
        let diff = build_line_map(&par, &cur, BlameDiffAlgorithm::Myers, false);
        let map = build_exact_relocate_line_map(&par, &cur, &diff);
        let xdiff = crate::diff::xdiff_new_line_changed_flags(&par, &cur, false);
        let blame = blame_stops_on_current_commit(&diff, &map, &xdiff);
        assert_eq!(blame, vec![true, false, false, false]);
    }

    #[test]
    fn reposition_blame_swapped_inserted_line_in_three_line_file() {
        let par = ["alpha ONE", "inserted", " gamma three"];
        let cur = ["alpha ONE", " gamma three", "inserted"];
        let diff = build_line_map(&par, &cur, BlameDiffAlgorithm::Myers, false);
        let map = build_exact_relocate_line_map(&par, &cur, &diff);
        let xdiff = crate::diff::xdiff_new_line_changed_flags(&par, &cur, false);
        let blame = blame_stops_on_current_commit(&diff, &map, &xdiff);
        assert_eq!(blame, vec![false, false, true]);
    }

    #[test]
    fn reposition_blame_dacb_permutation() {
        let par = ["a", "b", "c", "d"];
        let cur = ["d", "a", "c", "b"];
        let diff = build_line_map(&par, &cur, BlameDiffAlgorithm::Myers, false);
        let map = build_exact_relocate_line_map(&par, &cur, &diff);
        let xdiff = crate::diff::xdiff_new_line_changed_flags(&par, &cur, false);
        let blame = blame_stops_on_current_commit(&diff, &map, &xdiff);
        assert_eq!(blame, vec![true, false, false, true]);
    }

    #[test]
    fn reposition_blame_flags_same_index_diff_insertion() {
        let par = ["a", "b", "c", "d"];
        let cur = ["a", "d", "c", "b"];
        let diff = build_line_map(&par, &cur, BlameDiffAlgorithm::Myers, false);
        let map = build_exact_relocate_line_map(&par, &cur, &diff);
        let xdiff = crate::diff::xdiff_new_line_changed_flags(&par, &cur, false);
        let blame = blame_stops_on_current_commit(&diff, &map, &xdiff);
        assert_eq!(blame, vec![false, false, true, true]);
    }

    #[test]
    fn fuzzy_line_map_recovers_moved_unchanged_line() {
        let par = ["alpha ONE", "inserted", " gamma three"];
        let cur = ["alpha ONE", " gamma three", "inserted"];
        let map = build_exact_relocate_line_map(
            &par,
            &cur,
            &build_line_map(&par, &cur, BlameDiffAlgorithm::Myers, false),
        );
        assert_eq!(map[1], Some(2));
    }

    #[test]
    fn parse_diff_algorithm_name_known_and_unknown() {
        assert_eq!(
            parse_diff_algorithm_name("myers"),
            Some(BlameDiffAlgorithm::Myers)
        );
        assert_eq!(
            parse_diff_algorithm_name("histogram"),
            Some(BlameDiffAlgorithm::Histogram)
        );
        assert_eq!(parse_diff_algorithm_name("nope"), None);
    }

    #[test]
    fn blame_file_walks_linear_history() -> Result<()> {
        use crate::objects::ObjectKind;
        use crate::repo::init_repository;
        use crate::RefStorageFormat;
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().map_err(crate::error::Error::Io)?;
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)?;
        let path = "trace.txt";
        let mut parent: Option<ObjectId> = None;
        let mut tip = repo.odb().write(ObjectKind::Blob, b"line0\n")?;
        for i in 1..=48_i32 {
            let content = format!("line{i}\n");
            fs::write(dir.path().join(path), &content).map_err(crate::error::Error::Io)?;
            tip = repo.odb().write(ObjectKind::Blob, content.as_bytes())?;
            let mut body = format!("100644 {path}\0").into_bytes();
            body.extend_from_slice(tip.as_bytes());
            let tree = repo.odb().write(ObjectKind::Tree, &body)?;
            let mut commit_body = format!("tree {tree}\n");
            if let Some(p) = parent {
                commit_body.push_str(&format!("parent {p}\n"));
            }
            commit_body.push_str(&format!(
                "author T <t@e.com> {} +0000\ncommitter T <t@e.com> {} +0000\n\nc{i}\n",
                100 + i,
                100 + i
            ));
            tip = repo
                .odb()
                .write(ObjectKind::Commit, commit_body.as_bytes())?;
            parent = Some(tip);
        }
        fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))
            .map_err(crate::error::Error::Io)?;
        for name in ["myers", "histogram", "minimal", "patience"] {
            let _ = parse_diff_algorithm_name(name);
        }
        let result = blame_file(
            &repo,
            path,
            &BlameOptions {
                start: Some(tip),
                line_range: Some(1..=10),
                ..Default::default()
            },
        )?;
        assert!(!result.lines.is_empty());
        Ok(())
    }
}
