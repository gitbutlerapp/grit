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
use std::sync::OnceLock;

use similar::{Algorithm as SimilarAlgorithm, DiffOp};

#[path = "blame_ignore_revs.rs"]
mod blame_ignore_revs;
pub use crate::line_range::{resolve_line_range, LineRangeError};
pub use blame_ignore_revs::{IgnoreRevs, IgnoreRevsSources};

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

/// Parent line index for a child line known to lie in an unchanged affix region.
fn affix_parent_index(
    child_idx: usize,
    prefix: usize,
    suffix: usize,
    old_len: usize,
    new_len: usize,
) -> usize {
    let suffix_start = new_len.saturating_sub(suffix);
    if child_idx < prefix {
        child_idx
    } else {
        debug_assert!(suffix > 0 && child_idx >= suffix_start);
        let off = child_idx - suffix_start;
        old_len.saturating_sub(suffix) + off
    }
}

/// Split pending lines into those mappable through unchanged affix vs those in the edited middle.
///
/// Only the part of the affix that change compaction cannot reach is mapped directly; see
/// [`slide_safe_affix`].
fn partition_pending_by_affix(
    affix: Affix,
    old_len: usize,
    new_len: usize,
    pending: Vec<TrackedLine>,
) -> (Vec<TrackedLine>, Vec<TrackedLine>) {
    let (prefix, suffix) = (affix.safe_prefix, affix.safe_suffix);
    let suffix_start = new_len.saturating_sub(suffix);
    let mut affix_mapped = Vec::new();
    let mut need_diff = Vec::new();
    for t in pending {
        let i = t.current_idx;
        if i < prefix || (suffix > 0 && i >= suffix_start) {
            affix_mapped.push(TrackedLine {
                final_lineno: t.final_lineno,
                current_idx: affix_parent_index(i, prefix, suffix, old_len, new_len),
                ignored: t.ignored,
                source_path: t.source_path,
                origin: t.origin,
            });
        } else {
            need_diff.push(t);
        }
    }
    (affix_mapped, need_diff)
}

/// Common prefix and suffix of two files, with the lengths of them no diff group can slide into.
///
/// The line diff only marks lines between the common prefix and suffix, but change compaction
/// then slides each group while the line leaving it equals the line entering it, so every line
/// a group ever covers has the text of a line in the edited middle. Affix lines next to the
/// middle are therefore only safe once a line whose text appears nowhere in either middle
/// stops the slide.
fn slide_safe_affix<L: BlameLines + ?Sized>(
    old: &L,
    new: &L,
    ws: crate::diff::WhitespaceMode,
) -> Affix {
    let (prefix, suffix) = common_affix_len(old, new, ws);
    let new_hi = new.len() - suffix;
    let old_hi = old.len() - suffix;
    let mut affix = Affix {
        prefix,
        suffix,
        safe_prefix: prefix,
        safe_suffix: suffix,
    };
    if prefix == new_hi && prefix == old_hi {
        return affix;
    }
    let mut middle = MiddleLines {
        old,
        new,
        old_range: prefix..old_hi,
        new_range: prefix..new_hi,
        ws,
        // A scan under whitespace rules normalizes every middle line again.
        scans_left: if ws.normalizes_line_compare() { 0 } else { 8 },
        set: None,
    };
    let prefix_reach = (0..prefix)
        .rev()
        .take_while(|&i| middle.contains(new.comparison_line(i, ws)))
        .count();
    let suffix_reach = (new_hi..new.len())
        .take_while(|&i| middle.contains(new.comparison_line(i, ws)))
        .count();
    affix.safe_prefix -= prefix_reach;
    affix.safe_suffix -= suffix_reach;
    affix
}

/// Common prefix and suffix line counts of two files, and the parts of them that change
/// compaction cannot reach (see [`slide_safe_affix`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Affix {
    prefix: usize,
    suffix: usize,
    safe_prefix: usize,
    safe_suffix: usize,
}

/// Membership test for the text of the edited middle of two files.
///
/// Usually only a line or two next to the middle is asked about, so the first few answers come
/// from a scan; a set is built once more are needed.
struct MiddleLines<'a, L: BlameLines + ?Sized> {
    old: &'a L,
    new: &'a L,
    old_range: std::ops::Range<usize>,
    new_range: std::ops::Range<usize>,
    ws: crate::diff::WhitespaceMode,
    scans_left: usize,
    set: Option<hashbrown::HashSet<std::borrow::Cow<'a, str>>>,
}

impl<'a, L: BlameLines + ?Sized> MiddleLines<'a, L> {
    fn lines(&self) -> impl Iterator<Item = &'a str> + '_ {
        let (old, new, ws) = (self.old, self.new, self.ws);
        self.old_range
            .clone()
            .map(move |i| old.comparison_line(i, ws))
            .chain(
                self.new_range
                    .clone()
                    .map(move |i| new.comparison_line(i, ws)),
            )
    }

    fn contains(&mut self, line: &str) -> bool {
        let ws = self.ws;
        if self.set.is_none() && self.scans_left > 0 {
            self.scans_left -= 1;
            return self.lines().any(|m| blame_lines_equal(m, line, ws));
        }
        let set = match self.set.take() {
            Some(set) => set,
            None => self
                .lines()
                .map(|m| ws.normalize_line_for_compare(m))
                .collect(),
        };
        let found = set.contains(&ws.normalize_line_for_compare(line));
        self.set = Some(set);
        found
    }
}

/// Line access shared by blob lines and plain line slices in the line-map helpers.
trait BlameLines {
    fn len(&self) -> usize;
    /// Text used to compare line `idx`, before any whitespace normalization.
    fn comparison_line(&self, idx: usize, ws: crate::diff::WhitespaceMode) -> &str;
    /// Byte length of `comparison_line(idx, WhitespaceMode::default())`.
    fn exact_len(&self, idx: usize) -> usize;
    /// Stored bytes of line `idx`, including its newline.
    fn stored_line(&self, idx: usize) -> StoredLine<'_>;
    /// Stored bytes of the last `lines` lines.
    fn tail_bytes(&self, lines: usize) -> usize;
}

/// A line's stored bytes: its text, plus a newline the text itself leaves out.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StoredLine<'a> {
    text: &'a [u8],
    implied_newline: bool,
}

impl<'a> StoredLine<'a> {
    fn rev_bytes(self) -> impl Iterator<Item = u8> + 'a {
        self.implied_newline
            .then_some(b'\n')
            .into_iter()
            .chain(self.text.iter().rev().copied())
    }
}

impl BlameLines for [&str] {
    fn len(&self) -> usize {
        <[&str]>::len(self)
    }

    fn comparison_line(&self, idx: usize, _ws: crate::diff::WhitespaceMode) -> &str {
        self[idx]
    }

    fn exact_len(&self, idx: usize) -> usize {
        self[idx].len()
    }

    /// Slice lines are records of a newline-separated file, so a line without one gains it.
    fn stored_line(&self, idx: usize) -> StoredLine<'_> {
        StoredLine {
            text: self[idx].as_bytes(),
            implied_newline: !self[idx].ends_with('\n'),
        }
    }

    fn tail_bytes(&self, lines: usize) -> usize {
        self[self.len() - lines..]
            .iter()
            .map(|l| l.len() + usize::from(!l.ends_with('\n')))
            .sum()
    }
}

impl BlameLines for BlobLines {
    fn len(&self) -> usize {
        BlobLines::len(self)
    }

    fn comparison_line(&self, idx: usize, ws: crate::diff::WhitespaceMode) -> &str {
        BlobLines::comparison_line(self, idx, ws)
    }

    fn exact_len(&self, idx: usize) -> usize {
        let start = self.line_start(idx);
        (self.line_ends[idx] + 1).min(self.bytes().len()) - start
    }

    fn stored_line(&self, idx: usize) -> StoredLine<'_> {
        StoredLine {
            text: BlobLines::comparison_line(self, idx, crate::diff::WhitespaceMode::default())
                .as_bytes(),
            implied_newline: false,
        }
    }

    fn tail_bytes(&self, lines: usize) -> usize {
        if lines == 0 {
            return 0;
        }
        self.bytes().len() - self.line_start(self.len() - lines)
    }
}

/// Size of the blocks Git compares when dropping the common tail before a blame diff.
const BLAME_TAIL_BLOCK: usize = 1024;

/// Number of trailing lines `git blame` leaves out when diffing `old` against `new`.
///
/// With no context lines, Git drops the common tail of both files in 1 KiB blocks before
/// running xdiff, keeping the line the last dropped block starts in. The dropped lines never
/// reach the diff, so they neither count as occurrences of a line nor give a hunk room to slide,
/// and the end of what remains is the end of the file for the indent heuristic. `affix` is
/// [`slide_safe_affix`] of the two files under `ws`.
fn blame_trimmed_tail_lines<L: BlameLines + ?Sized>(
    old: &L,
    new: &L,
    affix: Affix,
    ws: crate::diff::WhitespaceMode,
) -> usize {
    let (old_len, new_len) = (old.len(), new.len());
    let shorter = old_len.min(new_len);
    // Exact comparison already found the byte-identical suffix, unless it stopped at the prefix.
    let equal_lines = if !ws.normalizes_line_compare() && affix.prefix + affix.suffix < shorter {
        affix.suffix
    } else {
        (0..shorter)
            .take_while(|&j| old.stored_line(old_len - 1 - j) == new.stored_line(new_len - 1 - j))
            .count()
    };
    let mut common_bytes = new.tail_bytes(equal_lines);
    if equal_lines < shorter {
        let a = old.stored_line(old_len - 1 - equal_lines);
        let b = new.stored_line(new_len - 1 - equal_lines);
        common_bytes += a
            .rev_bytes()
            .zip(b.rev_bytes())
            .take_while(|(x, y)| x == y)
            .count();
    }
    let trimmed = common_bytes / BLAME_TAIL_BLOCK * BLAME_TAIL_BLOCK;
    if trimmed == 0 {
        return 0;
    }
    // A line is dropped when it starts after the first trimmed byte, i.e. when it and every
    // line after it fit in fewer than `trimmed` bytes.
    let (mut lo, mut hi) = (0usize, equal_lines);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if new.tail_bytes(mid) < trimmed {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Unchanged lines kept on each side of the slidable region when diffing a window of the files.
///
/// The indent heuristic scores a split from at most 20 lines before and after it, so with this
/// much context a window compacts exactly like the whole files.
const COMPACTION_CONTEXT: usize = 21;

fn blame_lines_equal(a: &str, b: &str, ws: crate::diff::WhitespaceMode) -> bool {
    if !ws.normalizes_line_compare() {
        a == b
    } else {
        ws.normalize_line_for_compare(a) == ws.normalize_line_for_compare(b)
    }
}

/// Compacted diff ops for two windows cut from a pair of files.
///
/// Like Git, the line diff itself skips the common prefix and suffix, but compaction runs over
/// the whole windows so groups can slide into the unchanged ends and the indent heuristic sees
/// the surrounding lines. The windows leave `shared_records` lines of Git's view of the files
/// out (identical on both sides); `shared` yields the comparison text of at least those whose
/// text is in the edited middle, and Myers counts their occurrences the way xdiff counts every
/// record of the files.
fn blame_compacted_line_ops<'s>(
    old: &[&str],
    new: &[&str],
    shared: impl Iterator<Item = &'s str>,
    shared_records: usize,
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
    ws: crate::diff::WhitespaceMode,
) -> Vec<DiffOp> {
    use similar::TextDiff;

    let normalized = ws.normalizes_line_compare().then(|| {
        let norm = |lines: &[&str]| -> Vec<String> {
            lines
                .iter()
                .map(|l| ws.normalize_line_for_compare(l).into_owned())
                .collect()
        };
        (norm(old), norm(new))
    });
    let normalized_refs: Option<(Vec<&str>, Vec<&str>)> = normalized.as_ref().map(|(o, n)| {
        (
            o.iter().map(String::as_str).collect(),
            n.iter().map(String::as_str).collect(),
        )
    });
    let (old_cmp, new_cmp) = match &normalized_refs {
        Some((o, n)) => (o.as_slice(), n.as_slice()),
        None => (old, new),
    };
    let (prefix, suffix) = common_affix_len(old_cmp, new_cmp, ws);
    let old_mid = &old_cmp[prefix..old_cmp.len() - suffix];
    let new_mid = &new_cmp[prefix..new_cmp.len() - suffix];
    if diff_algorithm == BlameDiffAlgorithm::Myers {
        use crate::line_interner::LineInterner;
        let mut interner = LineInterner::with_capacity(old_cmp.len() + new_cmp.len());
        let ids1 = interner.intern_all(old_cmp);
        let ids2 = interner.intern_all(new_cmp);
        let mut shared_counts = vec![0u32; interner.len()];
        for line in shared {
            if let Some(id) = interner.get(&ws.normalize_line_for_compare(line)) {
                shared_counts[id as usize] += 1;
            }
        }
        return crate::diff::line_diff_ops_git_xdiff_compacted_ids(
            &ids1,
            &ids2,
            &shared_counts,
            shared_records,
            old,
            new,
            indent_heuristic,
            ws,
        );
    }
    // Compaction reads only the changed ranges, so the middle's ops need no equal-run framing.
    let mid_ops: Vec<DiffOp> = TextDiff::configure()
        .algorithm(diff_algorithm.to_similar())
        .diff_slices(old_mid, new_mid)
        .ops()
        .iter()
        .map(|op| shift_diff_op(op, prefix))
        .collect();
    crate::diff_indent_heuristic::apply_change_compact_to_ops_with_ws(
        &mid_ops,
        old,
        new,
        indent_heuristic,
        ws,
    )
}

/// `op` with both line indexes moved forward by `by`.
fn shift_diff_op(op: &DiffOp, by: usize) -> DiffOp {
    match *op {
        DiffOp::Equal {
            old_index,
            new_index,
            len,
        } => DiffOp::Equal {
            old_index: old_index + by,
            new_index: new_index + by,
            len,
        },
        DiffOp::Delete {
            old_index,
            old_len,
            new_index,
        } => DiffOp::Delete {
            old_index: old_index + by,
            old_len,
            new_index: new_index + by,
        },
        DiffOp::Insert {
            old_index,
            new_index,
            new_len,
        } => DiffOp::Insert {
            old_index: old_index + by,
            new_index: new_index + by,
            new_len,
        },
        DiffOp::Replace {
            old_index,
            old_len,
            new_index,
            new_len,
        } => DiffOp::Replace {
            old_index: old_index + by,
            old_len,
            new_index: new_index + by,
            new_len,
        },
    }
}

/// Lengths of the common line prefix and suffix of two files (the suffix never overlaps it).
fn common_affix_len<L: BlameLines + ?Sized>(
    old: &L,
    new: &L,
    ws: crate::diff::WhitespaceMode,
) -> (usize, usize) {
    let mut prefix = 0usize;
    while prefix < old.len()
        && prefix < new.len()
        && blame_lines_equal(
            old.comparison_line(prefix, ws),
            new.comparison_line(prefix, ws),
            ws,
        )
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old.len().saturating_sub(prefix)
        && suffix < new.len().saturating_sub(prefix)
        && blame_lines_equal(
            old.comparison_line(old.len() - 1 - suffix, ws),
            new.comparison_line(new.len() - 1 - suffix, ws),
            ws,
        )
    {
        suffix += 1;
    }
    (prefix, suffix)
}

/// Map each line in `new` to its origin line index in `old` (if any).
///
/// The map is read from the diff after change compaction, the way `git blame` places hunks, so
/// an inserted or deleted block that could sit in several places is attributed where Git puts
/// it rather than where a raw common prefix/suffix would leave it. Lines are treated as
/// newline-terminated records of a file, and like `git blame` the diff leaves out the common
/// tail Git drops in 1 KiB blocks before running xdiff.
pub fn build_line_map(
    old: &[&str],
    new: &[&str],
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
) -> Vec<Option<usize>> {
    build_line_map_with_changed_flags(
        old,
        new,
        diff_algorithm,
        indent_heuristic,
        false,
        crate::diff::WhitespaceMode::default(),
    )
    .0
}

/// Like [`build_line_map_with_changed_flags`], for blob lines.
///
/// `affix` is [`slide_safe_affix`] of the two blobs.
fn build_line_map_with_changed_flags_owned(
    old: &BlobLines,
    new: &BlobLines,
    affix: Affix,
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
    with_changed_flags: bool,
    ws: crate::diff::WhitespaceMode,
) -> (Vec<Option<usize>>, Vec<bool>) {
    windowed_line_map(
        old,
        new,
        affix,
        diff_algorithm,
        indent_heuristic,
        with_changed_flags,
        ws,
    )
}

/// Like [`build_line_map`], optionally returning xdiff-style changed flags for the new side
/// from the same compacted diff ops (avoids a second full diff on blame hot paths).
fn build_line_map_with_changed_flags(
    old: &[&str],
    new: &[&str],
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
    with_changed_flags: bool,
    ws: crate::diff::WhitespaceMode,
) -> (Vec<Option<usize>>, Vec<bool>) {
    windowed_line_map(
        old,
        new,
        slide_safe_affix(old, new, ws),
        diff_algorithm,
        indent_heuristic,
        with_changed_flags,
        ws,
    )
}

/// Line map and changed flags from a compacted diff of the window that compaction can affect.
///
/// The diff sees the files as `git blame` gives them to xdiff (see
/// [`blame_trimmed_tail_lines`]). Lines outside [`slide_safe_affix`] plus
/// [`COMPACTION_CONTEXT`] map straight through, which is what compacting the whole files would
/// give them; they still count as occurrences for the Myers line classification.
fn windowed_line_map<L: BlameLines + ?Sized>(
    old: &L,
    new: &L,
    affix: Affix,
    diff_algorithm: BlameDiffAlgorithm,
    indent_heuristic: bool,
    with_changed_flags: bool,
    ws: crate::diff::WhitespaceMode,
) -> (Vec<Option<usize>>, Vec<bool>) {
    let trimmed_tail = blame_trimmed_tail_lines(old, new, affix, ws);
    let (safe_prefix, safe_suffix) = (affix.safe_prefix, affix.safe_suffix);
    let tail = safe_suffix
        .saturating_sub(COMPACTION_CONTEXT)
        .max(trimmed_tail);
    let (old_hi, new_hi) = (old.len() - tail, new.len() - tail);
    let lo = safe_prefix
        .saturating_sub(COMPACTION_CONTEXT)
        .min(old_hi)
        .min(new_hi);
    let old_w: Vec<&str> = (lo..old_hi).map(|i| old.comparison_line(i, ws)).collect();
    let new_w: Vec<&str> = (lo..new_hi).map(|i| new.comparison_line(i, ws)).collect();
    let shared_tail = new_hi..new.len() - trimmed_tail;
    let shared_records = lo + shared_tail.len();
    // Only the counts of lines in the edited middle are read, so without whitespace rules a
    // shared line of another length needs no lookup.
    let mid_lens = (!ws.normalizes_line_compare()).then(|| {
        let lens = (affix.prefix..old.len() - affix.suffix)
            .map(|i| old.exact_len(i))
            .chain((affix.prefix..new.len() - affix.suffix).map(|i| new.exact_len(i)));
        let mut seen = Vec::new();
        for len in lens {
            if seen.len() <= len {
                seen.resize(len + 1, false);
            }
            seen[len] = true;
        }
        seen
    });
    let shared = (0..lo)
        .chain(shared_tail)
        .filter(|&i| {
            mid_lens
                .as_ref()
                .is_none_or(|seen| seen.get(new.exact_len(i)).copied().unwrap_or(false))
        })
        .map(|i| new.comparison_line(i, ws));
    // `--indent-heuristic` / `diff.indentHeuristic` shifts ambiguous add/delete
    // runs like Git's xdiff, which changes which lines blame attributes (t4061).
    let ops = blame_compacted_line_ops(
        &old_w,
        &new_w,
        shared,
        shared_records,
        diff_algorithm,
        indent_heuristic,
        ws,
    );
    let window_map = crate::diff_indent_heuristic::map_new_to_old_from_ops(&ops, new_w.len());
    let map = (0..lo)
        .map(Some)
        .chain(window_map.into_iter().map(|m| m.map(|i| i + lo)))
        .chain((old_hi..old.len()).map(Some))
        .collect();
    let flags = if with_changed_flags {
        let mut flags = vec![false; new.len()];
        let window_flags =
            crate::diff_indent_heuristic::new_line_changed_flags_from_ops(&ops, new_w.len());
        flags[lo..new_hi].copy_from_slice(&window_flags);
        flags
    } else {
        Vec::new()
    };
    (map, flags)
}

/// Greedily map unresolved new lines to identical old lines (lowest use count, then distance).
fn fill_exact_text_relocate(
    old: &[&str],
    new: &[&str],
    map: &mut [Option<usize>],
    used_old: &mut [usize],
    ws: crate::diff::WhitespaceMode,
) {
    let mut by_content_norm: HashMap<String, Vec<usize>> = HashMap::new();
    if ws.normalizes_line_compare() {
        for (idx, line) in old.iter().enumerate() {
            let key = ws.normalize_line_for_compare(line).into_owned();
            by_content_norm.entry(key).or_default().push(idx);
        }
    } else {
        // Only unresolved child lines are looked up, and usually few are, so index just
        // the parent lines sharing their text (a length check skips the rest unhashed).
        let mut by_content: hashbrown::HashMap<&str, Vec<usize>> = map
            .iter()
            .zip(new)
            .filter(|(slot, _)| slot.is_none())
            .map(|(_, line)| (*line, Vec::new()))
            .collect();
        if by_content.is_empty() {
            return;
        }
        let mut lens: Vec<usize> = by_content.keys().map(|line| line.len()).collect();
        lens.sort_unstable();
        lens.dedup();
        for (idx, line) in old.iter().enumerate() {
            if lens.binary_search(&line.len()).is_ok() {
                if let Some(indices) = by_content.get_mut(line) {
                    indices.push(idx);
                }
            }
        }
        fill_exact_text_relocate_with_lookup(old, new, map, used_old, |line| {
            by_content
                .get(line)
                .filter(|indices| !indices.is_empty())
                .map(Vec::as_slice)
        });
        return;
    }

    fill_exact_text_relocate_with_lookup(old, new, map, used_old, |line| {
        let key = ws.normalize_line_for_compare(line).into_owned();
        by_content_norm.get(&key).map(Vec::as_slice)
    });
}

fn fill_exact_text_relocate_with_lookup<'a>(
    _old: &[&str],
    new: &[&str],
    map: &mut [Option<usize>],
    used_old: &mut [usize],
    mut lookup: impl FnMut(&str) -> Option<&'a [usize]>,
) {
    let mut windowed: Vec<usize> = Vec::new();
    for new_idx in 0..map.len() {
        if map[new_idx].is_some() {
            continue;
        }
        let Some(candidates) = lookup(new[new_idx]) else {
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
    build_exact_relocate_line_map_ws(old, new, exact_map, crate::diff::WhitespaceMode::default())
}

fn build_exact_relocate_line_map_ws(
    old: &[&str],
    new: &[&str],
    exact_map: &[Option<usize>],
    ws: crate::diff::WhitespaceMode,
) -> Vec<Option<usize>> {
    let mut fuzzy_map = exact_map.to_vec();
    let mut used_old = vec![0usize; old.len()];
    for old_idx in fuzzy_map.iter().flatten() {
        if *old_idx < used_old.len() {
            used_old[*old_idx] += 1;
        }
    }

    fill_exact_text_relocate(old, new, &mut fuzzy_map, &mut used_old, ws);

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
        if diff_map[child_idx].is_none() {
            // xdiff insert/replace on the child side: stay on this commit even when
            // exact-text relocation could pair the line with distant equal text (#1610).
            stop[child_idx] = true;
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
    build_fuzzy_line_map_ws(old, new, exact_map, crate::diff::WhitespaceMode::default())
}

fn build_fuzzy_line_map_ws(
    old: &[&str],
    new: &[&str],
    exact_map: &[Option<usize>],
    ws: crate::diff::WhitespaceMode,
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
    fill_exact_text_relocate(old, new, &mut fuzzy_map, &mut used_old, ws);

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

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use crate::attributes::{load_gitattributes_stack, MacroTable, ParsedGitAttributes};
use crate::commit_graph_file::CommitGraphChain;
use crate::config::ConfigSet;
use crate::crlf::{
    convert_to_worktree_eager, file_attrs_from_gitattributes, load_gitattributes_from_index,
    load_gitattributes_parsed, ConversionConfig, GitAttributes,
};
use crate::diff::{detect_renames, diff_trees, DiffEntry, DiffStatus};
use crate::error::{Error as LibError, Result};
use crate::index::Index;
use crate::objects::{
    commit_author_and_message_lenient, find_tree_entry_by_name_dir_or_file_in_data,
    parse_commit_tree_and_parents_lenient, parse_commit_tree_and_parents_prefix, parse_tree,
    Object, ObjectId, ObjectKind,
};
use crate::odb::Odb;
use crate::pack::SingleLocalPackReader;
use crate::parallel::ThreadPolicy;
use crate::repo::Repository;
use crate::repo_relpath::{format_path_stored, storage_bytes, storage_string};
use crate::wildmatch::wildmatch;

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
    /// Text of the line in the final file (lossy UTF-8 when [`Self::encoding_lossy`]).
    pub content: String,
    /// `true` when line bytes are not valid UTF-8 and [`Self::content`] used lossy decoding.
    pub encoding_lossy: bool,
    /// Exact line bytes excluding the line terminator when [`Self::encoding_lossy`]; otherwise `None`.
    pub content_bytes: Option<Vec<u8>>,
    /// Reversibly stored path in the originating commit, including rename/copy origins.
    /// Resolve this path in the attributed commit, not in the blamed revision.
    /// Recover exact filename bytes with `repo_relpath::storage_bytes`.
    pub original_path: String,
    /// Reversibly stored source path when copy detection attributed a different file.
    pub source_file: Option<String>,
    /// Line attribution passed through an ignored revision.
    pub ignored: bool,
    /// Line could not be blamed past an ignored revision.
    pub unblamable: bool,
    /// Line exists only in uncommitted content (worktree file or caller buffer); `oid` is
    /// the null object id.
    pub not_committed: bool,
}

fn final_line_display(final_text: &BlobLines, final_lineno: usize) -> BlameLineDisplay {
    final_text.line_display(final_lineno - 1)
}

#[allow(clippy::too_many_arguments)]
fn blame_line_at_path(
    oid: ObjectId,
    final_lineno: usize,
    orig_lineno: usize,
    display: BlameLineDisplay,
    blame_path: &str,
    source_file: Option<String>,
    ignored: bool,
    unblamable: bool,
) -> BlameLine {
    let original_path = source_file.clone().unwrap_or_else(|| blame_path.to_owned());
    BlameLine {
        oid,
        final_lineno,
        orig_lineno,
        content: display.content,
        encoding_lossy: display.encoding_lossy,
        content_bytes: display.content_bytes,
        original_path,
        source_file,
        ignored,
        unblamable,
        not_committed: false,
    }
}

fn blame_line_from_parent(
    pb: &BlameLine,
    final_lineno: usize,
    display: BlameLineDisplay,
    source_file: Option<String>,
    ignored: bool,
    unblamable: bool,
) -> BlameLine {
    BlameLine {
        oid: pb.oid,
        final_lineno,
        orig_lineno: pb.orig_lineno,
        content: display.content,
        encoding_lossy: display.encoding_lossy,
        content_bytes: display.content_bytes,
        original_path: pb.original_path.clone(),
        source_file,
        ignored,
        unblamable,
        not_committed: false,
    }
}

/// Resolve a file path through nested trees to get the blob OID + mode.
fn resolve_path_in_tree_entry(
    odb: &Odb,
    tree_oid: &ObjectId,
    path: &str,
) -> Result<Option<(ObjectId, u32)>> {
    Ok(odb
        .tree_entry_at_path(tree_oid, &storage_bytes(path))?
        .filter(|e| e.mode != 0o040000)
        .map(|e| (e.oid, e.mode)))
}

/// Split content into lines. A final line without a trailing newline is still a line
/// (matches git blame / `wc -l` + 1 semantics in upstream tests).
#[cfg(test)]
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
    /// Merge hand-off this line arrived through (`0` for the starting suspect). Lines of
    /// different hand-offs can share a queued suspect but never compete for a parent line.
    origin: u32,
}

/// A commit that still owns lines of the blamed file at `path`.
struct BlameSuspect {
    oid: ObjectId,
    blob: ObjectId,
    mode: u32,
    path: String,
    /// Lines of `blob`, carried along so the walk does not reload them.
    lines: Option<Arc<BlobLines>>,
    pending: Vec<TrackedLine>,
}

/// Uncommitted final text blamed as a commit on top of `parents` (Git's fake working-tree
/// commit). It has no object id: its lines are diffed straight from memory.
struct BlameOverlay {
    lines: Arc<BlobLines>,
    /// The start revision, then any `MERGE_HEAD` commits, in Git's parent order.
    parents: Vec<ObjectId>,
    /// Parent paths copy detection may search for uncommitted lines (`None`: every path).
    copy_paths: Option<HashSet<String>>,
}

/// History-walk settings shared by the fake-commit pass and the copy-source walks it starts.
struct OverlayWalk<'a> {
    path: &'a str,
    ignore_revs: &'a HashSet<ObjectId>,
    diff_algorithm: BlameDiffAlgorithm,
    textconv_ctx: Option<&'a BlameTextconvContext>,
    use_textconv: bool,
    copy_depth: usize,
    first_parent_only: bool,
    grafts: &'a HashMap<ObjectId, Vec<ObjectId>>,
    indent_heuristic: bool,
}

/// Pass the overlay's lines to its parents in order, like a merge commit hands lines to the
/// first parent whose version matches after diffing.
///
/// Lines no parent (or copy source) explains are pushed to `result` as not committed. The
/// first parent's share is returned so the walk continues with it; later parents' shares
/// are queued under fresh hand-off origins.
#[allow(clippy::too_many_arguments)]
fn pass_overlay_to_parents(
    odb: &Odb,
    session: &mut BlameSession<'_>,
    overlay: &BlameOverlay,
    walk: &OverlayWalk<'_>,
    pending: Vec<TrackedLine>,
    final_text: &BlobLines,
    commit_cache: &mut BlameCommitCache,
    queue: &mut SuspectQueue,
    next_origin: &mut u32,
    result: &mut Vec<BlameLine>,
) -> Result<Option<BlameSuspect>> {
    let ws = session.whitespace;
    let cur = overlay.lines.as_ref();
    if session.merge_line_stamps.len() < overlay.parents.len() {
        session
            .merge_line_stamps
            .resize_with(overlay.parents.len(), LineUseStamp::default);
    }
    let mut remaining = pending;
    let mut first = None;
    let parent_count = if walk.first_parent_only {
        1
    } else {
        overlay.parents.len()
    };
    for (i, &parent) in overlay.parents.iter().take(parent_count).enumerate() {
        if remaining.is_empty() {
            break;
        }
        let Some((blob, mode)) =
            session.resolve_blob_at_commit(odb, parent, walk.path, commit_cache)?
        else {
            continue;
        };
        let par = session.load_blob_lines(blob, walk.path, mode)?;
        let (map, _) = build_line_map_with_changed_flags_owned(
            &par,
            cur,
            slide_safe_affix(par.as_ref(), cur, ws),
            walk.diff_algorithm,
            walk.indent_heuristic,
            false,
            ws,
        );
        session.merge_line_stamps[i].begin_pass(par.len());
        let origin = if i == 0 {
            0
        } else {
            *next_origin += 1;
            *next_origin
        };
        let mut handed = Vec::new();
        let mut unmatched = Vec::new();
        for t in remaining {
            let matched = map.get(t.current_idx).copied().flatten().filter(|&p_idx| {
                p_idx < par.len()
                    && session.merge_line_stamps[i].is_unused(p_idx)
                    && blame_lines_equal(
                        par.comparison_line(p_idx, ws),
                        cur.comparison_line(t.current_idx, ws),
                        ws,
                    )
            });
            match matched {
                Some(p_idx) => {
                    session.merge_line_stamps[i].mark(p_idx);
                    handed.push(TrackedLine {
                        current_idx: p_idx,
                        origin,
                        ..t
                    });
                }
                None => unmatched.push(t),
            }
        }
        remaining = unmatched;
        if handed.is_empty() {
            continue;
        }
        let suspect = BlameSuspect {
            oid: parent,
            blob,
            mode,
            path: walk.path.to_owned(),
            lines: Some(par),
            pending: handed,
        };
        if i == 0 {
            first = Some(suspect);
        } else {
            queue.push(suspect, || blame_suspect_date(session, &parent));
        }
    }

    if walk.copy_depth >= 1 && !remaining.is_empty() {
        if let Some(&parent) = overlay.parents.first() {
            let parent_commit =
                get_blame_commit(odb, parent, commit_cache, session.commit_graph.as_deref())?;
            let (left, _handoff, copy_lines) = copy_detect_attribute_pending(
                odb,
                session,
                remaining,
                &overlay.lines,
                final_text,
                walk.path,
                parent,
                &parent_commit.tree,
                &parent_commit.tree,
                walk.path,
                walk.copy_depth,
                false,
                Some(overlay.copy_paths.as_ref()),
                None,
                walk.ignore_revs,
                walk.diff_algorithm,
                walk.textconv_ctx,
                walk.use_textconv,
                walk.first_parent_only,
                walk.grafts,
                walk.indent_heuristic,
            )?;
            result.extend(copy_lines);
            remaining = left;
        }
    }

    let null = ObjectId::null(odb.hash_algo());
    for t in remaining {
        result.push(BlameLine {
            oid: null,
            final_lineno: t.final_lineno,
            orig_lineno: t.current_idx + 1,
            content: final_line_display(final_text, t.final_lineno).content,
            encoding_lossy: final_line_display(final_text, t.final_lineno).encoding_lossy,
            content_bytes: final_line_display(final_text, t.final_lineno).content_bytes,
            original_path: walk.path.to_owned(),
            source_file: None,
            ignored: false,
            unblamable: false,
            not_committed: true,
        });
    }
    Ok(first)
}

/// Heap position of a queued suspect: newest committer date first, ties in queue order.
#[derive(PartialEq, Eq)]
struct SuspectSlot {
    date: i64,
    seq: u64,
    oid: ObjectId,
    path: String,
}

impl Ord for SuspectSlot {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.date
            .cmp(&other.date)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for SuspectSlot {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Suspects waiting to pass blame to their parents, keyed by `(commit, path)`.
///
/// Lines handed to a suspect that is already queued join its pending set. Processing in
/// committer-date order makes merge paths meet before the shared commit is popped, so it is
/// diffed against its parents once; only clock skew (an ancestor dated after a descendant)
/// can hand it lines after it was popped and queue it again.
#[derive(Default)]
struct SuspectQueue {
    heap: BinaryHeap<SuspectSlot>,
    entries: HashMap<(ObjectId, String), BlameSuspect>,
    seq: u64,
}

impl SuspectQueue {
    fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Queue `suspect`, merging it into an already queued suspect for the same commit and path.
    ///
    /// `date` is only evaluated for a new queue entry.
    fn push(&mut self, suspect: BlameSuspect, date: impl FnOnce() -> i64) {
        let key = (suspect.oid, suspect.path);
        if let Some(queued) = self.entries.get_mut(&key) {
            queued.pending.extend(suspect.pending);
            if queued.lines.is_none() {
                queued.lines = suspect.lines;
            }
            return;
        }
        self.heap.push(SuspectSlot {
            date: date(),
            seq: self.seq,
            oid: key.0,
            path: key.1.clone(),
        });
        self.seq += 1;
        self.entries.insert(
            key.clone(),
            BlameSuspect {
                oid: key.0,
                path: key.1,
                ..suspect
            },
        );
    }

    /// Remove the newest suspect.
    fn pop(&mut self) -> Option<BlameSuspect> {
        let slot = self.heap.pop()?;
        self.entries.remove(&(slot.oid, slot.path))
    }
}

/// Committer date used to order suspects (commit-graph when available, else the commit header).
fn blame_suspect_date(session: &mut BlameSession<'_>, oid: &ObjectId) -> i64 {
    if let Some((_, date)) = session
        .commit_graph
        .as_ref()
        .and_then(|chain| chain.graph_commit(oid))
    {
        return date;
    }
    session
        .pack
        .read(session.odb, oid)
        .ok()
        .and_then(|obj| {
            crate::objects::CommitRef::parse(&obj.data)
                .ok()
                .map(|c| c.committer_unix_seconds_for_ordering())
        })
        .unwrap_or(0)
}

/// Cached config and attributes for optional textconv during blame.
#[derive(Debug, Clone)]
pub struct BlameTextconvContext {
    /// Repository configuration snapshot.
    pub config: ConfigSet,
    conversion: ConversionConfig,
    /// Parsed `.gitattributes` rules for the repository.
    pub attrs: GitAttributes,
    attr_macros: MacroTable,
    diff_attrs: Vec<DiffAttrRule>,
    filter_process: std::sync::Arc<crate::repo_caches::RepoCaches>,
}

impl BlameTextconvContext {
    /// Build textconv context from the open repository (config, attributes, filters).
    pub fn new(repo: &Repository) -> Self {
        let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
        let conversion = ConversionConfig::from_config(&config);
        let parsed = load_attr_parsed(repo);
        let attrs = parsed.rules.clone();
        let attr_macros = parsed.macros;
        let diff_attrs = load_diff_attr_rules(repo);
        Self {
            config,
            conversion,
            attrs,
            attr_macros,
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

fn load_attr_parsed(repo: &Repository) -> ParsedGitAttributes {
    if let Some(work_tree) = repo.work_tree() {
        if let Ok(p) = load_gitattributes_stack(repo, work_tree) {
            if !p.rules.is_empty() {
                return p;
            }
        }
        let p = load_gitattributes_parsed(work_tree);
        if !p.rules.is_empty() {
            return p;
        }
    }

    if let Ok(index) = repo.load_index() {
        if let Some(wt) = repo.work_tree() {
            if let Ok(p) = crate::attributes::load_gitattributes_from_index(&index, repo.odb(), wt)
            {
                return p;
            }
        }
        return ParsedGitAttributes {
            rules: load_gitattributes_from_index(&index, repo.odb()),
            ..ParsedGitAttributes::default()
        };
    }

    ParsedGitAttributes::default()
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
        // Ephemeral textconv input (not a repo lock); retry on name collision.
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
) -> Result<Vec<u8>> {
    let obj = read_object_for_blame(odb, oid, None)?;
    if obj.kind != ObjectKind::Blob {
        return Err(LibError::Message("expected blob object".to_string()));
    }

    if !use_textconv || !is_regular_mode(mode) {
        return Ok(obj.data);
    }

    let Some(ctx) = textconv_ctx else {
        return Ok(obj.data);
    };
    let Some(command) = resolve_textconv_command(ctx, path) else {
        return Ok(obj.data);
    };

    let attrs =
        file_attrs_from_gitattributes(&ctx.attrs, &ctx.attr_macros, path, false, &ctx.config);
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
    run_textconv_command(&command, &worktree_data)
        .or_else(|_| run_textconv_command(&command, &obj.data))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BlobCacheKey {
    oid: ObjectId,
    path: String,
    mode: u32,
}

/// Display metadata for one line in a blamed file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlameLineDisplay {
    content: String,
    encoding_lossy: bool,
    content_bytes: Option<Vec<u8>>,
}

fn latin1_bytes_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

#[derive(Clone)]
enum BlobStorage {
    Utf8(Arc<str>),
    Binary(Arc<Vec<u8>>),
}

/// Blob bytes split into lines without per-line `String` allocations.
#[derive(Clone)]
struct BlobLines {
    storage: BlobStorage,
    /// Exclusive end byte offset of each line in the blob.
    line_ends: Arc<Vec<usize>>,
    compare_bodies: OnceLock<Arc<Vec<String>>>,
    compare_records: OnceLock<Arc<Vec<String>>>,
}

impl BlobLines {
    fn from_bytes(content: Vec<u8>) -> Self {
        let unterminated = !content.is_empty() && !content.ends_with(b"\n");
        let count = memchr::memchr_iter(b'\n', &content).count() + usize::from(unterminated);
        let mut line_ends = Vec::with_capacity(count);
        line_ends.extend(memchr::memchr_iter(b'\n', &content));
        if unterminated {
            line_ends.push(content.len());
        }
        let storage = match std::str::from_utf8(&content) {
            Ok(text) => BlobStorage::Utf8(Arc::from(text)),
            Err(_) => BlobStorage::Binary(Arc::new(content)),
        };
        Self {
            storage,
            line_ends: Arc::new(line_ends),
            compare_bodies: OnceLock::new(),
            compare_records: OnceLock::new(),
        }
    }

    #[cfg(test)]
    fn from_content(content: String) -> Self {
        Self::from_bytes(content.into_bytes())
    }

    fn bytes(&self) -> &[u8] {
        match &self.storage {
            BlobStorage::Utf8(s) => s.as_bytes(),
            BlobStorage::Binary(b) => b.as_ref(),
        }
    }

    fn utf8(&self) -> Option<&str> {
        match &self.storage {
            BlobStorage::Utf8(s) => Some(s.as_ref()),
            BlobStorage::Binary(_) => None,
        }
    }

    fn len(&self) -> usize {
        self.line_ends.len()
    }

    fn line_start(&self, idx: usize) -> usize {
        idx.checked_sub(1)
            .map_or(0, |prev| self.line_ends[prev] + 1)
    }

    fn line_body_bytes(&self, idx: usize) -> &[u8] {
        &self.bytes()[self.line_start(idx)..self.line_ends[idx]]
    }

    fn line(&self, idx: usize) -> &str {
        if let Some(text) = self.utf8() {
            let start = self.line_start(idx);
            let end = self.line_ends[idx];
            return &text[start..end];
        }
        // Only used for tests and lossy display helpers; comparison uses Latin-1 strings.
        self.compare_bodies()
            .get(idx)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn line_display(&self, idx: usize) -> BlameLineDisplay {
        let body = self.line_body_bytes(idx);
        let encoding_lossy = std::str::from_utf8(body).is_err();
        let content = String::from_utf8_lossy(body).into_owned();
        let content_bytes = encoding_lossy.then(|| body.to_vec());
        BlameLineDisplay {
            content,
            encoding_lossy,
            content_bytes,
        }
    }

    fn compare_bodies(&self) -> &Arc<Vec<String>> {
        self.compare_bodies.get_or_init(|| {
            Arc::new(
                (0..self.len())
                    .map(|i| latin1_bytes_to_string(self.line_body_bytes(i)))
                    .collect(),
            )
        })
    }

    fn compare_records(&self) -> &Arc<Vec<String>> {
        self.compare_records.get_or_init(|| {
            Arc::new(
                (0..self.len())
                    .map(|i| {
                        let start = self.line_start(i);
                        let end = (self.line_ends[i] + 1).min(self.bytes().len());
                        latin1_bytes_to_string(&self.bytes()[start..end])
                    })
                    .collect(),
            )
        })
    }

    /// Exact comparison borrows the full record; whitespace modes compare line bodies.
    fn comparison_line(&self, idx: usize, ws: crate::diff::WhitespaceMode) -> &str {
        if let Some(text) = self.utf8() {
            if ws.normalizes_line_compare() {
                let start = self.line_start(idx);
                let end = self.line_ends[idx];
                return &text[start..end];
            }
            let end = (self.line_ends[idx] + 1).min(text.len());
            return &text[self.line_start(idx)..end];
        }
        if ws.normalizes_line_compare() {
            return &self.compare_bodies()[idx];
        }
        &self.compare_records()[idx]
    }

    fn comparison_refs(&self, ws: crate::diff::WhitespaceMode) -> Vec<&str> {
        (0..self.len())
            .map(|i| self.comparison_line(i, ws))
            .collect()
    }

    fn to_vec(&self) -> Vec<String> {
        (0..self.len())
            .map(|i| self.line_display(i).content)
            .collect()
    }
}

/// Marks line indices used in one merge-parent pass without clearing a full `Vec<bool>`.
#[derive(Default)]
struct LineUseStamp {
    generation: u32,
    marks: Vec<u32>,
}

impl LineUseStamp {
    fn begin_pass(&mut self, min_len: usize) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.marks.fill(0);
            self.generation = 1;
        }
        if self.marks.len() < min_len {
            self.marks.resize(min_len, 0);
        }
    }

    fn mark(&mut self, idx: usize) {
        if idx >= self.marks.len() {
            self.marks.resize(idx + 1, 0);
        }
        self.marks[idx] = self.generation;
    }

    fn is_unused(&self, idx: usize) -> bool {
        self.marks.get(idx).is_none_or(|&g| g != self.generation)
    }
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
    whitespace: crate::diff::WhitespaceMode,
}

type BloomKeyCacheEntry = (String, u32, Arc<Vec<Vec<u32>>>);

#[derive(Clone)]
struct PairMapCacheValue {
    diff_line_map: Arc<Vec<Option<usize>>>,
    line_map: Arc<Vec<Option<usize>>>,
    stop_blame_on_current: Arc<Vec<bool>>,
}

/// Number of cached blob versions above which unreferenced ones are dropped.
const BLOB_LINES_CACHE_SOFT_LIMIT: usize = 64;

/// Per-blame caches for blob lines, tree path resolution, and parent/child line maps.
struct BlameSession<'a> {
    odb: &'a Odb,
    textconv_ctx: Option<&'a BlameTextconvContext>,
    use_textconv: bool,
    whitespace: crate::diff::WhitespaceMode,
    commit_graph: Option<Arc<CommitGraphChain>>,
    bloom_pathspec: Vec<String>,
    bloom_key_cache: Option<BloomKeyCacheEntry>,
    blob_lines: HashMap<BlobCacheKey, Arc<BlobLines>>,
    path_entries: HashMap<PathCacheKey, Option<(ObjectId, u32)>>,
    commit_path_blob: HashMap<(ObjectId, String), Option<(ObjectId, u32)>>,
    pair_maps: HashMap<PairMapCacheKey, PairMapCacheValue>,
    /// Keep parent/child line maps for reuse. Only nested walks (ignored merges, copy
    /// sources) diff a pair twice; a plain walk diffs each pair once.
    cache_pair_maps: bool,
    identity_line_maps: HashMap<usize, Arc<Vec<Option<usize>>>>,
    pack: SessionPackReader,
    merge_line_stamps: Vec<LineUseStamp>,
    #[cfg(test)]
    diffed_pairs: Vec<(ObjectId, ObjectId)>,
}

impl<'a> BlameSession<'a> {
    fn new(
        odb: &'a Odb,
        textconv_ctx: Option<&'a BlameTextconvContext>,
        use_textconv: bool,
        thread_policy: &ThreadPolicy,
    ) -> Self {
        let _ = thread_policy;
        Self {
            odb,
            textconv_ctx,
            use_textconv,
            whitespace: crate::diff::WhitespaceMode::default(),
            commit_graph: None,
            bloom_pathspec: Vec::new(),
            bloom_key_cache: None,
            blob_lines: HashMap::new(),
            path_entries: HashMap::new(),
            commit_path_blob: HashMap::new(),
            pair_maps: HashMap::new(),
            cache_pair_maps: false,
            identity_line_maps: HashMap::new(),
            pack: SessionPackReader::default(),
            merge_line_stamps: Vec::new(),
            #[cfg(test)]
            diffed_pairs: Vec::new(),
        }
    }

    fn identity_line_map(&mut self, len: usize) -> Arc<Vec<Option<usize>>> {
        if let Some(map) = self.identity_line_maps.get(&len) {
            return Arc::clone(map);
        }
        let map: Arc<Vec<Option<usize>>> = Arc::new((0..len).map(Some).collect());
        self.identity_line_maps.insert(len, Arc::clone(&map));
        map
    }

    /// Look up one entry of `tree_oid` without retaining the tree data.
    fn tree_entry(&mut self, tree_oid: ObjectId, name: &[u8]) -> Result<Option<(ObjectId, u32)>> {
        // Whole-object reads (unlike `read_into`) feed the delta-base cache; consecutive
        // root trees are long delta chains against each other, so the cache is what keeps
        // a first-parent walk from re-inflating every chain from its base.
        let obj = self.pack.read(self.odb, &tree_oid)?;
        if obj.kind != ObjectKind::Tree {
            return Err(LibError::Message("expected tree".to_string()));
        }
        let oid_len = self.odb.hash_algo().len();
        Ok(
            find_tree_entry_by_name_dir_or_file_in_data(&obj.data, oid_len, name)?
                .map(|e| (e.oid, e.mode)),
        )
    }

    /// Drop cached blob lines that no queued suspect holds once the cache outgrows
    /// [`BLOB_LINES_CACHE_SOFT_LIMIT`], so a deep walk keeps only the versions it still needs.
    fn prune_blob_lines(&mut self) {
        if self.blob_lines.len() > BLOB_LINES_CACHE_SOFT_LIMIT {
            self.blob_lines
                .retain(|_, lines| Arc::strong_count(lines) > 1);
        }
    }

    fn set_bloom_path(&mut self, path: &str) {
        if self.bloom_pathspec.len() == 1 && self.bloom_pathspec[0] == path {
            return;
        }
        self.bloom_pathspec.clear();
        self.bloom_pathspec.push(path.to_owned());
        self.bloom_key_cache = None;
    }

    fn bloom_keys_for_path(
        &mut self,
        path: &str,
        settings: &crate::bloom::BloomFilterSettings,
    ) -> Arc<Vec<Vec<u32>>> {
        let version = settings.hash_version;
        let needs_refresh = self
            .bloom_key_cache
            .as_ref()
            .is_none_or(|(p, v, _)| p != path || *v != version);
        if needs_refresh {
            let bytes = storage_bytes(path);
            let keys = crate::bloom::bloom_keyvec_for_bytes(&bytes, settings);
            self.bloom_key_cache = Some((path.to_owned(), version, Arc::new(keys)));
        }
        if let Some((_, _, keys)) = &self.bloom_key_cache {
            return Arc::clone(keys);
        }
        Arc::new(Vec::new())
    }

    fn resolve_blob_at_commit(
        &mut self,
        odb: &Odb,
        commit: ObjectId,
        path: &str,
        commit_cache: &mut BlameCommitCache,
    ) -> Result<Option<(ObjectId, u32)>> {
        let cache_key = (commit, path.to_owned());
        if let Some(resolved) = self.commit_path_blob.get(&cache_key) {
            return Ok(*resolved);
        }
        let meta = get_blame_commit(odb, commit, commit_cache, self.commit_graph.as_deref())?;
        let resolved = self.resolve_path_in_tree(meta.tree, path)?;
        self.commit_path_blob.insert(cache_key, resolved);
        Ok(resolved)
    }

    fn line_refs(lines: &BlobLines) -> Vec<&str> {
        (0..lines.len()).map(|i| lines.line(i)).collect()
    }

    fn load_blob_lines(&mut self, oid: ObjectId, path: &str, mode: u32) -> Result<Arc<BlobLines>> {
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
        let lines = Arc::new(BlobLines::from_bytes(content));
        self.blob_lines.insert(key, Arc::clone(&lines));
        Ok(lines)
    }

    fn resolve_path_in_tree(
        &mut self,
        tree_oid: ObjectId,
        path: &str,
    ) -> Result<Option<(ObjectId, u32)>> {
        let bytes = storage_bytes(path);
        self.resolve_path_in_tree_bytes(tree_oid, &bytes)
    }

    // Decode only at the storage boundary. A subtree suffix may itself start with the
    // reserved storage prefix, so recursion must keep using the original bytes.
    fn resolve_path_in_tree_bytes(
        &mut self,
        tree_oid: ObjectId,
        path: &[u8],
    ) -> Result<Option<(ObjectId, u32)>> {
        let key = PathCacheKey {
            tree: tree_oid,
            path: storage_string(path),
        };
        if let Some(entry) = self.path_entries.get(&key) {
            return Ok(*entry);
        }
        // Memoizing each `(subtree, remaining path)` level means a walk only inflates
        // the trees that changed between commits: unchanged subtrees hit the cache.
        let (head, rest) = match path.iter().position(|&b| b == b'/') {
            Some(pos) => (&path[..pos], Some(&path[pos + 1..])),
            None => (path, None),
        };
        let resolved = match (self.tree_entry(tree_oid, head)?, rest) {
            (Some((oid, 0o040000)), Some(rest)) => self.resolve_path_in_tree_bytes(oid, rest)?,
            (Some((oid, mode)), None) if mode != 0o040000 => Some((oid, mode)),
            _ => None,
        };
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
        par_lines: &BlobLines,
        cur_lines: &BlobLines,
        affix: Affix,
        diff_algorithm: BlameDiffAlgorithm,
        indent_heuristic: bool,
        is_ignored: bool,
        include_stop_blame: bool,
    ) -> PairMapCacheValue {
        #[cfg(test)]
        self.diffed_pairs.push((parent_oid, child_oid));
        let ws = self.whitespace;
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
            whitespace: ws,
        };
        if self.cache_pair_maps {
            if let Some(maps) = self.pair_maps.get(&key) {
                return maps.clone();
            }
        }
        let identity_map = parent_oid == child_oid && par_lines.len() == cur_lines.len();
        let (diff_line_map, xdiff_changed) = if identity_map {
            (
                (0..cur_lines.len()).map(Some).collect(),
                vec![false; cur_lines.len()],
            )
        } else if include_stop_blame {
            build_line_map_with_changed_flags_owned(
                par_lines,
                cur_lines,
                affix,
                diff_algorithm,
                indent_heuristic,
                true,
                ws,
            )
        } else {
            (
                build_line_map_with_changed_flags_owned(
                    par_lines,
                    cur_lines,
                    affix,
                    diff_algorithm,
                    indent_heuristic,
                    false,
                    ws,
                )
                .0,
                Vec::new(),
            )
        };
        let (line_map, stop_blame_on_current) = if include_stop_blame {
            if !is_ignored
                && par_lines.len() == cur_lines.len()
                && diff_line_map.iter().all(|m| m.is_some())
                && xdiff_changed.iter().all(|&f| !f)
            {
                (diff_line_map.clone(), vec![false; cur_lines.len()])
            } else {
                let par_refs = par_lines.comparison_refs(ws);
                let cur_refs = cur_lines.comparison_refs(ws);
                let mut line_map =
                    build_exact_relocate_line_map_ws(&par_refs, &cur_refs, &diff_line_map, ws);
                if is_ignored {
                    // Ignored revisions deliberately recover rewritten bodies by similarity.
                    line_map = build_fuzzy_line_map_ws(
                        &Self::line_refs(par_lines),
                        &Self::line_refs(cur_lines),
                        &line_map,
                        ws,
                    );
                }
                let stop = if par_lines.len() == cur_lines.len() {
                    blame_stops_on_current_commit(&diff_line_map, &line_map, &xdiff_changed)
                } else {
                    Vec::new()
                };
                (line_map, stop)
            }
        } else {
            (Vec::new(), Vec::new())
        };
        let value = PairMapCacheValue {
            diff_line_map: Arc::new(diff_line_map),
            line_map: Arc::new(line_map),
            stop_blame_on_current: Arc::new(stop_blame_on_current),
        };
        if self.cache_pair_maps {
            self.pair_maps.insert(key, value.clone());
        }
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

/// Pack reader held for one blame walk, opened on first use.
///
/// [`Odb::read`] re-opens a pack reader per call; a walk over tens of thousands of commits
/// and trees keeps one instead. Any miss or error falls back to the [`Odb`], so this is
/// purely a fast path.
#[derive(Default)]
struct SessionPackReader {
    reader: Option<SingleLocalPackReader>,
    opened: bool,
}

impl SessionPackReader {
    fn get(&mut self, odb: &Odb) -> Option<&mut SingleLocalPackReader> {
        if !self.opened {
            self.opened = true;
            self.reader = odb.open_session_pack_reader().ok().flatten();
        }
        self.reader.as_mut()
    }

    /// Read an object, preferring the session pack (which feeds the delta-base cache).
    fn read(&mut self, odb: &Odb, oid: &ObjectId) -> Result<Object> {
        if let Some(Ok(obj)) = self.get(odb).map(|r| r.read_object(oid)) {
            return Ok(obj);
        }
        read_object_for_blame(odb, oid, None)
    }

    /// Tree and parents of `oid`, inflating only the commit header when it is packed whole.
    ///
    /// Both the header-prefix and the whole-object paths use Git's canonical tree/parent
    /// parse, which ignores the rest of the headers; a prefix is only trusted when it holds
    /// the complete parent list, so the result never depends on how the object is stored.
    fn commit_tree_and_parents(
        &mut self,
        odb: &Odb,
        oid: &ObjectId,
    ) -> Result<(ObjectId, Vec<ObjectId>)> {
        let hex_len = odb.hash_algo().hex_len();
        if let Some(reader) = self.get(odb) {
            if let Ok(Some(prefix)) = reader.read_commit_author_prefix_unordered(oid) {
                if let Some(parsed) = parse_commit_tree_and_parents_prefix(prefix, hex_len) {
                    return Ok(parsed);
                }
            }
            reader.clear_commit_header_prefix();
        }
        let mut data = Vec::new();
        if odb.read_into(oid, &mut data)? != ObjectKind::Commit {
            return Err(LibError::CorruptObject(format!(
                "{} is not a commit",
                oid.to_hex()
            )));
        }
        parse_commit_tree_and_parents_lenient(&data, hex_len).ok_or_else(|| {
            LibError::CorruptObject(format!("commit {} has no valid tree header", oid.to_hex()))
        })
    }
}

/// Commit metadata memoized across one blame walk.
#[derive(Default)]
struct BlameCommitCache {
    metas: HashMap<ObjectId, Arc<BlameCommitMeta>>,
    pack: SessionPackReader,
}

/// Walk history and attribute each line in a blob at `start_oid` to a commit.
///
/// When `line_range` is set, only lines whose 1-based final line number lies in the range
/// are walked and returned.
///
/// `file_path` is repository-relative and uses `repo_relpath::storage_string`,
/// as do returned `original_path` and `source_file` values. Decoding preserves exact
/// filename bytes and does not recursively interpret reserved-prefix names.
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
    whitespace: crate::diff::WhitespaceMode,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<BlameLine>> {
    let bounded_odb = line_range.as_ref().map(|_| odb.with_bounded_pack_cache());
    let odb = bounded_odb.as_ref().unwrap_or(odb);
    let mut session = BlameSession::new(odb, textconv_ctx, use_textconv, thread_policy);
    session.whitespace = whitespace;
    session.cache_pair_maps = !ignore_revs.is_empty() || copy_depth > 0;
    let revision_label = start_oid.to_hex();
    let run = || {
        compute_blame_with_session(
            odb,
            start_oid,
            &revision_label,
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
            None,
            &mut session,
        )
    };
    if bounded_odb.is_some() {
        odb.with_pack_read_context(run)
    } else {
        run()
    }
}

fn blame_path_missing_in_revision(path: &str, revision: &str) -> LibError {
    crate::revwalk::rev_parse_error::RevParseError::PathNotInRevision {
        path: path.to_owned(),
        revision: revision.to_owned(),
    }
    .into()
}

/// Advance `start_oid` along first parents while `path` resolves to `blob_oid`.
#[allow(clippy::too_many_arguments)]
fn fast_forward_same_blob(
    odb: &Odb,
    session: &mut BlameSession<'_>,
    mut oid: ObjectId,
    blob_oid: ObjectId,
    blob_mode: u32,
    path: &str,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    commit_cache: &mut BlameCommitCache,
) -> Result<ObjectId> {
    session.set_bloom_path(path);
    loop {
        if grafts.contains_key(&oid) {
            break;
        }
        let parent_oid = if let Some(chain) = session.commit_graph.as_deref() {
            if let Some((parents, _)) = chain.graph_commit(&oid) {
                parents.first().copied()
            } else {
                commit_parents_for_blame(odb, oid, grafts, commit_cache, session)?
                    .first()
                    .copied()
            }
        } else {
            commit_parents_for_blame(odb, oid, grafts, commit_cache, session)?
                .first()
                .copied()
        };
        let Some(parent_oid) = parent_oid else {
            break;
        };
        if session.commit_graph.is_some()
            && bloom_may_skip_to_first_parent(
                session,
                odb,
                oid,
                parent_oid,
                path,
                grafts,
                commit_cache,
                true,
            )?
        {
            session
                .commit_path_blob
                .insert((parent_oid, path.to_owned()), Some((blob_oid, blob_mode)));
            oid = parent_oid;
            continue;
        }
        let parent_tree = if let Some(chain) = session.commit_graph.as_deref() {
            if let Some(tree) = chain.graph_commit_tree(&parent_oid) {
                tree
            } else {
                get_blame_commit(odb, parent_oid, commit_cache, Some(chain))?.tree
            }
        } else {
            get_blame_commit(odb, parent_oid, commit_cache, None)?.tree
        };
        let Some((p_blob, _mode)) = session.resolve_path_in_tree(parent_tree, path)? else {
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
    revision_label: &str,
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
    overlay: Option<&BlameOverlay>,
    session: &mut BlameSession<'_>,
) -> Result<Vec<BlameLine>> {
    let blamed_path = file_path.to_owned();
    let mut commit_cache = BlameCommitCache::default();
    let (start_lines, blob_oid, blob_mode) = match overlay {
        Some(overlay) => (
            Arc::clone(&overlay.lines),
            ObjectId::null(odb.hash_algo()),
            0,
        ),
        None => {
            let start_commit = get_blame_commit(
                odb,
                start_oid,
                &mut commit_cache,
                session.commit_graph.as_deref(),
            )?;
            let (blob_oid, blob_mode) = session
                .resolve_path_in_tree(start_commit.tree, file_path)?
                .ok_or_else(|| blame_path_missing_in_revision(file_path, revision_label))?;
            let lines = session.load_blob_lines(blob_oid, file_path, blob_mode)?;
            (lines, blob_oid, blob_mode)
        }
    };
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
                    origin: 0,
                })
            })
            .collect(),
        None => (0..num_lines)
            .map(|i| TrackedLine {
                final_lineno: i + 1,
                current_idx: i,
                ignored: false,
                source_path: None,
                origin: 0,
            })
            .collect(),
    };

    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let mut result: Vec<BlameLine> = Vec::with_capacity(num_lines);
    let final_text: &BlobLines = &start_lines;

    let mut current_oid = start_oid;
    let mut current_blob_oid = blob_oid;
    let mut current_blob_mode = blob_mode;
    let mut current_path = file_path.to_string();
    let mut queue = SuspectQueue::default();
    let mut next_origin: u32 = 0;
    let merge_aware = !first_parent_only;

    session.set_bloom_path(&current_path);
    let mut walk_lines: Option<(ObjectId, Arc<BlobLines>)> =
        Some((current_blob_oid, Arc::clone(&start_lines)));

    if let Some(overlay) = overlay {
        let walk = OverlayWalk {
            path: file_path,
            ignore_revs,
            diff_algorithm,
            textconv_ctx,
            use_textconv,
            copy_depth,
            first_parent_only,
            grafts,
            indent_heuristic,
        };
        let first = pass_overlay_to_parents(
            odb,
            session,
            overlay,
            &walk,
            std::mem::take(&mut pending),
            final_text,
            &mut commit_cache,
            &mut queue,
            &mut next_origin,
            &mut result,
        )?;
        walk_lines = None;
        if let Some(first) = first {
            current_oid = first.oid;
            current_blob_oid = first.blob;
            current_blob_mode = first.mode;
            walk_lines = first.lines.map(|lines| (first.blob, lines));
            pending = first.pending;
        }
    }

    // Each iteration examines the suspect described by `current_*` and `pending`. Moving
    // to a parent rewrites those in place; while other suspects are queued, the moved
    // suspect goes back through the queue so commits are visited newest first and shared
    // ancestors are reached once.
    'blame_loop: loop {
        if pending.is_empty() || !queue.is_empty() {
            if !pending.is_empty() {
                let lines = walk_lines
                    .take()
                    .and_then(|(oid, lines)| (oid == current_blob_oid).then_some(lines));
                let oid = current_oid;
                queue.push(
                    BlameSuspect {
                        oid,
                        blob: current_blob_oid,
                        mode: current_blob_mode,
                        path: std::mem::take(&mut current_path),
                        lines,
                        pending: std::mem::take(&mut pending),
                    },
                    || blame_suspect_date(session, &oid),
                );
            }
            let Some(next) = queue.pop() else {
                break;
            };
            current_oid = next.oid;
            current_blob_oid = next.blob;
            current_blob_mode = next.mode;
            current_path = next.path;
            walk_lines = next.lines.map(|lines| (next.blob, lines));
            pending = next.pending;
            session.set_bloom_path(&current_path);
        }
        session.prune_blob_lines();

        if !ignore_revs.contains(&current_oid) {
            let (new_oid, new_mode) = advance_across_unchanged_first_parents(
                session,
                odb,
                current_oid,
                current_blob_oid,
                current_blob_mode,
                &current_path,
                grafts,
                &mut commit_cache,
            )?;
            if new_oid != current_oid {
                current_oid = new_oid;
                current_blob_mode = new_mode;
                continue 'blame_loop;
            }
        }

        let commit = get_blame_commit(
            odb,
            current_oid,
            &mut commit_cache,
            session.commit_graph.as_deref(),
        )?;
        let mut parents =
            commit_parents_for_blame(odb, current_oid, grafts, &mut commit_cache, session)?;
        let is_ignored = ignore_revs.contains(&current_oid);

        if first_parent_only && parents.len() > 1 {
            parents.truncate(1);
        }

        // git/blame.c `pass_whole_blame`: run before loading every parent tree for coalesce.
        if merge_aware && !is_ignored && parents.len() >= 2 {
            if let Some(first_parent) = parents.first().copied() {
                if session.commit_graph.is_some()
                    && bloom_may_skip_to_first_parent(
                        session,
                        odb,
                        current_oid,
                        first_parent,
                        &current_path,
                        grafts,
                        &mut commit_cache,
                        true,
                    )?
                {
                    session.commit_path_blob.insert(
                        (first_parent, current_path.clone()),
                        Some((current_blob_oid, current_blob_mode)),
                    );
                    current_oid = fast_forward_same_blob(
                        odb,
                        session,
                        first_parent,
                        current_blob_oid,
                        current_blob_mode,
                        &current_path,
                        grafts,
                        &mut commit_cache,
                    )?;
                    continue 'blame_loop;
                }
            }
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
                            current_blob_mode,
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

        if merge_aware && !ignore_revs.contains(&current_oid) && parents.len() >= 2 {
            if let Some((first_blob, _)) =
                session.resolve_blob_at_commit(odb, parents[0], &current_path, &mut commit_cache)?
            {
                let mut all_same = true;
                for parent_oid in parents.iter().skip(1) {
                    match session.resolve_blob_at_commit(
                        odb,
                        *parent_oid,
                        &current_path,
                        &mut commit_cache,
                    )? {
                        Some((blob, _)) if blob == first_blob => {}
                        _ => {
                            all_same = false;
                            break;
                        }
                    }
                }
                if all_same {
                    parents = vec![parents[0]];
                }
            }
        }

        // If an ignored merge commit is encountered, try to continue blame
        // through the parent that actually contributed each line.
        if is_ignored && parents.len() > 1 {
            let cur_lines_arc =
                session.load_blob_lines(current_blob_oid, &current_path, current_blob_mode)?;
            let cur_lines = BlameSession::line_refs(cur_lines_arc.as_ref());

            let mut parent_lines: Vec<Option<Vec<String>>> = Vec::new();
            let mut parent_blames: Vec<Option<Vec<BlameLine>>> = Vec::new();
            for parent_oid in &parents {
                let parent_commit = get_blame_commit(
                    odb,
                    *parent_oid,
                    &mut commit_cache,
                    session.commit_graph.as_deref(),
                )?;
                if let Some((p_blob_oid, p_blob_mode)) =
                    session.resolve_path_in_tree(parent_commit.tree, &current_path)?
                {
                    let p_lines_arc =
                        session.load_blob_lines(p_blob_oid, &current_path, p_blob_mode)?;
                    let p_lines = p_lines_arc.to_vec();
                    let parent_revision = parent_oid.to_hex();
                    let p_blame = compute_blame_with_session(
                        odb,
                        *parent_oid,
                        &parent_revision,
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
                    result.push(blame_line_at_path(
                        current_oid,
                        t.final_lineno,
                        idx + 1,
                        final_line_display(final_text, t.final_lineno),
                        &current_path,
                        None,
                        t.ignored,
                        true,
                    ));
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
                    result.push(blame_line_from_parent(
                        &pb,
                        t.final_lineno,
                        final_line_display(final_text, t.final_lineno),
                        pb.source_file.clone(),
                        true,
                        pb.unblamable,
                    ));
                } else {
                    result.push(blame_line_at_path(
                        current_oid,
                        t.final_lineno,
                        idx + 1,
                        final_line_display(final_text, t.final_lineno),
                        &current_path,
                        None,
                        t.ignored,
                        true,
                    ));
                }
            }
            continue 'blame_loop;
        }

        if parents.is_empty() {
            // Root commit — attribute all remaining lines
            for t in pending.drain(..) {
                result.push(blame_line_at_path(
                    current_oid,
                    t.final_lineno,
                    t.current_idx + 1,
                    final_line_display(final_text, t.final_lineno),
                    &blamed_path,
                    t.source_path.clone(),
                    t.ignored,
                    false,
                ));
            }
            continue 'blame_loop;
        }

        // Merge commits (2+ parents): sequential `pass_blame_to_parent` order from git/blame.c —
        // each line is attributed to the first parent whose version matches after line mapping.
        // Octopus merges and `.git/info/grafts` with many synthetic parents use the same rule.

        if merge_aware && !is_ignored && parents.len() >= 2 {
            let cur_lines_arc = match walk_lines.take() {
                Some((oid, lines)) if oid == current_blob_oid => lines,
                _ => session.load_blob_lines(current_blob_oid, &current_path, current_blob_mode)?,
            };
            let line_count = cur_lines_arc.len();

            let mut parent_blobs: Vec<Option<(ObjectId, ObjectId, u32, String)>> =
                vec![None; parents.len()];
            let mut par_lines_vec: Vec<Option<Arc<BlobLines>>> = vec![None; parents.len()];
            let mut maps: Vec<Option<Arc<Vec<Option<usize>>>>> = vec![None; parents.len()];

            let mut buckets: Vec<Vec<TrackedLine>> = vec![Vec::new(); parents.len()];
            let mut attributed: Vec<BlameLine> = Vec::new();

            if session.merge_line_stamps.len() < parents.len() {
                session
                    .merge_line_stamps
                    .resize_with(parents.len(), LineUseStamp::default);
            }

            let mut remaining = std::mem::take(&mut pending);
            // Stamps dedupe parent lines within one hand-off; grouping lets each group
            // start a fresh stamp generation.
            remaining.sort_by_key(|t| t.origin);
            let mut any_parent_has_path = false;
            for i in 0..parents.len() {
                if remaining.is_empty() {
                    break;
                }
                let p = parents[i];
                let parent_commit =
                    get_blame_commit(odb, p, &mut commit_cache, session.commit_graph.as_deref())?;
                let mut parent_path = current_path.clone();
                let mut resolved =
                    session.resolve_blob_at_commit(odb, p, &current_path, &mut commit_cache)?;
                if resolved.is_none() {
                    if let Some((renamed_path, renamed_mode)) = find_path_by_oid_in_tree(
                        odb,
                        &parent_commit.tree,
                        &commit.tree,
                        &current_blob_oid,
                        &current_path,
                    )? {
                        parent_path = renamed_path;
                        resolved = session.resolve_blob_at_commit(
                            odb,
                            p,
                            &parent_path,
                            &mut commit_cache,
                        )?;
                        if resolved.is_none() {
                            resolved = Some((current_blob_oid, renamed_mode));
                        }
                    } else if let Some(entry) = find_rename_entry_for_path(
                        odb,
                        &parent_commit.tree,
                        &commit.tree,
                        &current_path,
                        BLAME_RENAME_SIMILARITY,
                    )? {
                        parent_path = entry.old_path.clone().unwrap_or_default();
                        let p_blob_oid = entry.old_oid;
                        let p_blob_mode = entry.old_mode.bits();
                        resolved = Some((p_blob_oid, p_blob_mode));
                    }
                }
                let Some((blob, mode)) = resolved else {
                    continue;
                };
                let renamed_parent = parent_path != current_path;
                any_parent_has_path = true;
                parent_blobs[i] = Some((p, blob, mode, parent_path.clone()));
                if blob == current_blob_oid {
                    par_lines_vec[i] = Some(Arc::clone(&cur_lines_arc));
                    maps[i] = Some(session.identity_line_map(line_count));
                    let mut next_remaining: Vec<TrackedLine> = Vec::new();
                    let mut pass_origin = None;
                    for t in remaining {
                        if pass_origin != Some(t.origin) {
                            pass_origin = Some(t.origin);
                            session.merge_line_stamps[i].begin_pass(line_count);
                        }
                        if t.current_idx < line_count
                            && session.merge_line_stamps[i].is_unused(t.current_idx)
                        {
                            session.merge_line_stamps[i].mark(t.current_idx);
                            let source_path = if renamed_parent {
                                Some(parent_path.clone())
                            } else {
                                t.source_path.clone()
                            };
                            buckets[i].push(TrackedLine {
                                final_lineno: t.final_lineno,
                                current_idx: t.current_idx,
                                ignored: t.ignored,
                                source_path,
                                origin: t.origin,
                            });
                        } else {
                            next_remaining.push(t);
                        }
                    }
                    remaining = next_remaining;
                    continue;
                }
                let pl_arc = session.load_blob_lines(blob, &parent_path, mode)?;
                let map_algo = diff_algorithm;
                let pair = session.parent_child_line_maps(
                    blob,
                    &parent_path,
                    mode,
                    current_blob_oid,
                    &current_path,
                    current_blob_mode,
                    pl_arc.as_ref(),
                    cur_lines_arc.as_ref(),
                    slide_safe_affix(pl_arc.as_ref(), cur_lines_arc.as_ref(), session.whitespace),
                    map_algo,
                    indent_heuristic,
                    false,
                    true,
                );
                par_lines_vec[i] = Some(pl_arc);
                maps[i] = Some(Arc::clone(&pair.diff_line_map));

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
                let mut pass_origin = None;
                for t in remaining {
                    if pass_origin != Some(t.origin) {
                        pass_origin = Some(t.origin);
                        session.merge_line_stamps[i].begin_pass(parent_line_count);
                    }
                    let idx = t.current_idx;
                    let cur_line = cur_lines_arc.comparison_line(idx, session.whitespace);
                    let m = map.get(idx).copied().flatten();
                    if let Some(p_idx) = m {
                        let text_ok = blame_lines_equal(
                            par_lines.comparison_line(p_idx, session.whitespace),
                            cur_line,
                            session.whitespace,
                        );
                        if text_ok
                            && p_idx < parent_line_count
                            && session.merge_line_stamps[i].is_unused(p_idx)
                        {
                            session.merge_line_stamps[i].mark(p_idx);
                            let source_path = if renamed_parent {
                                Some(parent_path.clone())
                            } else {
                                t.source_path.clone()
                            };
                            buckets[i].push(TrackedLine {
                                final_lineno: t.final_lineno,
                                current_idx: p_idx,
                                ignored: t.ignored,
                                source_path,
                                origin: t.origin,
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
                    attributed.push(blame_line_at_path(
                        current_oid,
                        t.final_lineno,
                        idx + 1,
                        final_line_display(final_text, t.final_lineno),
                        &current_path,
                        None,
                        t.ignored,
                        false,
                    ));
                }

                for bl in attributed {
                    result.push(bl);
                }

                for i in 1..parents.len() {
                    if buckets[i].is_empty() {
                        continue;
                    }
                    let Some((p, blob, mode, parent_path)) = parent_blobs[i].clone() else {
                        continue;
                    };
                    next_origin += 1;
                    let mut handed_off = std::mem::take(&mut buckets[i]);
                    for t in &mut handed_off {
                        t.origin = next_origin;
                    }
                    queue.push(
                        BlameSuspect {
                            oid: p,
                            blob,
                            mode,
                            path: parent_path,
                            lines: par_lines_vec[i].take(),
                            pending: handed_off,
                        },
                        || blame_suspect_date(session, &p),
                    );
                }

                if !buckets[0].is_empty() {
                    let Some((p, blob, mode, parent_path)) = parent_blobs[0].clone() else {
                        continue 'blame_loop;
                    };
                    if parent_path != current_path {
                        note_blame_source_path(&mut buckets[0], &parent_path);
                        current_path = parent_path;
                        session.set_bloom_path(&current_path);
                    }
                    current_oid = p;
                    current_blob_oid = blob;
                    current_blob_mode = mode;
                    walk_lines = par_lines_vec[0].take().map(|lines| (blob, lines));
                    pending = std::mem::take(&mut buckets[0]);
                }
                continue;
            }
            pending = remaining;
        }

        let parent_oid = parents[0];
        let parent_commit = get_blame_commit(
            odb,
            parent_oid,
            &mut commit_cache,
            session.commit_graph.as_deref(),
        )?;
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
                note_blame_source_path(&mut pending, &renamed_path);
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
                let p_blob_mode = entry.old_mode.bits();
                if p_blob_oid == current_blob_oid {
                    note_blame_source_path(&mut pending, &old_path);
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
                if copy_depth >= 1 {
                    let cur_lines_arc = session.load_blob_lines(
                        current_blob_oid,
                        &current_path,
                        current_blob_mode,
                    )?;
                    let pending_batch = std::mem::take(&mut pending);
                    let (remaining, _handoff, copy_lines) = copy_detect_attribute_pending(
                        odb,
                        session,
                        pending_batch,
                        &cur_lines_arc,
                        final_text,
                        &blamed_path,
                        parent_oid,
                        &parent_commit.tree,
                        &commit.tree,
                        &current_path,
                        copy_depth,
                        true,
                        None,
                        None,
                        ignore_revs,
                        diff_algorithm,
                        textconv_ctx,
                        use_textconv,
                        first_parent_only,
                        grafts,
                        indent_heuristic,
                    )?;
                    result.extend(copy_lines);
                    for t in remaining {
                        result.push(blame_line_at_path(
                            current_oid,
                            t.final_lineno,
                            t.current_idx + 1,
                            final_line_display(final_text, t.final_lineno),
                            &blamed_path,
                            t.source_path.clone(),
                            t.ignored,
                            false,
                        ));
                    }
                    continue 'blame_loop;
                }

                // No rename/copy source found — attribute to current commit.
                for t in pending.drain(..) {
                    result.push(blame_line_at_path(
                        current_oid,
                        t.final_lineno,
                        t.current_idx + 1,
                        final_line_display(final_text, t.final_lineno),
                        &blamed_path,
                        t.source_path.clone(),
                        t.ignored,
                        false,
                    ));
                }
                continue 'blame_loop;
            }
            None => {
                // Ignored commit but file doesn't exist in parent.
                // Attribute to current anyway (can't go further back).
                for t in pending.drain(..) {
                    result.push(blame_line_at_path(
                        current_oid,
                        t.final_lineno,
                        t.current_idx + 1,
                        final_line_display(final_text, t.final_lineno),
                        &current_path,
                        None,
                        t.ignored,
                        true,
                    ));
                }
                continue 'blame_loop;
            }
            Some((p_blob_oid, p_blob_mode)) if p_blob_oid == current_blob_oid => {
                current_oid = fast_forward_same_blob(
                    odb,
                    session,
                    parent_oid,
                    current_blob_oid,
                    p_blob_mode,
                    &current_path,
                    grafts,
                    &mut commit_cache,
                )?;
                current_blob_mode = p_blob_mode;
                continue;
            }
            Some((p_blob_oid, p_blob_mode)) => {
                // Diff current vs parent
                let cur_lines_arc = match walk_lines.take() {
                    Some((oid, lines)) if oid == current_blob_oid => lines,
                    _ => session.load_blob_lines(
                        current_blob_oid,
                        &current_path,
                        current_blob_mode,
                    )?,
                };
                let par_lines_arc =
                    session.load_blob_lines(p_blob_oid, &parent_blob_path, p_blob_mode)?;
                let pending_batch = std::mem::take(&mut pending);
                let affix = slide_safe_affix(
                    par_lines_arc.as_ref(),
                    cur_lines_arc.as_ref(),
                    session.whitespace,
                );
                let (affix_mapped, need_diff) = partition_pending_by_affix(
                    affix,
                    par_lines_arc.len(),
                    cur_lines_arc.len(),
                    pending_batch,
                );
                // Only use affix partition when it avoids work: affix-only skip, or mixed
                // affix + middle. When every pending line sits in the edited middle, fall back
                // to the pre-round-3 path (full batch through parent_child_line_maps).
                if need_diff.is_empty() {
                    pending = affix_mapped;
                    current_oid = parent_oid;
                    current_blob_oid = p_blob_oid;
                    current_blob_mode = p_blob_mode;
                    walk_lines = Some((p_blob_oid, Arc::clone(&par_lines_arc)));
                    if parent_blob_path != current_path {
                        note_blame_source_path(&mut pending, &parent_blob_path);
                        current_path = parent_blob_path;
                        session.set_bloom_path(&current_path);
                    }
                    continue 'blame_loop;
                }
                let mut still_pending = affix_mapped;
                pending = need_diff;
                let maps = session.parent_child_line_maps(
                    p_blob_oid,
                    &parent_blob_path,
                    p_blob_mode,
                    current_blob_oid,
                    &current_path,
                    current_blob_mode,
                    par_lines_arc.as_ref(),
                    cur_lines_arc.as_ref(),
                    affix,
                    diff_algorithm,
                    indent_heuristic,
                    is_ignored,
                    true,
                );
                let diff_line_map = maps.diff_line_map.as_slice();
                let line_map = maps.line_map.as_slice();
                let stop_blame_on_current = maps.stop_blame_on_current.as_slice();
                let inserted_line_duplicates_parent = pending.iter().any(|t| {
                    t.current_idx < cur_lines_arc.len()
                        && (0..par_lines_arc.len()).any(|i| {
                            blame_lines_equal(
                                cur_lines_arc.comparison_line(t.current_idx, session.whitespace),
                                par_lines_arc.comparison_line(i, session.whitespace),
                                session.whitespace,
                            )
                        })
                });
                let mut introduced_for_copy = Vec::new();

                for t in pending.drain(..) {
                    if t.current_idx < line_map.len() {
                        if diff_line_map.get(t.current_idx).and_then(|m| *m).is_none()
                            && !is_ignored
                        {
                            if copy_depth >= 1 && !inserted_line_duplicates_parent {
                                introduced_for_copy.push(t);
                            } else {
                                result.push(blame_line_at_path(
                                    current_oid,
                                    t.final_lineno,
                                    t.current_idx + 1,
                                    final_line_display(final_text, t.final_lineno),
                                    &current_path,
                                    None,
                                    t.ignored,
                                    false,
                                ));
                            }
                            continue;
                        }
                        if let Some(parent_idx) = line_map[t.current_idx] {
                            // Line came from parent — keep tracking
                            let carried_ignored = if is_ignored {
                                let unchanged = parent_idx == t.current_idx
                                    && par_lines_arc.line(parent_idx)
                                        == cur_lines_arc.line(t.current_idx);
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
                                let duplicates_earlier_parent_line =
                                    (0..par_lines_arc.len()).any(|i| {
                                        i != t.current_idx
                                            && blame_lines_equal(
                                                cur_lines_arc.comparison_line(
                                                    t.current_idx,
                                                    session.whitespace,
                                                ),
                                                par_lines_arc
                                                    .comparison_line(i, session.whitespace),
                                                session.whitespace,
                                            )
                                    });
                                if duplicates_earlier_parent_line {
                                    still_pending.push(TrackedLine {
                                        final_lineno: t.final_lineno,
                                        current_idx: parent_idx,
                                        ignored: t.ignored,
                                        source_path: t.source_path.clone(),
                                        origin: t.origin,
                                    });
                                    continue;
                                }
                                result.push(blame_line_at_path(
                                    current_oid,
                                    t.final_lineno,
                                    t.current_idx + 1,
                                    final_line_display(final_text, t.final_lineno),
                                    &blamed_path,
                                    t.source_path.clone(),
                                    t.ignored,
                                    false,
                                ));
                                continue;
                            }
                            still_pending.push(TrackedLine {
                                final_lineno: t.final_lineno,
                                current_idx: parent_idx,
                                ignored: carried_ignored,
                                source_path: t.source_path.clone(),
                                origin: t.origin,
                            });
                        } else if is_ignored {
                            // Best-effort pass-through through ignored revisions:
                            // only keep walking when the same-slot parent line
                            // is text-identical; otherwise keep blame on the
                            // ignored commit and mark as unblamable.
                            if t.current_idx < par_lines_arc.len()
                                && cur_lines_arc.line(t.current_idx)
                                    == par_lines_arc.line(t.current_idx)
                            {
                                still_pending.push(TrackedLine {
                                    final_lineno: t.final_lineno,
                                    current_idx: t.current_idx,
                                    ignored: t.ignored,
                                    source_path: t.source_path.clone(),
                                    origin: t.origin,
                                });
                            } else {
                                result.push(blame_line_at_path(
                                    current_oid,
                                    t.final_lineno,
                                    t.current_idx + 1,
                                    final_line_display(final_text, t.final_lineno),
                                    &current_path,
                                    None,
                                    t.ignored,
                                    true,
                                ));
                            }
                        } else {
                            let line_text =
                                cur_lines_arc.comparison_line(t.current_idx, session.whitespace);
                            if let Some(parent_idx) = (0..par_lines_arc.len()).find(|&i| {
                                blame_lines_equal(
                                    par_lines_arc.comparison_line(i, session.whitespace),
                                    line_text,
                                    session.whitespace,
                                )
                            }) {
                                still_pending.push(TrackedLine {
                                    final_lineno: t.final_lineno,
                                    current_idx: parent_idx,
                                    ignored: t.ignored,
                                    source_path: t.source_path.clone(),
                                    origin: t.origin,
                                });
                                continue;
                            }
                            if copy_depth >= 1 && !inserted_line_duplicates_parent {
                                introduced_for_copy.push(t);
                            } else {
                                result.push(blame_line_at_path(
                                    current_oid,
                                    t.final_lineno,
                                    t.current_idx + 1,
                                    final_line_display(final_text, t.final_lineno),
                                    &blamed_path,
                                    t.source_path.clone(),
                                    t.ignored,
                                    false,
                                ));
                            }
                        }
                    } else if is_ignored {
                        result.push(blame_line_at_path(
                            current_oid,
                            t.final_lineno,
                            t.current_idx + 1,
                            final_line_display(final_text, t.final_lineno),
                            &current_path,
                            None,
                            t.ignored,
                            true,
                        ));
                    } else {
                        // Out of range — attribute to current
                        result.push(blame_line_at_path(
                            current_oid,
                            t.final_lineno,
                            t.current_idx + 1,
                            final_line_display(final_text, t.final_lineno),
                            &current_path,
                            None,
                            t.ignored,
                            false,
                        ));
                    }
                }

                if copy_depth >= 1 && !introduced_for_copy.is_empty() {
                    let (left, handoff, copy_lines) = copy_detect_attribute_pending(
                        odb,
                        session,
                        introduced_for_copy,
                        &cur_lines_arc,
                        final_text,
                        &blamed_path,
                        parent_oid,
                        &parent_commit.tree,
                        &commit.tree,
                        &current_path,
                        copy_depth,
                        false,
                        None,
                        Some(&par_lines_arc),
                        ignore_revs,
                        diff_algorithm,
                        textconv_ctx,
                        use_textconv,
                        first_parent_only,
                        grafts,
                        indent_heuristic,
                    )?;
                    result.extend(copy_lines);
                    for t in left {
                        result.push(blame_line_at_path(
                            current_oid,
                            t.final_lineno,
                            t.current_idx + 1,
                            final_line_display(final_text, t.final_lineno),
                            &blamed_path,
                            t.source_path.clone(),
                            t.ignored,
                            false,
                        ));
                    }
                    still_pending.extend(handoff);
                }

                pending = still_pending;
                current_oid = parent_oid;
                current_blob_oid = p_blob_oid;
                current_blob_mode = p_blob_mode;
                walk_lines = Some((p_blob_oid, Arc::clone(&par_lines_arc)));
                if parent_blob_path != current_path {
                    note_blame_source_path(&mut pending, &parent_blob_path);
                    current_path = parent_blob_path;
                    session.set_bloom_path(&current_path);
                }
            }
        }
    }

    result.sort_by_key(|b| b.final_lineno);
    Ok(result)
}

/// Git's default minimum line score for copy detection (`blame.h`).
const BLAME_DEFAULT_COPY_SCORE: usize = 40;
/// Git's default minimum chunk score for within-file move detection (`blame.h`).
const BLAME_DEFAULT_MOVE_SCORE: usize = 20;

fn blame_copy_content_score(line: &str) -> usize {
    line.chars().filter(|c| c.is_ascii_alphanumeric()).count()
}

/// Git `blame_entry_score`: one plus alphanumeric bytes across all lines in the chunk.
fn blame_entry_score_lines(lines: &[&str]) -> usize {
    1 + lines
        .iter()
        .map(|line| blame_copy_content_score(line))
        .sum::<usize>()
}

/// Longest contiguous equal run between a suspect chunk and a candidate file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContiguousCopyMatch {
    suspect_start: usize,
    parent_start: usize,
    len: usize,
    score: usize,
}

fn find_best_contiguous_copy_match(
    suspect: &[&str],
    parent: &[&str],
    ws: crate::diff::WhitespaceMode,
) -> Option<ContiguousCopyMatch> {
    let mut best: Option<ContiguousCopyMatch> = None;
    for s_start in 0..suspect.len() {
        for p_start in 0..parent.len() {
            let mut len = 0usize;
            while s_start + len < suspect.len()
                && p_start + len < parent.len()
                && blame_lines_equal(suspect[s_start + len], parent[p_start + len], ws)
            {
                len += 1;
            }
            if len == 0 {
                continue;
            }
            let score = blame_entry_score_lines(&suspect[s_start..s_start + len]);
            let replace = best.is_none_or(|b| score > b.score || (score == b.score && len > b.len));
            if replace {
                best = Some(ContiguousCopyMatch {
                    suspect_start: s_start,
                    parent_start: p_start,
                    len,
                    score,
                });
            }
        }
    }
    best
}

/// Contiguous `current_idx` groups for copy/move suspect chunks.
fn group_contiguous_tracked(pending: &[TrackedLine]) -> Vec<(usize, usize)> {
    if pending.is_empty() {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..pending.len()).collect();
    order.sort_by_key(|&i| pending[i].current_idx);
    let mut groups = Vec::new();
    let mut start = pending[order[0]].current_idx;
    let mut end = start + 1;
    for &i in order.iter().skip(1) {
        let idx = pending[i].current_idx;
        if idx == end {
            end += 1;
        } else {
            groups.push((start, end));
            start = idx;
            end = idx + 1;
        }
    }
    groups.push((start, end));
    groups
}

type CopySourceCache = HashMap<String, (Arc<BlobLines>, Vec<BlameLine>)>;

#[expect(clippy::too_many_arguments)]
fn load_copy_source_blame(
    odb: &Odb,
    parent_oid: ObjectId,
    path: &str,
    oid: ObjectId,
    mode: u32,
    ignore_revs: &HashSet<ObjectId>,
    diff_algorithm: BlameDiffAlgorithm,
    textconv_ctx: Option<&BlameTextconvContext>,
    use_textconv: bool,
    copy_depth: usize,
    first_parent_only: bool,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    indent_heuristic: bool,
    session: &mut BlameSession<'_>,
    cache: &mut CopySourceCache,
) -> Result<(Arc<BlobLines>, Vec<BlameLine>)> {
    if let Some(entry) = cache.get(path) {
        return Ok((Arc::clone(&entry.0), entry.1.clone()));
    }
    let source_lines = session.load_blob_lines(oid, path, mode)?;
    let parent_revision = parent_oid.to_hex();
    let source_blame = compute_blame_with_session(
        odb,
        parent_oid,
        &parent_revision,
        path,
        ignore_revs,
        diff_algorithm,
        textconv_ctx,
        use_textconv,
        copy_depth,
        first_parent_only,
        grafts,
        indent_heuristic,
        None,
        None,
        session,
    )?;
    cache.insert(
        path.to_owned(),
        (Arc::clone(&source_lines), source_blame.clone()),
    );
    Ok((source_lines, source_blame))
}

/// Apply Git-style copy/move detection to still-unblamed lines.
#[expect(clippy::too_many_arguments)]
fn copy_detect_attribute_pending(
    odb: &Odb,
    session: &mut BlameSession<'_>,
    pending: Vec<TrackedLine>,
    cur_lines: &Arc<BlobLines>,
    final_text: &BlobLines,
    _blamed_path: &str,
    parent_oid: ObjectId,
    parent_tree: &ObjectId,
    child_tree: &ObjectId,
    current_path: &str,
    copy_depth: usize,
    blaming_new_file: bool,
    // When set, skip tree-based copy path rules and use this allow-list (`None` = all paths).
    explicit_allowed_paths: Option<Option<&HashSet<String>>>,
    move_parent_lines: Option<&Arc<BlobLines>>,
    ignore_revs: &HashSet<ObjectId>,
    diff_algorithm: BlameDiffAlgorithm,
    textconv_ctx: Option<&BlameTextconvContext>,
    use_textconv: bool,
    first_parent_only: bool,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    indent_heuristic: bool,
) -> Result<(Vec<TrackedLine>, Vec<TrackedLine>, Vec<BlameLine>)> {
    if copy_depth == 0 || pending.is_empty() {
        return Ok((pending, Vec::new(), Vec::new()));
    }
    let ws = session.whitespace;
    let cur_refs = cur_lines.comparison_refs(ws);
    let allowed = match explicit_allowed_paths {
        Some(paths) => paths.cloned(),
        None => {
            copy_allowed_parent_paths(odb, parent_tree, child_tree, copy_depth, blaming_new_file)?
        }
    };
    let mut entries = Vec::new();
    collect_tree_file_entries(odb, parent_tree, "", &mut entries)?;
    let mut candidates: Vec<(String, ObjectId, u32)> = Vec::new();
    for (path, oid, mode) in entries {
        if path == current_path || !is_regular_mode(mode) {
            continue;
        }
        if !path_allowed_for_copy_search(&path, allowed.as_ref()) {
            continue;
        }
        candidates.push((path, oid, mode));
    }
    let mut source_cache: CopySourceCache = HashMap::new();
    let mut result = Vec::new();
    let mut parent_handoff = Vec::new();
    let mut work = pending;
    let copy_threshold = BLAME_DEFAULT_COPY_SCORE;
    let move_threshold = BLAME_DEFAULT_MOVE_SCORE;

    if let Some(par_lines) = move_parent_lines {
        let par_refs = par_lines.comparison_refs(ws);
        let mut next_work = Vec::new();
        for group in group_contiguous_tracked(&work) {
            let slice: Vec<&str> = (group.0..group.1).map(|i| cur_refs[i]).collect();
            if blame_entry_score_lines(&slice) <= move_threshold {
                next_work.extend(
                    work.iter()
                        .filter(|t| t.current_idx >= group.0 && t.current_idx < group.1)
                        .cloned(),
                );
                continue;
            }
            let Some(m) = find_best_contiguous_copy_match(&slice, &par_refs, ws) else {
                next_work.extend(
                    work.iter()
                        .filter(|t| t.current_idx >= group.0 && t.current_idx < group.1)
                        .cloned(),
                );
                continue;
            };
            if m.score <= move_threshold {
                next_work.extend(
                    work.iter()
                        .filter(|t| t.current_idx >= group.0 && t.current_idx < group.1)
                        .cloned(),
                );
                continue;
            }
            for t in work
                .iter()
                .filter(|t| t.current_idx >= group.0 && t.current_idx < group.1)
            {
                let off = t.current_idx - group.0;
                if off < m.suspect_start || off >= m.suspect_start + m.len {
                    next_work.push(t.clone());
                    continue;
                }
                let parent_line = m.parent_start + (off - m.suspect_start);
                parent_handoff.push(TrackedLine {
                    current_idx: parent_line,
                    ..t.clone()
                });
            }
        }
        work = next_work;
    }

    loop {
        let mut made_progress = false;
        let groups = group_contiguous_tracked(&work);
        let mut still_pending = Vec::new();
        for (start_idx, end_idx) in groups {
            let slice: Vec<&str> = (start_idx..end_idx).map(|i| cur_refs[i]).collect();
            let group_lines: Vec<TrackedLine> = work
                .iter()
                .filter(|t| t.current_idx >= start_idx && t.current_idx < end_idx)
                .cloned()
                .collect();
            let mut subranges = vec![(0usize, slice.len())];
            let mut group_pending = Vec::new();
            while let Some((off, len)) = subranges.pop() {
                if len == 0 {
                    continue;
                }
                let sub = &slice[off..off + len];
                if blame_entry_score_lines(sub) <= copy_threshold {
                    for t in &group_lines {
                        let rel = t.current_idx - start_idx;
                        if rel >= off && rel < off + len {
                            group_pending.push(t.clone());
                        }
                    }
                    continue;
                }
                let mut best: Option<(ContiguousCopyMatch, String, ObjectId, u32)> = None;
                for (path, oid, mode) in &candidates {
                    let source_lines = session.load_blob_lines(*oid, path, *mode)?;
                    let src_refs = source_lines.comparison_refs(ws);
                    let Some(m) = find_best_contiguous_copy_match(sub, &src_refs, ws) else {
                        continue;
                    };
                    let replace = best.as_ref().is_none_or(|(b, _, _, _)| {
                        m.score > b.score || (m.score == b.score && m.len > b.len)
                    });
                    if replace {
                        best = Some((m, path.clone(), *oid, *mode));
                    }
                }
                let Some((m, source_path, source_oid, source_mode)) = best else {
                    for t in &group_lines {
                        let rel = t.current_idx - start_idx;
                        if rel >= off && rel < off + len {
                            group_pending.push(t.clone());
                        }
                    }
                    continue;
                };
                if m.score <= copy_threshold {
                    for t in &group_lines {
                        let rel = t.current_idx - start_idx;
                        if rel >= off && rel < off + len {
                            group_pending.push(t.clone());
                        }
                    }
                    continue;
                }
                let (_source_lines, source_blame) = load_copy_source_blame(
                    odb,
                    parent_oid,
                    &source_path,
                    source_oid,
                    source_mode,
                    ignore_revs,
                    diff_algorithm,
                    textconv_ctx,
                    use_textconv,
                    copy_depth.saturating_sub(1),
                    first_parent_only,
                    grafts,
                    indent_heuristic,
                    session,
                    &mut source_cache,
                )?;
                made_progress = true;
                if m.suspect_start > 0 {
                    subranges.push((off, m.suspect_start));
                }
                if m.suspect_start + m.len < len {
                    subranges.push((off + m.suspect_start + m.len, len - m.suspect_start - m.len));
                }
                for t in &group_lines {
                    let rel = t.current_idx - start_idx;
                    if rel < off + m.suspect_start || rel >= off + m.suspect_start + m.len {
                        continue;
                    }
                    let parent_line = m.parent_start + (rel - off - m.suspect_start);
                    let pb = source_blame
                        .iter()
                        .find(|l| l.final_lineno == parent_line + 1)
                        .cloned()
                        .unwrap_or_else(|| {
                            blame_line_at_path(
                                parent_oid,
                                parent_line + 1,
                                parent_line + 1,
                                final_line_display(final_text, t.final_lineno),
                                &source_path,
                                None,
                                false,
                                false,
                            )
                        });
                    result.push(blame_line_from_parent(
                        &pb,
                        t.final_lineno,
                        final_line_display(final_text, t.final_lineno),
                        pb.source_file.clone().or_else(|| Some(source_path.clone())),
                        pb.ignored || t.ignored,
                        pb.unblamable,
                    ));
                }
            }
            still_pending.extend(group_pending);
        }
        work = still_pending;
        if !made_progress {
            break;
        }
    }
    Ok((work, parent_handoff, result))
}

fn copy_changed_parent_paths(
    odb: &Odb,
    parent_tree: &ObjectId,
    child_tree: &ObjectId,
) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for entry in diff_trees(odb, Some(parent_tree), Some(child_tree), "")? {
        if let Some(path) = entry.old_path {
            out.insert(path);
        }
        if let Some(path) = entry.new_path {
            out.insert(path);
        }
    }
    Ok(out)
}

fn copy_allowed_parent_paths(
    odb: &Odb,
    parent_tree: &ObjectId,
    child_tree: &ObjectId,
    copy_depth: usize,
    blaming_new_file: bool,
) -> Result<Option<HashSet<String>>> {
    if copy_depth >= 3 {
        return Ok(None);
    }
    if copy_depth >= 2 && blaming_new_file {
        return Ok(None);
    }
    Ok(Some(copy_changed_parent_paths(
        odb,
        parent_tree,
        child_tree,
    )?))
}

fn path_allowed_for_copy_search(path: &str, allowed: Option<&HashSet<String>>) -> bool {
    allowed.is_none_or(|set| set.contains(path))
}

fn note_blame_source_path(pending: &mut [TrackedLine], path: &str) {
    let path = path.to_owned();
    for t in pending.iter_mut() {
        t.source_path = Some(path.clone());
    }
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
        let path = format_path_stored(prefix, &entry.name);
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
    cache: &mut BlameCommitCache,
    graph: Option<&CommitGraphChain>,
) -> Result<Arc<BlameCommitMeta>> {
    if let Some(c) = cache.metas.get(&oid) {
        return Ok(Arc::clone(c));
    }
    let meta = if let Some(chain) = graph {
        if let Some((tree, parents)) = chain.graph_commit_meta(&oid) {
            Arc::new(BlameCommitMeta { tree, parents })
        } else {
            load_blame_commit_from_object(odb, oid, &mut cache.pack)?
        }
    } else {
        load_blame_commit_from_object(odb, oid, &mut cache.pack)?
    };
    cache.metas.insert(oid, Arc::clone(&meta));
    Ok(meta)
}

/// Read only the tree and parent headers; author and summary are loaded later, and
/// only for the commits that end up owning lines.
fn load_blame_commit_from_object(
    odb: &Odb,
    oid: ObjectId,
    pack: &mut SessionPackReader,
) -> Result<Arc<BlameCommitMeta>> {
    let (tree, parents) = pack.commit_tree_and_parents(odb, &oid)?;
    Ok(Arc::new(BlameCommitMeta { tree, parents }))
}

/// Skip first-parent history while Bloom filters or an identical blob show `path` unchanged.
#[allow(clippy::too_many_arguments)]
fn advance_across_unchanged_first_parents(
    session: &mut BlameSession<'_>,
    odb: &Odb,
    mut current_oid: ObjectId,
    blob_oid: ObjectId,
    mut blob_mode: u32,
    path: &str,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    commit_cache: &mut BlameCommitCache,
) -> Result<(ObjectId, u32)> {
    session.set_bloom_path(path);
    let chain = session.commit_graph.clone();
    loop {
        if grafts.contains_key(&current_oid) {
            break;
        }
        let parent_oid = if let Some(ref chain) = chain {
            if let Some((parents, _)) = chain.graph_commit(&current_oid) {
                parents.first().copied()
            } else {
                commit_parents_for_blame(odb, current_oid, grafts, commit_cache, session)?
                    .first()
                    .copied()
            }
        } else {
            commit_parents_for_blame(odb, current_oid, grafts, commit_cache, session)?
                .first()
                .copied()
        };
        let Some(parent_oid) = parent_oid else {
            break;
        };
        if chain.is_some()
            && bloom_may_skip_to_first_parent(
                session,
                odb,
                current_oid,
                parent_oid,
                path,
                grafts,
                commit_cache,
                true,
            )?
        {
            session
                .commit_path_blob
                .insert((parent_oid, path.to_owned()), Some((blob_oid, blob_mode)));
            current_oid = parent_oid;
            continue;
        }
        if let Some((p_blob, p_mode)) =
            session.resolve_blob_at_commit(odb, parent_oid, path, commit_cache)?
        {
            if p_blob == blob_oid {
                current_oid = fast_forward_same_blob(
                    odb,
                    session,
                    parent_oid,
                    blob_oid,
                    p_mode,
                    path,
                    grafts,
                    commit_cache,
                )?;
                blob_mode = p_mode;
                continue;
            }
        }
        break;
    }
    Ok((current_oid, blob_mode))
}

/// Commit-graph Bloom filters describe path changes vs the commit object's first parent.
/// Skip the shortcut when grafts replace parents or the walk target is not that parent.
#[allow(clippy::too_many_arguments)]
fn bloom_may_skip_to_first_parent(
    session: &mut BlameSession<'_>,
    odb: &Odb,
    current_oid: ObjectId,
    parent_oid: ObjectId,
    path: &str,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    commit_cache: &mut BlameCommitCache,
    parent_is_first_in_graph: bool,
) -> Result<bool> {
    if grafts.contains_key(&current_oid) {
        return Ok(false);
    }
    if !parent_is_first_in_graph {
        let object_parents = commit_object_parents(session, odb, current_oid, commit_cache)?;
        if object_parents.first() != Some(&parent_oid) {
            return Ok(false);
        }
    }
    let Some(chain) = session.commit_graph.clone() else {
        return Ok(false);
    };
    session.set_bloom_path(path);
    Ok(bloom_precheck_definitely_not(
        session,
        &chain,
        current_oid,
        path,
    ))
}

fn bloom_precheck_definitely_not(
    session: &mut BlameSession<'_>,
    chain: &CommitGraphChain,
    oid: ObjectId,
    path: &str,
) -> bool {
    let Some(settings) = chain.bloom_settings_for_commit(&oid) else {
        return false;
    };
    let keys = session.bloom_keys_for_path(path, &settings);
    chain.bloom_definitely_not_for_keys(oid, keys.as_slice())
}

/// Parent OIDs stored in the commit object (or commit-graph), ignoring grafts.
fn commit_object_parents(
    session: &BlameSession<'_>,
    odb: &Odb,
    oid: ObjectId,
    cache: &mut BlameCommitCache,
) -> Result<Vec<ObjectId>> {
    if let Some(chain) = session.commit_graph.as_ref() {
        if let Some((parents, _commit_time)) = chain.graph_commit(&oid) {
            return Ok(parents);
        }
    }
    Ok(
        get_blame_commit(odb, oid, cache, session.commit_graph.as_deref())?
            .parents
            .clone(),
    )
}

/// Parent list for a commit, honoring `.git/info/grafts` (same rules as `git rev-list`).
fn commit_parents_for_blame(
    odb: &Odb,
    oid: ObjectId,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    cache: &mut BlameCommitCache,
    session: &BlameSession<'_>,
) -> Result<Vec<ObjectId>> {
    if let Some(p) = grafts.get(&oid) {
        return Ok(p.clone());
    }
    if let Some(chain) = session.commit_graph.as_ref() {
        if let Some((parents, _)) = chain.graph_commit(&oid) {
            return Ok(parents);
        }
    }
    Ok(
        get_blame_commit(odb, oid, cache, session.commit_graph.as_deref())?
            .parents
            .clone(),
    )
}

/// Resolve a Git-style `-L` range against the blob at `start` for `path`.
///
/// Uses the same line splitting, funcname rules, and reversible stored-path input
/// contract as [`blame_file`]. Equivalent to [`resolve_blame_line_range_with_options`]
/// with `start` and the committed file.
///
/// # Errors
///
/// Returns [`LineRangeError`] when the spec is invalid or out of range for the file at `start`.
pub fn resolve_blame_line_range(
    repo: &crate::repo::Repository,
    start: crate::objects::ObjectId,
    path: &str,
    spec: &str,
) -> std::result::Result<RangeInclusive<usize>, LineRangeError> {
    resolve_blame_line_range_with_options(
        repo,
        path,
        spec,
        &BlameOptions {
            start: Some(start),
            ..BlameOptions::default()
        },
    )
}

/// Resolve a Git-style `-L` range against the text [`blame_file`] would blame with `options`.
///
/// Only `options.start` and `options.final_source` matter: regex, funcname and
/// relative ranges are matched against the worktree file or the supplied contents when
/// those are selected, so the range lines up with the returned final line numbers.
///
/// # Errors
///
/// Returns [`LineRangeError`] when the spec is invalid or out of range, or when the
/// final text cannot be loaded (reported as [`LineRangeError::InvalidSpec`]).
pub fn resolve_blame_line_range_with_options(
    repo: &crate::repo::Repository,
    path: &str,
    spec: &str,
    options: &BlameOptions,
) -> std::result::Result<RangeInclusive<usize>, LineRangeError> {
    use crate::attributes::load_gitattributes_for_check_attr_source;
    use crate::diff::merge_diff::read_blob_at_path;
    use crate::userdiff::matcher_for_path_parsed;

    let invalid = |detail: String| LineRangeError::InvalidSpec {
        spec: spec.to_owned(),
        detail,
    };
    let file_path = normalize_blame_path(repo, path);
    let start = resolve_blame_start(repo, options.start).map_err(|e| invalid(e.to_string()))?;
    let commit = get_blame_commit(repo.odb(), start, &mut BlameCommitCache::default(), None)
        .map_err(|e| invalid(e.to_string()))?;
    let overlay = load_blame_overlay(
        repo,
        start,
        &blame_revision_label(options.start, start),
        &file_path,
        &options.final_source,
        CopyDetection::Off,
    )
    .map_err(|e| invalid(e.to_string()))?;
    let content: std::borrow::Cow<'_, str> = match &overlay {
        Some(overlay) => overlay
            .lines
            .utf8()
            .map(std::borrow::Cow::Borrowed)
            .unwrap_or_else(|| {
                String::from_utf8_lossy(overlay.lines.bytes())
                    .into_owned()
                    .into()
            }),
        None => {
            let Some(content_bytes) = read_blob_at_path(repo.odb(), &commit.tree, &file_path)
            else {
                return Err(invalid(format!("file '{file_path}' not found in revision")));
            };
            String::from_utf8_lossy(&content_bytes).into_owned().into()
        }
    };
    let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
    let parsed = load_gitattributes_for_check_attr_source(repo, &commit.tree)
        .map_err(|e| invalid(e.to_string()))?;
    let matcher =
        matcher_for_path_parsed(&config, &parsed.rules, &parsed.macros, &file_path, false)
            .map_err(invalid)?;
    resolve_line_range(spec, &content, &file_path, &config, matcher.as_ref(), 1)
}

/// Copy detection depth for blame, matching Git's repeated `-C` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyDetection {
    /// No copy detection (default).
    #[default]
    Off,
    /// `-C` once: copies from files removed in the same commit.
    FromModifiedFiles,
    /// `-C -C`: also search existing parent files when the blamed path is new in the commit.
    FromCreatingCommit,
    /// `-C -C -C`: search all parent files for copies (including appended lines).
    FromAnyCommit,
}

impl CopyDetection {
    /// Number of `-C` flags this level represents (`0` when [`CopyDetection::Off`]).
    #[must_use]
    pub const fn count(self) -> usize {
        match self {
            Self::Off => 0,
            Self::FromModifiedFiles => 1,
            Self::FromCreatingCommit => 2,
            Self::FromAnyCommit => 3,
        }
    }

    /// Build from a CLI `-C` repetition count (Git allows at most three).
    ///
    /// # Errors
    ///
    /// Returns [`LibError::Message`] when `count` is greater than three.
    pub fn from_count(count: usize) -> Result<Self> {
        match count {
            0 => Ok(Self::Off),
            1 => Ok(Self::FromModifiedFiles),
            2 => Ok(Self::FromCreatingCommit),
            3 => Ok(Self::FromAnyCommit),
            _ => Err(LibError::Message(
                "blame copy detection: at most three -C flags are supported".into(),
            )),
        }
    }

    const fn depth(self) -> usize {
        self.count()
    }
}

/// Options for [`blame_file`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlameOptions {
    /// Commit to blame at. When `None`, uses the current `HEAD` commit.
    pub start: Option<ObjectId>,
    /// When set, only include lines whose 1-based final line number lies in this range.
    /// Packed history uses positioned entry reads, bounded retained pack readers,
    /// and private, bounded delta-base caches;
    /// repository caches and their configured limits are left unchanged. Complete blobs
    /// and tree delta chains still need to be reconstructed to attribute these lines.
    pub line_range: Option<RangeInclusive<usize>>,
    /// When true, follow only the first parent at merge commits (Git's `--first-parent`).
    pub first_parent_only: bool,
    /// Commits to treat as transparent during the history walk.
    pub ignore: IgnoreRevs,
    /// Whitespace rules when comparing lines between revisions (`-w` / ignore-all-space today).
    pub whitespace: crate::diff::WhitespaceMode,
    /// Copy detection depth (Git `-C`, `-C -C`, `-C -C -C`).
    pub copy_detection: CopyDetection,
    /// Which version of the file supplies the blamed lines. The default blames the file as
    /// committed at `start`.
    pub final_source: BlameFinalSource,
    /// Author time reported in [`BlameResult::commits`] for uncommitted lines (Git reports
    /// the current time). The library never reads the clock; `None` reports `0`.
    pub now: Option<time::OffsetDateTime>,
}

/// Which version of the blamed file [`blame_file`] attributes, line by line.
///
/// For [`Worktree`](Self::Worktree) and [`Contents`](Self::Contents) the lines are blamed
/// as if they were a commit on top of the start revision (plus any `MERGE_HEAD` parents of
/// an unfinished merge, unless [`BlameOptions::first_parent_only`] is set): lines that
/// match a parent keep their history, and the rest are
/// reported with [`BlameAttributedLine::not_committed`] set and the null object id. Content
/// that matches the start revision exactly is blamed like
/// [`Revision`](Self::Revision). Nothing is written to the object database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BlameFinalSource {
    /// The file as committed at the start revision (Git's `blame <rev> -- <path>`).
    #[default]
    Revision,
    /// The file in the working tree, cleaned like `add` would (line endings, clean
    /// filters, symlink targets). This is what `git blame <path>` shows.
    Worktree,
    /// Caller-supplied file content, such as an editor buffer with unsaved edits
    /// (Git's `blame --contents`). The bytes get the same clean conversion as a worktree file.
    Contents(Vec<u8>),
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
    /// Reversibly stored path in the originating commit, including rename/copy origins.
    /// Resolve this path in the attributed commit, not in the blamed revision.
    /// Recover exact filename bytes with `repo_relpath::storage_bytes`.
    pub original_path: String,
    /// Line text in the blamed revision (lossy UTF-8 when [`Self::encoding_lossy`]).
    pub content: String,
    /// `true` when line bytes are not valid UTF-8 and [`Self::content`] used lossy decoding.
    pub encoding_lossy: bool,
    /// Exact line bytes excluding the line terminator when [`Self::encoding_lossy`]; otherwise `None`.
    pub content_bytes: Option<Vec<u8>>,
    /// Line attribution passed through an ignored revision.
    pub ignored: bool,
    /// Line could not be blamed past an ignored revision.
    pub unblamable: bool,
    /// Line exists only in the uncommitted content selected by
    /// [`BlameOptions::final_source`]. `commit` is then the null object id and
    /// `original_line` equals `final_line`.
    pub not_committed: bool,
}

/// A contiguous run of blamed lines sharing commit, path, and original line sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameHunk {
    /// Commit that last modified every line in this hunk.
    pub commit: ObjectId,
    /// Reversibly stored path in the originating commit.
    /// Resolve this path in `commit`; source lines begin at `original_start`.
    /// Recover exact filename bytes with `repo_relpath::storage_bytes`.
    pub original_path: String,
    /// 1-based starting line in the originating commit.
    pub original_start: usize,
    /// 1-based starting line in the blamed revision's file.
    pub final_start: usize,
    /// Number of consecutive final lines in this hunk.
    pub count: usize,
    /// Set when every line in the hunk passed through an ignored revision.
    pub ignored: bool,
    /// Set when every line in the hunk could not be blamed past an ignored revision.
    pub unblamable: bool,
}

/// Metadata for a commit referenced by [`BlameResult::lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommitInfo {
    /// Author identity line (`Name <email> <epoch> <tz>`); empty when the commit has no
    /// `author` header.
    pub author: String,
    /// Author timestamp (Unix seconds); `0` when the author line has none.
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

impl BlameResult {
    /// Group consecutive lines into hunks matching `git blame --porcelain` group headers.
    #[must_use]
    pub fn hunks(&self) -> Vec<BlameHunk> {
        if self.lines.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut start = 0usize;
        for i in 1..=self.lines.len() {
            let extend =
                i < self.lines.len() && blame_lines_share_hunk(&self.lines[i - 1], &self.lines[i]);
            if !extend {
                let slice = &self.lines[start..i];
                out.push(BlameHunk {
                    commit: slice[0].commit,
                    original_path: slice[0].original_path.clone(),
                    original_start: slice[0].original_line,
                    final_start: slice[0].final_line,
                    count: slice.len(),
                    ignored: slice[0].ignored,
                    unblamable: slice[0].unblamable,
                });
                start = i;
            }
        }
        out
    }
}

fn blame_lines_share_hunk(prev: &BlameAttributedLine, next: &BlameAttributedLine) -> bool {
    prev.commit == next.commit
        && prev.original_path == next.original_path
        && prev.ignored == next.ignored
        && prev.unblamable == next.unblamable
        && next.original_line == prev.original_line + 1
        && next.final_line == prev.final_line + 1
}

/// Blame `path` at `options.start` (or `HEAD`) and return per-line attributions.
///
/// `path` accepts the reversible strings returned by status or
/// `repo_relpath::storage_string`. Encoded paths are repository-relative;
/// ordinary UTF-8 paths retain work-tree cwd resolution. Decoding happens once,
/// keeping literal reserved-prefix names distinct. Returned `original_path` values
/// use the same storage encoding. Ordinary operands such as `./grit+pathhex:6162`
/// are normalized before storage escaping, so they name the literal file.
///
/// Adding or removing the final LF changes attribution of the last line by default.
/// Whitespace normalization (for example [`crate::diff::WhitespaceMode::ignore_all_space`])
/// compares normalized bodies instead, so newline-only changes remain transparent.
/// Returned line content excludes LF, regardless of the comparison mode.
///
/// [`BlameOptions::final_source`] selects uncommitted content (the worktree file or a
/// caller buffer) to blame on top of the start revision; see [`BlameFinalSource`].
///
/// # Errors
///
/// Returns an error when `HEAD` is unborn, the revision or path cannot be resolved,
/// or object data cannot be read. With uncommitted content it also fails when the
/// worktree file cannot be read or cleaned, when the path is in no parent and not in the
/// index ([`crate::rev_parse_error::RevParseError::PathNotInRevision`]), and for
/// [`BlameFinalSource::Worktree`] in a bare repository
/// ([`LibError::BlameWorktreeBareRepository`]).
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
    let indent_heuristic = crate::diff::indent_heuristic_from_config(&config);
    let ignore_revs: HashSet<ObjectId> = options.ignore.as_set().clone();

    let bounded_odb = options
        .line_range
        .as_ref()
        .map(|_| repo.odb().with_bounded_pack_cache());
    let odb = bounded_odb.as_ref().unwrap_or(repo.odb());
    let thread_policy = repo.thread_policy();
    let mut session = BlameSession::new(odb, None, false, &thread_policy);
    session.whitespace = options.whitespace;
    session.commit_graph = repo.odb().commit_graph_chain();
    let revision_label = blame_revision_label(options.start, start_oid);
    let overlay = load_blame_overlay(
        repo,
        start_oid,
        &revision_label,
        &file_path,
        &options.final_source,
        options.copy_detection,
    )?;
    let copy_depth = options.copy_detection.depth();
    session.cache_pair_maps = !ignore_revs.is_empty() || copy_depth > 0;
    let (lines, commits) = odb.with_pack_read_context(
        || -> Result<(Vec<BlameAttributedLine>, HashMap<ObjectId, BlameCommitInfo>)> {
            let raw = compute_blame_with_session(
                odb,
                start_oid,
                &revision_label,
                &file_path,
                &ignore_revs,
                diff_algorithm,
                None,
                false,
                copy_depth,
                options.first_parent_only,
                &grafts,
                indent_heuristic,
                options.line_range.clone(),
                overlay.as_ref(),
                &mut session,
            )?;

            let lines: Vec<BlameAttributedLine> = raw
                .into_iter()
                .map(|line| BlameAttributedLine {
                    commit: line.oid,
                    original_line: line.orig_lineno,
                    final_line: line.final_lineno,
                    original_path: line.original_path,
                    content: line.content,
                    encoding_lossy: line.encoding_lossy,
                    content_bytes: line.content_bytes,
                    ignored: line.ignored,
                    unblamable: line.unblamable,
                    not_committed: line.not_committed,
                })
                .collect();

            let mut commits = HashMap::new();
            for line in &lines {
                commits.entry(line.commit).or_insert_with(|| {
                    if line.not_committed {
                        BlameCommitInfo {
                            author: String::new(),
                            author_time: options.now.map_or(0, |now| now.unix_timestamp()),
                            summary: String::new(),
                        }
                    } else {
                        load_blame_commit_info(odb, line.commit)
                    }
                });
            }
            Ok((lines, commits))
        },
    )?;

    Ok(BlameResult { lines, commits })
}

/// Revision name used in "path not in revision" errors: `HEAD` unless a start was given.
fn blame_revision_label(start: Option<ObjectId>, start_oid: ObjectId) -> String {
    if start.is_some() {
        start_oid.to_hex()
    } else {
        "HEAD".into()
    }
}

/// Load the uncommitted final text `source` selects as an overlay on `start_oid`.
///
/// Returns `None` for [`BlameFinalSource::Revision`] and when the text hashes to the blob
/// already committed at `start_oid`, so a clean file is blamed exactly like the revision.
fn load_blame_overlay(
    repo: &Repository,
    start_oid: ObjectId,
    revision_label: &str,
    file_path: &str,
    source: &BlameFinalSource,
    copy_detection: CopyDetection,
) -> Result<Option<BlameOverlay>> {
    let supplied = match source {
        BlameFinalSource::Revision => return Ok(None),
        BlameFinalSource::Worktree => None,
        BlameFinalSource::Contents(bytes) => Some(bytes.as_slice()),
    };
    let odb = repo.odb();
    let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
    let path_bytes = storage_bytes(file_path);
    let rel_match = crate::repo_relpath::as_utf8_for_matching(file_path);
    let conv = crate::crlf::ConversionConfig::from_config(&config);
    let attrs = repo.work_tree().map(|work_tree| {
        let rules = crate::crlf::load_gitattributes(work_tree);
        crate::crlf::get_file_attrs(&rules, rel_match.as_ref(), false, &config)
    });
    // Only some questions need the index, and a clean blame should not pay for loading it.
    let mut index = None;
    let bytes = match (supplied, repo.work_tree(), attrs.as_ref()) {
        (None, Some(work_tree), Some(attrs)) => {
            let abs = crate::repo_relpath::worktree_abs(work_tree, file_path);
            let meta = std::fs::symlink_metadata(&abs).map_err(|source| {
                crate::worktree::map_worktree_io(
                    rel_match.as_ref(),
                    crate::worktree::WorktreePathOperation::Metadata,
                    source,
                )
            })?;
            let index_entry = if crate::crlf::clean_uses_autocrlf_index_guard(attrs, &conv) {
                loaded_index(repo, &mut index)?.get(&path_bytes, 0)
            } else {
                None
            };
            crate::diff::worktree_blob_bytes(
                odb,
                &abs,
                &meta,
                &conv,
                attrs,
                rel_match.as_ref(),
                index_entry,
                None,
            )?
        }
        (None, _, _) => return Err(LibError::BlameWorktreeBareRepository),
        (Some(bytes), _, Some(attrs)) => {
            crate::crlf::convert_to_git(bytes, rel_match.as_ref(), &conv, attrs)
                .map_err(LibError::Filter)?
        }
        (Some(bytes), _, None) => bytes.to_vec(),
    };

    let start_tree = get_blame_commit(odb, start_oid, &mut BlameCommitCache::default(), None)?.tree;
    let start_blob = resolve_path_in_tree_entry(odb, &start_tree, file_path)?;
    if let Some((blob, _)) = start_blob {
        if odb.hash(ObjectKind::Blob, &bytes)? == blob {
            return Ok(None);
        }
    }
    let mut parents = vec![start_oid];
    parents.extend(crate::state::read_merge_heads(repo.git_dir())?);
    if start_blob.is_none() {
        let staged = loaded_index(repo, &mut index)?;
        let mut known = (0..=3).any(|stage| staged.get(&path_bytes, stage).is_some());
        for parent in &parents[1..] {
            if known {
                break;
            }
            let tree = get_blame_commit(odb, *parent, &mut BlameCommitCache::default(), None)?.tree;
            known = resolve_path_in_tree_entry(odb, &tree, file_path)?.is_some();
        }
        if !known {
            return Err(blame_path_missing_in_revision(file_path, revision_label));
        }
    }

    let copy_depth = copy_detection.depth();
    let search_all_paths =
        copy_depth == 0 || copy_depth >= 3 || (copy_depth == 2 && start_blob.is_none());
    let copy_paths = if search_all_paths {
        None
    } else {
        // Git's fake commit has the index as its tree, so `-C` searches staged changes.
        Some(
            crate::diff::diff_index_to_tree(
                odb,
                loaded_index(repo, &mut index)?,
                Some(&start_tree),
                true,
            )?
            .into_iter()
            .flat_map(|entry| [entry.old_path, entry.new_path])
            .flatten()
            .collect(),
        )
    };
    Ok(Some(BlameOverlay {
        lines: Arc::new(BlobLines::from_bytes(bytes)),
        parents,
        copy_paths,
    }))
}

/// The repository index, read into `slot` on first use.
fn loaded_index<'a>(repo: &Repository, slot: &'a mut Option<Index>) -> Result<&'a Index> {
    if slot.is_none() {
        *slot = Some(repo.load_index()?);
    }
    Ok(slot.get_or_insert_with(Index::default))
}

fn resolve_blame_start(repo: &Repository, start: Option<ObjectId>) -> Result<ObjectId> {
    if let Some(oid) = start {
        return crate::revwalk::rev_parse::peel_to_commit_for_merge_base(repo, oid);
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
    if let std::borrow::Cow::Owned(bytes) = storage_bytes(path) {
        // Stored paths already name a repository-relative file. Do not resolve the
        // ASCII encoding against the work tree or decode a literal prefix twice.
        return storage_string(&bytes);
    }
    let normalized = if let Some(work_tree) = repo.work_tree() {
        crate::pathspec::normalize_worktree_file_path_with_git_dir(
            path,
            work_tree,
            repo.pathspec_argv_prefix(path),
            Some(repo.git_dir()),
        )
    } else {
        path.to_string()
    };
    // Normalization can expose a literal reserved prefix (for example after `./`).
    if normalized.starts_with(crate::repo_relpath::STORAGE_PREFIX) {
        storage_string(normalized.as_bytes())
    } else {
        normalized
    }
}

/// Author and summary for a blamed commit, read leniently so a commit missing `author` or
/// `committer` (or with other malformed headers) still reports what it does have, the same
/// whether the object is loose or packed.
fn load_blame_commit_info(odb: &Odb, oid: ObjectId) -> BlameCommitInfo {
    let (author, message) = match read_object_for_blame(odb, &oid, None) {
        Ok(obj) if obj.kind == ObjectKind::Commit => commit_author_and_message_lenient(&obj.data),
        _ => (None, String::new()),
    };
    let author = author.unwrap_or_default();
    BlameCommitInfo {
        author_time: author_epoch_from_identity(&author),
        author: author.trim().to_string(),
        summary: message
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
    fn blob_lines_preserves_interior_and_trailing_blank_lines() {
        let cases = [
            "alpha\n\nbeta\n\ngamma\n",
            "only\n",
            "\n",
            "a\n\n\nb\n",
            "head\n\n",
        ];
        for content in cases {
            let blob = BlobLines::from_content(content.to_owned());
            let expected: Vec<String> = content_lines(content)
                .into_iter()
                .map(str::to_owned)
                .collect();
            assert_eq!(blob.to_vec(), expected, "content={content:?}");
        }
    }

    #[test]
    fn blob_lines_compare_records_without_changing_bodies() {
        let exact = crate::diff::WhitespaceMode::default();
        let ws = crate::diff::WhitespaceMode::ignore_all_space();
        for text in ["", "hello", "hello\n", "a\r\nb\r", "\n", "a\n\n"] {
            let lines = BlobLines::from_content(text.to_owned());
            assert_eq!(
                lines.comparison_refs(exact),
                text.split_inclusive('\n').collect::<Vec<_>>()
            );
            assert_eq!(lines.comparison_refs(ws), content_lines(text));
            assert_eq!(lines.comparison_refs(exact).concat(), text);
        }
    }

    #[test]
    fn final_newline_is_changed_in_affix_and_exact_maps() {
        let exact = crate::diff::WhitespaceMode::default();
        let ws = crate::diff::WhitespaceMode::ignore_all_space();
        for (before, after) in [
            ("first\nlast", "first\nlast\n"),
            ("first\nlast\n", "first\nlast"),
        ] {
            let old = BlobLines::from_content(before.to_owned());
            let new = BlobLines::from_content(after.to_owned());
            assert_eq!(common_affix_len(&old, &new, exact), (1, 0));
            let pending = vec![TrackedLine {
                final_lineno: 2,
                current_idx: 1,
                ignored: false,
                source_path: None,
                origin: 0,
            }];
            let (affix, need) = partition_pending_by_affix(
                slide_safe_affix(&old, &new, exact),
                old.len(),
                new.len(),
                pending,
            );
            assert!(affix.is_empty());
            assert_eq!(need.len(), 1);
            for algorithm in [
                BlameDiffAlgorithm::Myers,
                BlameDiffAlgorithm::Histogram,
                BlameDiffAlgorithm::Patience,
                BlameDiffAlgorithm::Minimal,
            ] {
                let (map, flags) = build_line_map_with_changed_flags_owned(
                    &old,
                    &new,
                    slide_safe_affix(&old, &new, exact),
                    algorithm,
                    true,
                    true,
                    exact,
                );
                assert_eq!(map, vec![Some(0), None]);
                assert_eq!(flags, vec![false, true]);
                assert_eq!(
                    build_exact_relocate_line_map_ws(
                        &old.comparison_refs(exact),
                        &new.comparison_refs(exact),
                        &map,
                        exact
                    ),
                    map
                );
                let (map, flags) = build_line_map_with_changed_flags_owned(
                    &old,
                    &new,
                    slide_safe_affix(&old, &new, ws),
                    algorithm,
                    true,
                    true,
                    ws,
                );
                assert_eq!(map, vec![Some(0), Some(1)]);
                assert_eq!(flags, vec![false, false]);
            }
        }
    }

    #[test]
    fn shuffle_commit_stops_blame_on_moved_include_line() {
        let old = [
            "",
            "#include \"cache.h\"",
            "#include \"builtin.h\"",
            "#include \"advice.h\"",
            "#include \"pathspec.h\"",
            "static int chmod_pathspec(struct pathspec_item *item, void *cb)",
            "{",
            "    return 0;",
            "}",
            "#include \"run-command.h\"",
            "#include \"builtin.h\"",
        ];
        let new = [
            "",
            "#include \"builtin.h\"",
            "#include \"advice.h\"",
            "#include \"pathspec.h\"",
            "#include \"run-command.h\"",
            "static int chmod_pathspec(struct pathspec_item *item, void *cb)",
            "{",
            "    return 0;",
            "}",
        ];
        let diff = build_line_map(&old, &new, BlameDiffAlgorithm::Myers, true);
        let map = build_exact_relocate_line_map(&old, &new, &diff);
        let (_, flags) = build_line_map_with_changed_flags(
            &old,
            &new,
            BlameDiffAlgorithm::Myers,
            true,
            true,
            crate::diff::WhitespaceMode::default(),
        );
        let stop = blame_stops_on_current_commit(&diff, &map, &flags);
        assert_eq!(diff[4], None);
        assert_eq!(
            map[4],
            Some(9),
            "exact relocate pairs moved text with old index"
        );
        assert!(
            stop[4],
            "moved include stops on shuffle commit: stop={stop:?} map={map:?} flags={flags:?}"
        );
    }

    #[test]
    fn build_line_map_include_shuffle_matches_git_with_indent_heuristic() {
        let old = [
            "",
            "#include \"cache.h\"",
            "#include \"builtin.h\"",
            "#include \"advice.h\"",
            "static void foo(void)",
            "{",
            "}",
        ];
        let new = [
            "",
            "#include \"builtin.h\"",
            "#include \"advice.h\"",
            "static void foo(void)",
            "{",
            "}",
        ];
        let map = build_line_map(&old, &new, BlameDiffAlgorithm::Myers, true);
        assert_eq!(
            map,
            vec![Some(0), Some(2), Some(3), Some(4), Some(5), Some(6),],
            "git keeps matching includes through equal hunks"
        );
    }

    #[test]
    fn build_line_map_maps_unchanged_lines_and_drops_replaced() {
        let old = ["alpha", "beta", "gamma"];
        let new = ["alpha", "BETA", "gamma"];
        let map = build_line_map(&old, &new, BlameDiffAlgorithm::Myers, false);
        // unchanged lines map back to their old index; the replaced line is None.
        assert_eq!(map, vec![Some(0), None, Some(2)]);
    }

    #[test]
    fn build_line_map_slides_insertion_into_unchanged_suffix() {
        let old = [
            "PROG=a b c",
            "\t$(CC) -o cat-file",
            "",
            "read-cache.o: cache.h",
        ];
        let new = [
            "PROG=a b c fsck-cache",
            "\t$(CC) -o cat-file",
            "",
            "fsck-cache: fsck-cache.o",
            "\t$(CC) -o fsck-cache",
            "",
            "read-cache.o: cache.h",
        ];
        for algorithm in [BlameDiffAlgorithm::Myers, BlameDiffAlgorithm::Histogram] {
            for indent_heuristic in [true, false] {
                let map = build_line_map(&old, &new, algorithm, indent_heuristic);
                assert_eq!(
                    map,
                    vec![None, Some(1), Some(2), None, None, None, Some(3)],
                    "{algorithm:?} indent_heuristic={indent_heuristic}"
                );
            }
        }
        let old_lines = BlobLines::from_content(old.map(|l| format!("{l}\n")).concat());
        let new_lines = BlobLines::from_content(new.map(|l| format!("{l}\n")).concat());
        let exact = crate::diff::WhitespaceMode::default();
        let (map, flags) = build_line_map_with_changed_flags_owned(
            &old_lines,
            &new_lines,
            slide_safe_affix(&old_lines, &new_lines, exact),
            BlameDiffAlgorithm::Myers,
            true,
            true,
            exact,
        );
        assert_eq!(map, vec![None, Some(1), Some(2), None, None, None, Some(3)]);
        assert_eq!(flags, vec![true, false, false, true, true, true, false]);
        let pending = (0..new.len())
            .map(|i| TrackedLine {
                final_lineno: i + 1,
                current_idx: i,
                ignored: false,
                source_path: None,
                origin: 0,
            })
            .collect();
        let (affix, need) = partition_pending_by_affix(
            slide_safe_affix(&old_lines, &new_lines, exact),
            old.len(),
            new.len(),
            pending,
        );
        let affix: Vec<(usize, usize)> = affix
            .iter()
            .map(|t| (t.final_lineno, t.current_idx))
            .collect();
        assert_eq!(
            affix,
            vec![(7, 3)],
            "the trailing blank can slide, so it needs the diff"
        );
        assert_eq!(need.len(), 6);
    }

    #[test]
    fn blame_tail_trim_follows_git_block_rule() {
        let exact = crate::diff::WhitespaceMode::default();
        let old_text = format!("a\n{}", "b\n".repeat(1000));
        let new_text = format!("a\n{}", "b\n".repeat(1001));
        let old = BlobLines::from_content(old_text.clone());
        let new = BlobLines::from_content(new_text.clone());
        // 2001 common tail bytes leave one 1 KiB block; the 511 two-byte lines after the line
        // it starts in are dropped.
        let affix = slide_safe_affix(&old, &new, exact);
        assert_eq!(blame_trimmed_tail_lines(&old, &new, affix, exact), 511);
        let old_refs: Vec<&str> = old_text.lines().collect();
        let new_refs: Vec<&str> = new_text.lines().collect();
        let slice_affix = slide_safe_affix(old_refs.as_slice(), new_refs.as_slice(), exact);
        assert_eq!(
            blame_trimmed_tail_lines(old_refs.as_slice(), new_refs.as_slice(), slice_affix, exact),
            511
        );
        let ws = crate::diff::WhitespaceMode::ignore_all_space();
        let ws_affix = slide_safe_affix(&old, &new, ws);
        assert_eq!(blame_trimmed_tail_lines(&old, &new, ws_affix, ws), 511);

        let map = build_line_map(&old_refs, &new_refs, BlameDiffAlgorithm::Myers, true);
        let inserted: Vec<usize> = (0..map.len()).filter(|&i| map[i].is_none()).collect();
        assert_eq!(
            inserted,
            vec![490],
            "the insertion stops where git cut the tail"
        );

        let short_old = BlobLines::from_content("a\nb\n".to_owned());
        let short_new = BlobLines::from_content("x\nb\n".to_owned());
        let affix = slide_safe_affix(&short_old, &short_new, exact);
        assert_eq!(
            blame_trimmed_tail_lines(&short_old, &short_new, affix, exact),
            0
        );
    }

    /// Small deterministic generator for the window equivalence test.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % bound
        }
    }

    #[test]
    fn windowed_line_map_matches_compacting_git_view() {
        // Frequent lines sit near xdiff's frequency limit for these file sizes, and blank and
        // indented lines give the indent heuristic something to weigh.
        let pool = [
            "",
            "",
            "}",
            "\t}",
            "#",
            "\t\treturn 0;",
            "\tif (x) {",
            "\tx = 1;",
            "fn a() {",
        ];
        let line = |rng: &mut Lcg, i: usize| -> String {
            let pick = rng.next(pool.len() * 2);
            pool.get(pick)
                .map_or_else(|| format!("line {i} {}", rng.next(4)), |l| (*l).to_owned())
        };
        let mut rng = Lcg(7);
        for case in 0..400 {
            let len = 60 + rng.next(500);
            let old: Vec<String> = (0..len).map(|i| line(&mut rng, i)).collect();
            let mut new = old.clone();
            for _ in 0..1 + rng.next(4) {
                let at = rng.next(new.len());
                let n = 1 + rng.next(6);
                match rng.next(3) {
                    0 => {
                        for k in 0..n {
                            let l = line(&mut rng, k);
                            new.insert(at, l);
                        }
                    }
                    1 => {
                        new.drain(at..(at + n).min(new.len()));
                    }
                    _ => {
                        let end = (at + n).min(new.len());
                        for slot in &mut new[at..end] {
                            *slot = line(&mut rng, case);
                        }
                    }
                }
            }
            let old: Vec<&str> = old.iter().map(String::as_str).collect();
            let new: Vec<&str> = new.iter().map(String::as_str).collect();
            assert_window_matches_git_view(&old, &new, &format!("case {case}"));
        }
    }

    /// A blank line stops the slide, so the indent heuristic reads the blank run before it.
    #[test]
    fn windowed_line_map_keeps_blank_context_for_indent_heuristic() {
        let block = ["\tif (x) {", "\t\ty();", "\t}"];
        let mut old = vec!["top"];
        old.extend(std::iter::repeat_n("", 25));
        old.extend(block);
        old.push("end");
        let mut new = old.clone();
        new.splice(29..29, block);
        assert_window_matches_git_view(&old, &new, "blank run");
    }

    /// A `}` frequent only outside the window still counts as frequent.
    #[test]
    fn windowed_line_map_counts_lines_outside_the_window() {
        let version = |middle: [&'static str; 8]| -> Vec<String> {
            let mut lines = vec!["}".to_owned(); 60];
            lines.extend((0..30).map(|i| format!("head {i}")));
            lines.extend(middle.map(str::to_owned));
            lines.extend((0..30).map(|i| format!("tail {i}")));
            lines
        };
        let old = version(["u1", "u2", "u3", "u4", "}", "u5", "u6", "u7"]);
        let new = version(["v1", "v2", "v3", "v4", "}", "v5", "v6", "v7"]);
        let old: Vec<&str> = old.iter().map(String::as_str).collect();
        let new: Vec<&str> = new.iter().map(String::as_str).collect();
        assert_window_matches_git_view(&old, &new, "frequent brace");
        let map = build_line_map(&old, &new, BlameDiffAlgorithm::Myers, true);
        assert!(
            map[90..98].iter().all(Option::is_none),
            "{:?}",
            &map[88..100]
        );
    }

    /// Windowed maps equal compacting all of Git's view of the files, for every algorithm.
    fn assert_window_matches_git_view(old: &[&str], new: &[&str], label: &str) {
        let exact = crate::diff::WhitespaceMode::default();
        for algorithm in [BlameDiffAlgorithm::Myers, BlameDiffAlgorithm::Histogram] {
            for indent_heuristic in [true, false] {
                let affix = slide_safe_affix(old, new, exact);
                let tail = blame_trimmed_tail_lines(old, new, affix, exact);
                let (old_v, new_v) = (&old[..old.len() - tail], &new[..new.len() - tail]);
                let ops = blame_compacted_line_ops(
                    old_v,
                    new_v,
                    std::iter::empty(),
                    0,
                    algorithm,
                    indent_heuristic,
                    exact,
                );
                let mut expected =
                    crate::diff_indent_heuristic::map_new_to_old_from_ops(&ops, new_v.len());
                expected.extend((old.len() - tail..old.len()).map(Some));
                let mut expected_flags =
                    crate::diff_indent_heuristic::new_line_changed_flags_from_ops(
                        &ops,
                        new_v.len(),
                    );
                expected_flags.resize(new.len(), false);
                let (map, flags) =
                    windowed_line_map(old, new, affix, algorithm, indent_heuristic, true, exact);
                assert_eq!(map, expected, "{label} {algorithm:?} {indent_heuristic}");
                assert_eq!(
                    flags, expected_flags,
                    "{label} {algorithm:?} {indent_heuristic}"
                );
            }
        }
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
    fn partition_pending_by_affix_maps_prefix_and_suffix() {
        let old = BlobLines::from_content("a\nb\nc\nd\ne\n".to_owned());
        let new = BlobLines::from_content("a\nb\nx\nd\ne\n".to_owned());
        let pending = vec![TrackedLine {
            final_lineno: 1,
            current_idx: 0,
            ignored: false,
            source_path: None,
            origin: 0,
        }];
        let exact = crate::diff::WhitespaceMode::default();
        let (affix, need) = partition_pending_by_affix(
            slide_safe_affix(&old, &new, exact),
            old.len(),
            new.len(),
            pending,
        );
        assert!(need.is_empty());
        assert_eq!(affix[0].current_idx, 0);

        let pending_mid = vec![TrackedLine {
            final_lineno: 3,
            current_idx: 2,
            ignored: false,
            source_path: None,
            origin: 0,
        }];
        let (affix, need) = partition_pending_by_affix(
            slide_safe_affix(&old, &new, exact),
            old.len(),
            new.len(),
            pending_mid,
        );
        assert!(affix.is_empty());
        assert_eq!(need.len(), 1);
    }

    #[test]
    fn blame_result_hunks_group_contiguous_runs() {
        use crate::objects::ObjectId;

        let c1 = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let c2 = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        let result = BlameResult {
            lines: vec![
                BlameAttributedLine {
                    commit: c1,
                    original_line: 1,
                    final_line: 1,
                    original_path: "a.txt".to_owned(),
                    content: "one".to_owned(),
                    encoding_lossy: false,
                    content_bytes: None,
                    ignored: false,
                    unblamable: false,
                    not_committed: false,
                },
                BlameAttributedLine {
                    commit: c1,
                    original_line: 2,
                    final_line: 2,
                    original_path: "a.txt".to_owned(),
                    content: "two".to_owned(),
                    encoding_lossy: false,
                    content_bytes: None,
                    ignored: false,
                    unblamable: false,
                    not_committed: false,
                },
                BlameAttributedLine {
                    commit: c2,
                    original_line: 4,
                    final_line: 3,
                    original_path: "b.txt".to_owned(),
                    content: "three".to_owned(),
                    encoding_lossy: false,
                    content_bytes: None,
                    ignored: false,
                    unblamable: false,
                    not_committed: false,
                },
            ],
            commits: HashMap::new(),
        };
        let hunks = result.hunks();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].count, 2);
        assert_eq!(hunks[0].original_start, 1);
        assert_eq!(hunks[0].final_start, 1);
        assert_eq!(hunks[1].count, 1);
        assert_eq!(hunks[1].original_path, "b.txt");
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
    fn blame_file_resolves_nested_path_in_wide_root_tree() -> Result<()> {
        use crate::objects::{serialize_tree, tree_entry_cmp, ObjectKind, TreeEntry};
        use crate::repo::init_repository;
        use crate::RefStorageFormat;
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().map_err(crate::error::Error::Io)?;
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)?;
        let hello = repo.odb().write(ObjectKind::Blob, b"hello\n")?;
        let mut sub_body = b"100644 inner.txt\0".to_vec();
        sub_body.extend_from_slice(hello.as_bytes());
        let sub_tree = repo.odb().write(ObjectKind::Tree, &sub_body)?;
        let filler = repo.odb().write(ObjectKind::Blob, b"1\n")?;
        let mut entries: Vec<TreeEntry> = (1..=60)
            .map(|i| TreeEntry {
                mode: 0o100644,
                name: format!("f{i}.txt").into_bytes(),
                oid: filler,
            })
            .collect();
        entries.push(TreeEntry {
            mode: 0o040000,
            name: b"sub".to_vec(),
            oid: sub_tree,
        });
        entries.sort_by(|a, b| {
            tree_entry_cmp(&a.name, a.mode == 0o040000, &b.name, b.mode == 0o040000)
        });
        let root_tree = repo
            .odb()
            .write(ObjectKind::Tree, &serialize_tree(&entries))?;
        let commit_body = format!(
            "tree {root_tree}\nauthor T <t@t.com> 1 +0000\ncommitter T <t@t.com> 1 +0000\n\ninit\n"
        );
        let tip = repo
            .odb()
            .write(ObjectKind::Commit, commit_body.as_bytes())?;
        fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))
            .map_err(crate::error::Error::Io)?;
        let result = blame_file(
            &repo,
            "sub/inner.txt",
            &BlameOptions {
                start: Some(tip),
                ..Default::default()
            },
        )?;
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].content, "hello");
        Ok(())
    }

    fn write_file_commit(
        repo: &Repository,
        path: &str,
        lines: &[String],
        parents: &[ObjectId],
        time: i64,
    ) -> Result<ObjectId> {
        use crate::objects::ObjectKind;
        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let blob = repo.odb().write(ObjectKind::Blob, content.as_bytes())?;
        let mut tree_body = format!("100644 {path}\0").into_bytes();
        tree_body.extend_from_slice(blob.as_bytes());
        let tree = repo.odb().write(ObjectKind::Tree, &tree_body)?;
        let mut body = format!("tree {tree}\n");
        for parent in parents {
            body.push_str(&format!("parent {parent}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> {time} +0000\ncommitter T <t@e.com> {time} +0000\n\nc{time}\n"
        ));
        repo.odb().write(ObjectKind::Commit, body.as_bytes())
    }

    /// Side branches that keep a mainline line's older text hand that line to the merge's
    /// second parent, whose history runs back into the mainline. Every pair must still be
    /// diffed once: suspects reaching the same commit are merged, not walked again.
    #[test]
    fn merge_ladder_diffs_each_parent_pair_once() -> Result<()> {
        use crate::repo::init_repository;
        use crate::RefStorageFormat;

        let dir = tempfile::tempdir().map_err(crate::error::Error::Io)?;
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)?;
        let path = "ladder.txt";
        let base: Vec<String> = (1..=40).map(|i| format!("base {i}")).collect();
        let root = write_file_commit(&repo, path, &base, &[], 1000)?;
        let mut tip = root;
        let mut tip_lines = base.clone();
        let mut main_commits = Vec::new();
        let mut side_commits = Vec::new();
        for k in 1..=8_usize {
            let t = 1000 + 10 * k as i64;
            let mut main_lines = tip_lines.clone();
            main_lines[k - 1] = format!("main {k}");
            main_lines[9 + k] = format!("kept main {k}");
            let main = write_file_commit(&repo, path, &main_lines, &[tip], t)?;
            let mut side_lines = tip_lines.clone();
            side_lines[19 + k] = format!("side {k}");
            let side = write_file_commit(&repo, path, &side_lines, &[tip], t + 1)?;
            // The merge keeps main's second edit but the side's (older) text of line `k`.
            let mut merged = side_lines.clone();
            merged[9 + k] = main_lines[9 + k].clone();
            tip = write_file_commit(&repo, path, &merged, &[main, side], t + 2)?;
            tip_lines = merged;
            main_commits.push(main);
            side_commits.push(side);
        }

        let thread_policy = repo.thread_policy();
        let mut session = BlameSession::new(repo.odb(), None, false, &thread_policy);
        let lines = compute_blame_with_session(
            repo.odb(),
            tip,
            "tip",
            path,
            &HashSet::new(),
            BlameDiffAlgorithm::Myers,
            None,
            false,
            0,
            false,
            &HashMap::new(),
            false,
            None,
            None,
            &mut session,
        )?;

        assert_eq!(lines.len(), 40);
        for line in &lines {
            let expected = match line.final_lineno {
                n @ 11..=18 => main_commits[n - 11],
                n @ 21..=28 => side_commits[n - 21],
                _ => root,
            };
            assert_eq!(line.oid, expected, "line {}", line.final_lineno);
        }
        let unique: HashSet<_> = session.diffed_pairs.iter().collect();
        assert_eq!(
            unique.len(),
            session.diffed_pairs.len(),
            "a parent/child pair was diffed more than once"
        );
        Ok(())
    }

    #[test]
    fn copy_detection_from_count_maps_git_levels() -> Result<()> {
        assert_eq!(CopyDetection::from_count(0)?, CopyDetection::Off);
        assert_eq!(
            CopyDetection::from_count(1)?,
            CopyDetection::FromModifiedFiles
        );
        assert_eq!(
            CopyDetection::from_count(2)?,
            CopyDetection::FromCreatingCommit
        );
        assert_eq!(CopyDetection::from_count(3)?, CopyDetection::FromAnyCommit);
        assert!(CopyDetection::from_count(4).is_err());
        Ok(())
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

    #[test]
    fn stored_path_lookup_preserves_raw_names_and_cache_keys() -> Result<()> {
        use crate::objects::{serialize_tree, TreeEntry};
        use crate::repo::init_repository;
        use crate::RefStorageFormat;

        let dir = tempfile::tempdir()?;
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)?;
        let mut entries = Vec::new();
        let names: &[&[u8]] = &[
            b"ab",
            b"grit+pathhex:6162",
            b"raw\xfe",
            b"raw\xff",
            "raw\u{fffd}".as_bytes(),
        ];
        for name in names {
            let oid = repo.odb().write(ObjectKind::Blob, name)?;
            entries.push(TreeEntry {
                mode: 0o100644,
                name: name.to_vec(),
                oid,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let subtree = repo
            .odb()
            .write(ObjectKind::Tree, &serialize_tree(&entries))?;
        let root = repo.odb().write(
            ObjectKind::Tree,
            &serialize_tree(&[TreeEntry {
                mode: 0o040000,
                name: b"dir\xff".to_vec(),
                oid: subtree,
            }]),
        )?;
        let policy = ThreadPolicy::default();
        let mut session = BlameSession::new(repo.odb(), None, false, &policy);
        for entry in entries {
            let raw = crate::repo_relpath::join_rel(b"dir\xff", &entry.name);
            let stored = storage_string(&raw);
            let expected = Some((entry.oid, entry.mode));
            assert_eq!(session.resolve_path_in_tree(root, &stored)?, expected);
            assert_eq!(
                resolve_path_in_tree_entry(repo.odb(), &root, &stored)?,
                expected
            );
            let count = session.path_entries.len();
            let uppercase = format!(
                "grit+pathhex:{}",
                stored
                    .strip_prefix("grit+pathhex:")
                    .unwrap_or_default()
                    .to_ascii_uppercase()
            );
            assert_eq!(session.resolve_path_in_tree(root, &uppercase)?, expected);
            assert_eq!(session.path_entries.len(), count);
        }
        Ok(())
    }

    #[test]
    fn stored_path_bloom_keys_hash_raw_directory_prefixes() -> Result<()> {
        use crate::bloom::{bloom_key_hashes, BloomFilterSettings};
        use crate::repo::init_repository;
        use crate::RefStorageFormat;

        let dir = tempfile::tempdir()?;
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)?;
        let policy = ThreadPolicy::default();
        let mut session = BlameSession::new(repo.odb(), None, false, &policy);
        for version in [1, 2] {
            let settings = BloomFilterSettings {
                hash_version: version,
                ..Default::default()
            };
            for path in [
                b"dir\xff/raw\xfe.txt".as_slice(),
                b"grit+pathhex:6162/file",
                b"utf8/file",
            ] {
                let slash = path.iter().rposition(|&b| b == b'/').unwrap_or_default();
                let expected = vec![
                    bloom_key_hashes(path, &settings),
                    bloom_key_hashes(&path[..slash], &settings),
                ];
                let keys = session.bloom_keys_for_path(&storage_string(path), &settings);
                assert_eq!(keys.as_ref(), &expected);
            }
        }
        Ok(())
    }
}
