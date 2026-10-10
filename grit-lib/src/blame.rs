//! Blame line-mapping algorithm.
//!
//! Pure line-correspondence machinery used by `git blame` / `annotate`: given an
//! old and a new version of a file (as line slices), produce a per-new-line map
//! back to the originating old line. Both an exact map (via the configured diff
//! algorithm, honoring the indent heuristic) and a fuzzy fallback (for lines a
//! parent rewrote) are provided. No I/O, no repository access — strings in,
//! mappings out. [`compute_blame`] and [`blame_file`] drive the per-commit walk and
//! call these helpers.

use std::ops::RangeInclusive;

use similar::Algorithm as SimilarAlgorithm;

/// Diff algorithm used when mapping lines between blob revisions during blame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlameDiffAlgorithm {
    Myers,
    Histogram,
    Patience,
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

/// Map each line in `new` to its origin line index in `old` (if any).
pub fn build_line_map(
    old: &[&str],
    new: &[&str],
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
) -> Vec<Option<usize>> {
    // Ensure trailing newlines so `from_lines` splits consistently
    let mut old_joined = old.join("\n");
    old_joined.push('\n');
    let mut new_joined = new.join("\n");
    new_joined.push('\n');

    // `--indent-heuristic` / `diff.indentHeuristic` shifts ambiguous add/delete
    // runs like Git's xdiff, which changes which lines blame attributes (t4061).
    let ops = crate::diff_indent_heuristic::diff_lines_ops_compacted(
        &old_joined,
        &new_joined,
        diff_algorithm.to_similar(),
        indent_heuristic,
    );

    let mut result = vec![None; new.len()];
    for op in ops {
        if let similar::DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for k in 0..len {
                if new_index + k < result.len() {
                    result[new_index + k] = Some(old_index + k);
                }
            }
        }
    }

    result
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

    for new_idx in 0..fuzzy_map.len() {
        if fuzzy_map[new_idx].is_some() {
            continue;
        }
        let mut best: Option<(usize, usize, usize)> = None;
        for old_idx in 0..old.len() {
            if old[old_idx] != new[new_idx] {
                continue;
            }
            let candidate = (used_old[old_idx], old_idx.abs_diff(new_idx), old_idx);
            if best.is_none_or(|b| candidate < b) {
                best = Some(candidate);
            }
        }
        if let Some((_, _, old_idx)) = best {
            fuzzy_map[new_idx] = Some(old_idx);
            used_old[old_idx] += 1;
        }
    }

    fuzzy_map
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
    for new_idx in 0..fuzzy_map.len() {
        if fuzzy_map[new_idx].is_some() {
            continue;
        }
        let mut best: Option<(usize, usize, usize)> = None;
        for old_idx in 0..old.len() {
            if old[old_idx] != new[new_idx] {
                continue;
            }
            let candidate = (used_old[old_idx], old_idx.abs_diff(new_idx), old_idx);
            if best.is_none_or(|b| candidate < b) {
                best = Some(candidate);
            }
        }
        if let Some((_, _, old_idx)) = best {
            fuzzy_map[new_idx] = Some(old_idx);
            used_old[old_idx] += 1;
        }
    }

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

// ---------------------------------------------------------------------------
// Per-commit blame attribution engine.
//
// Walks history from a starting commit, diffs successive blob versions (via the
// line-mapping helpers above), and attributes each final-image line to the
// commit that last touched it. Honors `.git/info/grafts`, ignored revisions
// copy/rename detection when configured, textconv filters, and graft parents.
// [`blame_file`] is the repository-level entry point; the `grit blame` command
// renders its result.
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use crate::config::ConfigSet;
use crate::crlf::{
    convert_to_worktree_eager, get_file_attrs, load_gitattributes, load_gitattributes_from_index,
    ConversionConfig, GitAttributes,
};
use crate::error::{Error as LibError, Result};
use crate::objects::{parse_commit, parse_tree, CommitData, Object, ObjectId, ObjectKind};
use crate::odb::Odb;
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
/// supplies a function that performs the fetch. Install with
/// [`Repository::set_promisor_hydrate_hook`].
pub type PromisorHydrateHook = fn(&Repository, ObjectId);

/// A single line attribution produced by [`compute_blame`].
#[derive(Debug, Clone)]
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
    if let Some(work_tree) = repo.work_tree.as_deref() {
        let rules = load_gitattributes(work_tree);
        if !rules.is_empty() {
            return rules;
        }
    }

    if let Ok(index) = repo.load_index() {
        return load_gitattributes_from_index(&index, &repo.odb);
    }

    Vec::new()
}

fn load_diff_attr_rules(repo: &Repository) -> Vec<DiffAttrRule> {
    let mut rules = Vec::new();

    if let Some(work_tree) = repo.work_tree.as_deref() {
        parse_diff_attr_file(&work_tree.join(".gitattributes"), &mut rules);
        parse_diff_attr_file(&work_tree.join(".git/info/attributes"), &mut rules);
    }

    if rules.is_empty() {
        if let Ok(index) = repo.load_index() {
            if let Some(entry) = index.get(b".gitattributes", 0) {
                if let Ok(obj) = repo.odb.read(&entry.oid) {
                    if let Ok(content) = String::from_utf8(obj.data) {
                        parse_diff_attr_content(&content, &mut rules);
                    }
                }
            }
        }
        parse_diff_attr_file(&repo.git_dir.join("info/attributes"), &mut rules);
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

/// Walk history and attribute each line in a blob at `start_oid` to a commit.
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
) -> Result<Vec<BlameLine>> {
    let start_commit = {
        let obj = read_object_for_blame(odb, &start_oid, None)?;
        parse_commit(&obj.data)?
    };

    let (blob_oid, blob_mode) = resolve_path_in_tree_entry(odb, &start_commit.tree, file_path)?
        .ok_or_else(|| LibError::Message(format!("file '{file_path}' not found in revision")))?;
    let content = read_blob_content_for_blame(
        odb,
        &blob_oid,
        file_path,
        blob_mode,
        textconv_ctx,
        use_textconv,
    )?;
    let lines = content_lines(&content);
    let num_lines = lines.len();

    if num_lines == 0 {
        return Ok(Vec::new());
    }

    // Lines still needing attribution
    let mut pending: Vec<TrackedLine> = (0..num_lines)
        .map(|i| TrackedLine {
            final_lineno: i + 1,
            current_idx: i,
            ignored: false,
            source_path: None,
        })
        .collect();

    let mut result: Vec<BlameLine> = Vec::with_capacity(num_lines);
    // Store final content for output
    let final_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();

    let mut current_oid = start_oid;
    let mut current_blob_oid = blob_oid;
    let mut current_blob_mode = blob_mode;
    let mut current_path = file_path.to_string();
    let mut commit_cache: HashMap<ObjectId, CommitData> = HashMap::new();
    commit_cache.insert(start_oid, start_commit);
    let mut deferred: VecDeque<(ObjectId, ObjectId, u32, String, Vec<TrackedLine>)> =
        VecDeque::new();

    'blame_loop: loop {
        if pending.is_empty() {
            if let Some((oid, blob, mode, path, lines)) = deferred.pop_front() {
                current_oid = oid;
                current_blob_oid = blob;
                current_blob_mode = mode;
                current_path = path;
                pending = lines;
                continue;
            }
            break;
        }

        let commit = get_commit(odb, current_oid, &mut commit_cache)?;
        let parents = commit_parents_for_blame(odb, current_oid, grafts, &mut commit_cache)?;

        let is_ignored = ignore_revs.contains(&current_oid);

        // If an ignored merge commit is encountered, try to continue blame
        // through the parent that actually contributed each line.
        if is_ignored && parents.len() > 1 {
            let cur_content = read_blob_content_for_blame(
                odb,
                &current_blob_oid,
                &current_path,
                current_blob_mode,
                textconv_ctx,
                use_textconv,
            )?;
            let cur_lines = content_lines(&cur_content);

            let mut parent_lines: Vec<Option<Vec<String>>> = Vec::new();
            let mut parent_blames: Vec<Option<Vec<BlameLine>>> = Vec::new();
            for parent_oid in &parents {
                let parent_commit = get_commit(odb, *parent_oid, &mut commit_cache)?;
                if let Some((p_blob_oid, p_blob_mode)) =
                    resolve_path_in_tree_entry(odb, &parent_commit.tree, &current_path)?
                {
                    let p_content = read_blob_content_for_blame(
                        odb,
                        &p_blob_oid,
                        &current_path,
                        p_blob_mode,
                        textconv_ctx,
                        use_textconv,
                    )?;
                    let p_lines = content_lines(&p_content)
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>();
                    let p_blame = compute_blame(
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
        let mut all_parents_have_path = parents.len() >= 2;
        if all_parents_have_path {
            for p in &parents {
                let pc = get_commit(odb, *p, &mut commit_cache)?;
                if resolve_path_in_tree_entry(odb, &pc.tree, &current_path)?.is_none() {
                    all_parents_have_path = false;
                    break;
                }
            }
        }

        if !first_parent_only && !is_ignored && all_parents_have_path {
            let cur_content = read_blob_content_for_blame(
                odb,
                &current_blob_oid,
                &current_path,
                current_blob_mode,
                textconv_ctx,
                use_textconv,
            )?;
            let cur_lines = content_lines(&cur_content);

            let mut parent_blobs: Vec<(ObjectId, ObjectId, u32)> =
                Vec::with_capacity(parents.len());
            let mut par_lines_vec: Vec<Vec<String>> = Vec::with_capacity(parents.len());
            let mut maps: Vec<Vec<Option<usize>>> = Vec::with_capacity(parents.len());

            for &p in &parents {
                let pc = get_commit(odb, p, &mut commit_cache)?;
                let Some((blob, mode)) = resolve_path_in_tree_entry(odb, &pc.tree, &current_path)?
                else {
                    return Err(LibError::Message(
                        "internal: missing blob in merge parent".to_string(),
                    ));
                };
                let par_content = read_blob_content_for_blame(
                    odb,
                    &blob,
                    &current_path,
                    mode,
                    textconv_ctx,
                    use_textconv,
                )?;
                let pl: Vec<String> = content_lines(&par_content)
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect();
                let pl_refs: Vec<&str> = pl.iter().map(|s| s.as_str()).collect();
                let map_algo = if parents.len() > 2 {
                    BlameDiffAlgorithm::Patience
                } else {
                    diff_algorithm
                };
                let map = build_line_map(&pl_refs, &cur_lines, map_algo, indent_heuristic);
                parent_blobs.push((p, blob, mode));
                par_lines_vec.push(pl);
                maps.push(map);
            }

            let mut buckets: Vec<Vec<TrackedLine>> = vec![Vec::new(); parents.len()];
            let mut attributed: Vec<BlameLine> = Vec::new();

            let mut used_in_parent: Vec<HashSet<usize>> = vec![HashSet::new(); parents.len()];

            let mut remaining = std::mem::take(&mut pending);
            for i in 0..parents.len() {
                if remaining.is_empty() {
                    break;
                }
                let mut next_remaining: Vec<TrackedLine> = Vec::new();
                for t in remaining {
                    let idx = t.current_idx;
                    let cur_line = cur_lines.get(idx).copied();
                    let m = maps[i].get(idx).copied().flatten();
                    if let Some(p_idx) = m {
                        let text_ok = par_lines_vec[i].get(p_idx).map(|s| s.as_str()) == cur_line;
                        if text_ok && used_in_parent[i].insert(p_idx) {
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
                if !buckets[i].is_empty() {
                    let (p, blob, mode) = parent_blobs[i];
                    deferred.push_back((
                        p,
                        blob,
                        mode,
                        current_path.clone(),
                        std::mem::take(&mut buckets[i]),
                    ));
                }
            }

            if !buckets[0].is_empty() {
                let (p, blob, mode) = parent_blobs[0];
                current_oid = p;
                current_blob_oid = blob;
                current_blob_mode = mode;
                pending = std::mem::take(&mut buckets[0]);
            } else if deferred.is_empty() {
                break 'blame_loop;
            }
            continue;
        }

        let parent_oid = parents[0];
        let parent_commit = get_commit(odb, parent_oid, &mut commit_cache)?;
        let parent_blob_entry =
            resolve_path_in_tree_entry(odb, &parent_commit.tree, &current_path)?;
        let can_follow_rename = true;

        match parent_blob_entry {
            None if !is_ignored => {
                // File doesn't exist at this path in parent.
                // First, try to follow a pure rename by matching blob OID.
                if can_follow_rename {
                    if let Some((renamed_path, renamed_mode)) = find_path_by_oid_in_tree(
                        odb,
                        &parent_commit.tree,
                        &commit.tree,
                        &current_blob_oid,
                        &current_path,
                    )? {
                        current_path = renamed_path;
                        current_oid = parent_oid;
                        current_blob_mode = renamed_mode;
                        continue;
                    }
                }

                // If copy detection is enabled, try to track lines to source
                // files in the parent tree.
                if copy_depth >= 1 {
                    let cur_content = read_blob_content_for_blame(
                        odb,
                        &current_blob_oid,
                        &current_path,
                        current_blob_mode,
                        textconv_ctx,
                        use_textconv,
                    )?;
                    let cur_lines = content_lines(&cur_content);
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
                            && resolve_path_in_tree_entry(odb, &commit.tree, &path)?.is_some()
                        {
                            continue;
                        }

                        let source_content = read_blob_content_for_blame(
                            odb,
                            &oid,
                            &path,
                            mode,
                            textconv_ctx,
                            use_textconv,
                        )?;
                        let source_lines = content_lines(&source_content);
                        let overlap = cur_lines
                            .iter()
                            .filter(|line| source_lines.iter().any(|src| src == *line))
                            .count();
                        if overlap == 0 {
                            continue;
                        }

                        let source_blame = compute_blame(
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
                // Identical blob — skip to parent
                current_oid = parent_oid;
                current_blob_mode = p_blob_mode;
                continue;
            }
            Some((p_blob_oid, p_blob_mode)) => {
                // Diff current vs parent
                let cur_content = read_blob_content_for_blame(
                    odb,
                    &current_blob_oid,
                    &current_path,
                    current_blob_mode,
                    textconv_ctx,
                    use_textconv,
                )?;
                let par_content = read_blob_content_for_blame(
                    odb,
                    &p_blob_oid,
                    &current_path,
                    p_blob_mode,
                    textconv_ctx,
                    use_textconv,
                )?;
                let cur_lines = content_lines(&cur_content);
                let par_lines = content_lines(&par_content);

                // Build mapping: cur_line_idx → Option<parent_line_idx>
                let mut line_map =
                    build_line_map(&par_lines, &cur_lines, diff_algorithm, indent_heuristic);
                line_map = build_exact_relocate_line_map(&par_lines, &cur_lines, &line_map);
                if is_ignored {
                    line_map = build_fuzzy_line_map(&par_lines, &cur_lines, &line_map);
                }
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
                            if parent_idx < t.current_idx
                                && !is_ignored
                                && par_lines.len() == cur_lines.len()
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
            }
        }
    }

    result.sort_by_key(|b| b.final_lineno);
    Ok(result)
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
) -> Result<Option<(String, Vec<BlameLine>)>> {
    let mut entries = Vec::new();
    collect_tree_file_entries(odb, parent_tree_oid, "", &mut entries)?;

    let mut best_path: Option<String> = None;
    let mut best_score = 0usize;
    for (path, oid, mode) in &entries {
        if (!include_current_path && path == exclude_path) || !is_regular_mode(*mode) {
            continue;
        }
        let content =
            read_blob_content_for_blame(odb, oid, path, *mode, textconv_ctx, use_textconv)?;
        let lines = content_lines(&content);
        let score = current_lines
            .iter()
            .filter(|line| lines.iter().any(|src| src == *line))
            .count();
        if score > best_score {
            best_score = score;
            best_path = Some(path.clone());
        }
    }

    let Some(source_path) = best_path else {
        return Ok(None);
    };
    if best_score == 0 {
        return Ok(None);
    }

    let source_blame = compute_blame(
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

fn get_commit(
    odb: &Odb,
    oid: ObjectId,
    cache: &mut HashMap<ObjectId, CommitData>,
) -> Result<CommitData> {
    if let Some(c) = cache.get(&oid) {
        return Ok(c.clone());
    }
    let obj = read_object_for_blame(odb, &oid, None)?;
    let c = parse_commit(&obj.data)?;
    cache.insert(oid, c.clone());
    Ok(c)
}

/// Parent list for a commit, honoring `.git/info/grafts` (same rules as `git rev-list`).
fn commit_parents_for_blame(
    odb: &Odb,
    oid: ObjectId,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    cache: &mut HashMap<ObjectId, CommitData>,
) -> Result<Vec<ObjectId>> {
    if let Some(p) = grafts.get(&oid) {
        return Ok(p.clone());
    }
    Ok(get_commit(odb, oid, cache)?.parents)
}

/// Options for [`blame_file`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlameOptions {
    /// Commit to blame at. When `None`, uses the current `HEAD` commit.
    pub start: Option<ObjectId>,
    /// When set, only include lines whose 1-based final line number lies in this range.
    pub line_range: Option<RangeInclusive<usize>>,
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
    let grafts = crate::rev_parse::load_graft_parents(&repo.git_dir);
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

    let raw = compute_blame(
        &repo.odb,
        start_oid,
        &file_path,
        &ignore_revs,
        diff_algorithm,
        None,
        false,
        0,
        false,
        &grafts,
        indent_heuristic,
    )?;

    let mut lines: Vec<BlameAttributedLine> = raw
        .into_iter()
        .map(|line| BlameAttributedLine {
            commit: line.oid,
            original_line: line.orig_lineno,
            final_line: line.final_lineno,
            content: line.content,
        })
        .collect();

    if let Some(range) = &options.line_range {
        lines.retain(|line| range.contains(&line.final_line));
    }

    let mut commits = HashMap::new();
    for line in &lines {
        commits
            .entry(line.commit)
            .or_insert_with(|| load_blame_commit_info(&repo.odb, line.commit));
    }

    Ok(BlameResult { lines, commits })
}

fn resolve_blame_start(repo: &Repository, start: Option<ObjectId>) -> Result<ObjectId> {
    if let Some(oid) = start {
        return Ok(oid);
    }
    use crate::state::{resolve_head, HeadState};
    match resolve_head(&repo.git_dir)? {
        HeadState::Detached { oid } => Ok(oid),
        HeadState::Branch { oid: Some(oid), .. } => Ok(oid),
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => Err(LibError::Message(
            "cannot blame: HEAD does not point to a commit".to_string(),
        )),
    }
}

fn normalize_blame_path(repo: &Repository, path: &str) -> String {
    if let Some(work_tree) = repo.work_tree.as_deref() {
        crate::pathspec::normalize_worktree_file_path(path, work_tree, None)
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
}
