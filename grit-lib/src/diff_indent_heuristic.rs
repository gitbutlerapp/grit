//! Git-compatible diff hunk sliding (`xdl_change_compact`) including the indent heuristic.
//!
//! Ported from Git's `xdiff/xdiffi.c` (`xdl_change_compact`, `score_split`, etc.).

use similar::{Algorithm, DiffOp, DiffTag, TextDiff};

const MAX_INDENT: i32 = 200;
const MAX_BLANKS: i32 = 20;
const START_OF_FILE_PENALTY: i32 = 1;
const END_OF_FILE_PENALTY: i32 = 21;
const TOTAL_BLANK_WEIGHT: i32 = -30;
const POST_BLANK_WEIGHT: i32 = 6;
const RELATIVE_INDENT_PENALTY: i32 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i32 = 10;
const RELATIVE_OUTDENT_PENALTY: i32 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i32 = 17;
const RELATIVE_DEDENT_PENALTY: i32 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i32 = 17;
const INDENT_WEIGHT: i32 = 60;
const INDENT_HEURISTIC_MAX_SLIDING: isize = 100;

#[derive(Clone)]
struct XdFile<'a> {
    lines: &'a [&'a str],
    /// `changed[i]` matches Git's `xdf->changed`: length `nrec + 1`, last entry always false.
    changed: Vec<bool>,
}

impl<'a> XdFile<'a> {
    fn from_mask(lines: &'a [&'a str], mut changed: Vec<bool>) -> Self {
        let n = lines.len();
        changed.resize(n + 1, false);
        if !changed.is_empty() {
            changed[n] = false;
        }
        Self { lines, changed }
    }

    fn nrec(&self) -> usize {
        self.lines.len()
    }

    fn changed_at(&self, i: isize) -> bool {
        if i < 0 {
            return false;
        }
        let u = i as usize;
        if u >= self.changed.len() {
            return false;
        }
        self.changed[u]
    }

    fn set_changed(&mut self, i: usize, v: bool) {
        if i < self.changed.len() {
            self.changed[i] = v;
        }
    }

    fn into_changed(self) -> Vec<bool> {
        let n = self.lines.len();
        let mut c = self.changed;
        c.truncate(n);
        c
    }
}

fn get_indent(line: &str) -> i32 {
    let mut ret = 0_i32;
    for c in line.chars() {
        if c == ' ' {
            ret += 1;
        } else if c == '\t' {
            ret += 8 - ret % 8;
        } else if c.is_whitespace() {
            // Git ignores other whitespace in indent (no advance)
        } else {
            return ret;
        }
        if ret >= MAX_INDENT {
            return MAX_INDENT;
        }
    }
    -1
}

#[derive(Default)]
struct SplitMeasurement {
    end_of_file: i32,
    indent: i32,
    pre_blank: i32,
    pre_indent: i32,
    post_blank: i32,
    post_indent: i32,
}

fn measure_split(xdf: &XdFile<'_>, split: isize, m: &mut SplitMeasurement) {
    let n = xdf.nrec() as isize;
    if split >= n {
        m.end_of_file = 1;
        m.indent = -1;
    } else {
        m.end_of_file = 0;
        m.indent = get_indent(xdf.lines[split as usize]);
    }

    m.pre_blank = 0;
    m.pre_indent = -1;
    let mut i = split - 1;
    loop {
        if i < 0 {
            break;
        }
        m.pre_indent = get_indent(xdf.lines[i as usize]);
        if m.pre_indent != -1 {
            break;
        }
        m.pre_blank += 1;
        if m.pre_blank == MAX_BLANKS {
            m.pre_indent = 0;
            break;
        }
        i -= 1;
    }

    m.post_blank = 0;
    m.post_indent = -1;
    i = split + 1;
    loop {
        if i >= n {
            break;
        }
        m.post_indent = get_indent(xdf.lines[i as usize]);
        if m.post_indent != -1 {
            break;
        }
        m.post_blank += 1;
        if m.post_blank == MAX_BLANKS {
            m.post_indent = 0;
            break;
        }
        i += 1;
    }
}

#[derive(Default, Clone)]
struct SplitScore {
    effective_indent: i32,
    penalty: i32,
}

fn score_add_split(m: &SplitMeasurement, s: &mut SplitScore) {
    if m.pre_indent == -1 && m.pre_blank == 0 {
        s.penalty += START_OF_FILE_PENALTY;
    }

    if m.end_of_file != 0 {
        s.penalty += END_OF_FILE_PENALTY;
    }

    let post_blank = if m.indent == -1 { 1 + m.post_blank } else { 0 };
    let total_blank = m.pre_blank + post_blank;

    s.penalty += TOTAL_BLANK_WEIGHT * total_blank;
    s.penalty += POST_BLANK_WEIGHT * post_blank;

    let indent = if m.indent != -1 {
        m.indent
    } else {
        m.post_indent
    };

    let any_blanks = total_blank != 0;

    s.effective_indent += indent;

    if indent != -1 && m.pre_indent != -1 {
        if indent > m.pre_indent {
            s.penalty += if any_blanks {
                RELATIVE_INDENT_WITH_BLANK_PENALTY
            } else {
                RELATIVE_INDENT_PENALTY
            };
        } else if indent != m.pre_indent {
            if m.post_indent != -1 && m.post_indent > indent {
                s.penalty += if any_blanks {
                    RELATIVE_OUTDENT_WITH_BLANK_PENALTY
                } else {
                    RELATIVE_OUTDENT_PENALTY
                };
            } else {
                s.penalty += if any_blanks {
                    RELATIVE_DEDENT_WITH_BLANK_PENALTY
                } else {
                    RELATIVE_DEDENT_PENALTY
                };
            }
        }
    }
}

fn score_cmp(s1: &SplitScore, s2: &SplitScore) -> i32 {
    let cmp_indents = (s1.effective_indent > s2.effective_indent) as i32
        - (s1.effective_indent < s2.effective_indent) as i32;
    INDENT_WEIGHT * cmp_indents + (s1.penalty - s2.penalty)
}

#[derive(Clone, Copy)]
struct XdlGroup {
    start: isize,
    end: isize,
}

fn group_init(xdf: &XdFile<'_>, g: &mut XdlGroup) {
    g.start = 0;
    g.end = 0;
    let n = xdf.nrec() as isize;
    while g.end < n && xdf.changed_at(g.end) {
        g.end += 1;
    }
}

fn group_next(xdf: &XdFile<'_>, g: &mut XdlGroup) -> bool {
    let n = xdf.nrec() as isize;
    if g.end == n {
        return false;
    }
    g.start = g.end + 1;
    g.end = g.start;
    while g.end < n && xdf.changed_at(g.end) {
        g.end += 1;
    }
    true
}

fn group_previous(xdf: &XdFile<'_>, g: &mut XdlGroup) -> bool {
    if g.start == 0 {
        return false;
    }
    g.end = g.start - 1;
    g.start = g.end;
    while g.start > 0 && xdf.changed_at(g.start - 1) {
        g.start -= 1;
    }
    true
}

fn recs_match(xdf: &XdFile<'_>, i: isize, j: isize) -> bool {
    if i < 0 || j < 0 {
        return false;
    }
    let ui = i as usize;
    let uj = j as usize;
    if ui >= xdf.nrec() || uj >= xdf.nrec() {
        return false;
    }
    xdf.lines[ui] == xdf.lines[uj]
}

fn group_slide_down(xdf: &mut XdFile<'_>, g: &mut XdlGroup) -> bool {
    let n = xdf.nrec() as isize;
    if g.end < n && recs_match(xdf, g.start, g.end) {
        xdf.set_changed(g.start as usize, false);
        xdf.set_changed(g.end as usize, true);
        g.start += 1;
        g.end += 1;
        while g.end < n && xdf.changed_at(g.end) {
            g.end += 1;
        }
        return true;
    }
    false
}

fn group_slide_up(xdf: &mut XdFile<'_>, g: &mut XdlGroup) -> bool {
    if g.start > 0 && recs_match(xdf, g.start - 1, g.end - 1) {
        g.start -= 1;
        g.end -= 1;
        xdf.set_changed(g.start as usize, true);
        xdf.set_changed(g.end as usize, false);
        while g.start > 0 && xdf.changed_at(g.start - 1) {
            g.start -= 1;
        }
        return true;
    }
    false
}

fn change_compact_one(xdf: &mut XdFile<'_>, xdfo: &mut XdFile<'_>, indent_heuristic: bool) {
    let mut g = XdlGroup { start: 0, end: 0 };
    let mut go = XdlGroup { start: 0, end: 0 };
    group_init(xdf, &mut g);
    group_init(xdfo, &mut go);

    loop {
        if g.end != g.start {
            loop {
                let groupsize = g.end - g.start;
                let mut end_matching_other = -1_isize;

                // Git's C uses `while (!group_slide_up)` with 0=success; here bool true=success.
                while group_slide_up(xdf, &mut g) {
                    let slid = group_previous(xdfo, &mut go);
                    debug_assert!(slid, "group sync broken sliding up");
                }

                let earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = g.end;
                }

                loop {
                    if !group_slide_down(xdf, &mut g) {
                        break;
                    }
                    let slid = group_next(xdfo, &mut go);
                    debug_assert!(slid, "group sync broken sliding down");
                    if go.end > go.start {
                        end_matching_other = g.end;
                    }
                }

                if groupsize != g.end - g.start {
                    continue;
                }

                if g.end == earliest_end {
                    // no shifting was possible
                } else if end_matching_other != -1 {
                    while go.end == go.start {
                        let slid = group_slide_up(xdf, &mut g);
                        debug_assert!(slid, "match disappeared");
                        let p = group_previous(xdfo, &mut go);
                        debug_assert!(p, "group sync broken sliding to match");
                    }
                } else if indent_heuristic {
                    apply_indent_heuristic_shift(
                        xdf,
                        xdfo,
                        &mut g,
                        &mut go,
                        groupsize,
                        earliest_end,
                    );
                }
                break;
            }
        }

        if !group_next(xdf, &mut g) {
            break;
        }
        let advanced = group_next(xdfo, &mut go);
        debug_assert!(advanced, "group sync broken at end of file");
    }
}

fn apply_indent_heuristic_shift(
    xdf: &mut XdFile<'_>,
    xdfo: &mut XdFile<'_>,
    g: &mut XdlGroup,
    go: &mut XdlGroup,
    groupsize: isize,
    earliest_end: isize,
) {
    let mut shift = earliest_end;
    if g.end - groupsize - 1 > shift {
        shift = g.end - groupsize - 1;
    }
    if g.end - INDENT_HEURISTIC_MAX_SLIDING > shift {
        shift = g.end - INDENT_HEURISTIC_MAX_SLIDING;
    }

    let mut best_shift: Option<isize> = None;
    let mut best_score = SplitScore::default();

    let mut s = shift;
    while s <= g.end {
        let mut m = SplitMeasurement::default();
        let mut score = SplitScore::default();
        measure_split(xdf, s, &mut m);
        score_add_split(&m, &mut score);
        measure_split(xdf, s - groupsize, &mut m);
        score_add_split(&m, &mut score);

        if best_shift.is_none() || score_cmp(&score, &best_score) <= 0 {
            best_score.effective_indent = score.effective_indent;
            best_score.penalty = score.penalty;
            best_shift = Some(s);
        }
        s += 1;
    }

    let Some(best_shift) = best_shift else {
        return;
    };
    while g.end > best_shift {
        let _ = group_slide_up(xdf, g);
        let _ = group_previous(xdfo, go);
    }
}

fn change_compact_both_passes(
    old_lines: &[&str],
    new_lines: &[&str],
    changed_old: &mut Vec<bool>,
    changed_new: &mut Vec<bool>,
    indent_heuristic: bool,
) {
    let c_old = std::mem::take(changed_old);
    let c_new = std::mem::take(changed_new);
    let mut xdf = XdFile::from_mask(old_lines, c_old);
    let mut xdfo = XdFile::from_mask(new_lines, c_new);

    change_compact_one(&mut xdf, &mut xdfo, indent_heuristic);
    *changed_old = xdf.into_changed();
    *changed_new = xdfo.into_changed();

    let mut xdf2 = XdFile::from_mask(new_lines, std::mem::take(changed_new));
    let mut xdfo2 = XdFile::from_mask(old_lines, std::mem::take(changed_old));
    change_compact_one(&mut xdf2, &mut xdfo2, indent_heuristic);
    *changed_new = xdf2.into_changed();
    *changed_old = xdfo2.into_changed();
}

/// Initial `changed[]` masks from a line diff (Myers etc.), matching Git's xdf `changed` flags.
fn masks_from_ops(ops: &[DiffOp], old_len: usize, new_len: usize) -> (Vec<bool>, Vec<bool>) {
    let mut c1 = vec![false; old_len];
    let mut c2 = vec![false; new_len];
    for op in ops {
        match op.tag() {
            DiffTag::Equal => {}
            DiffTag::Delete => {
                for i in op.old_range() {
                    if i < old_len {
                        c1[i] = true;
                    }
                }
            }
            DiffTag::Insert => {
                for j in op.new_range() {
                    if j < new_len {
                        c2[j] = true;
                    }
                }
            }
            DiffTag::Replace => {
                for i in op.old_range() {
                    if i < old_len {
                        c1[i] = true;
                    }
                }
                for j in op.new_range() {
                    if j < new_len {
                        c2[j] = true;
                    }
                }
            }
        }
    }
    (c1, c2)
}

/// Rebuild `DiffOp` list from compacted masks (Git `xdl_build_script`, reversed collect).
fn ops_from_masks(old_lines: &[&str], new_lines: &[&str], c1: &[bool], c2: &[bool]) -> Vec<DiffOp> {
    let n1 = old_lines.len() as isize;
    let n2 = new_lines.len() as isize;
    let mut out_rev: Vec<DiffOp> = Vec::new();

    let changed1 = |i: isize| -> bool {
        if i < 0 || i >= n1 {
            return false;
        }
        c1[i as usize]
    };
    let changed2 = |i: isize| -> bool {
        if i < 0 || i >= n2 {
            return false;
        }
        c2[i as usize]
    };

    let mut i1 = n1;
    let mut i2 = n2;
    while i1 > 0 || i2 > 0 {
        if changed1(i1 - 1) || changed2(i2 - 1) {
            let l1 = i1;
            let l2 = i2;
            while i1 > 0 && changed1(i1 - 1) {
                i1 -= 1;
            }
            while i2 > 0 && changed2(i2 - 1) {
                i2 -= 1;
            }
            let chg1 = (l1 - i1) as usize;
            let chg2 = (l2 - i2) as usize;
            let i1u = i1 as usize;
            let i2u = i2 as usize;

            if chg1 > 0 && chg2 > 0 {
                out_rev.push(DiffOp::Replace {
                    old_index: i1u,
                    old_len: chg1,
                    new_index: i2u,
                    new_len: chg2,
                });
            } else if chg1 > 0 {
                out_rev.push(DiffOp::Delete {
                    old_index: i1u,
                    old_len: chg1,
                    new_index: i2u,
                });
            } else if chg2 > 0 {
                out_rev.push(DiffOp::Insert {
                    old_index: i1u,
                    new_index: i2u,
                    new_len: chg2,
                });
            }
        } else {
            // equal line
            i1 -= 1;
            i2 -= 1;
            out_rev.push(DiffOp::Equal {
                old_index: i1 as usize,
                new_index: i2 as usize,
                len: 1,
            });
        }
    }

    out_rev.reverse();
    merge_adjacent_equal_ops(out_rev)
}

fn merge_adjacent_equal_ops(ops: Vec<DiffOp>) -> Vec<DiffOp> {
    if ops.is_empty() {
        return ops;
    }
    let mut merged: Vec<DiffOp> = Vec::with_capacity(ops.len());
    for op in ops {
        if let (
            Some(DiffOp::Equal {
                old_index: o0,
                new_index: n0,
                len: l0,
            }),
            DiffOp::Equal {
                old_index: o1,
                new_index: n1,
                len: l1,
            },
        ) = (merged.last(), op)
        {
            if o0 + l0 == o1 && n0 + l0 == n1 {
                let last = merged.len() - 1;
                if let DiffOp::Equal { len, .. } = &mut merged[last] {
                    *len += l1;
                }
                continue;
            }
        }
        merged.push(op);
    }
    merged
}

/// Apply Git `xdl_change_compact` twice (old/new), optionally with `XDF_INDENT_HEURISTIC`.
pub fn apply_change_compact_to_ops(
    ops: &[DiffOp],
    old_lines: &[&str],
    new_lines: &[&str],
    indent_heuristic: bool,
) -> Vec<DiffOp> {
    let (mut c1, mut c2) = masks_from_ops(ops, old_lines.len(), new_lines.len());
    change_compact_both_passes(old_lines, new_lines, &mut c1, &mut c2, indent_heuristic);
    ops_from_masks(old_lines, new_lines, &c1, &c2)
}

/// Line-diff then Git-style compaction; used by unified diff generation.
pub fn diff_lines_ops_compacted(
    old_content: &str,
    new_content: &str,
    algorithm: Algorithm,
    indent_heuristic: bool,
) -> Vec<DiffOp> {
    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_lines(old_content, new_content);
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let raw = diff.ops().to_vec();
    apply_change_compact_to_ops(&raw, &old_lines, &new_lines, indent_heuristic)
}

/// Like [`diff_lines_ops_compacted`] but for pre-split line slices (e.g. `--no-index` compare keys).
pub fn diff_slice_ops_compacted(
    old_lines: &[&str],
    new_lines: &[&str],
    algorithm: Algorithm,
    indent_heuristic: bool,
) -> Vec<DiffOp> {
    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_slices(old_lines, new_lines);
    let raw = diff.ops().to_vec();
    apply_change_compact_to_ops(&raw, old_lines, new_lines, indent_heuristic)
}

/// Map each line in `new` to its origin line in `old` (if any), matching [`similar::TextDiff::iter_all_changes`]
/// semantics over `ops` (typically compacted ops).
pub(crate) fn map_new_to_old_from_ops(ops: &[DiffOp], new_line_count: usize) -> Vec<Option<usize>> {
    let mut result = vec![None; new_line_count];
    let mut old_idx: usize = 0;
    let mut new_idx: usize = 0;

    for op in ops {
        match op.tag() {
            DiffTag::Equal => {
                let len = op.new_range().len();
                for k in 0..len {
                    if new_idx + k < result.len() {
                        result[new_idx + k] = Some(old_idx + k);
                    }
                }
                old_idx += len;
                new_idx += len;
            }
            DiffTag::Delete => {
                old_idx += op.old_range().len();
            }
            DiffTag::Insert => {
                new_idx += op.new_range().len();
            }
            DiffTag::Replace => {
                old_idx += op.old_range().len();
                new_idx += op.new_range().len();
            }
        }
    }

    result
}
