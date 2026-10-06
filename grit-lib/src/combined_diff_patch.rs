//! Git-style combined merge diff hunks (`diff --cc` / `diff --combined`).
//!
//! This approximates Git's `combine-diff.c` behaviour: per-parent Myers diffs against
//! the merge result, lost-line lists with LCS coalescing, dense (`--cc`) hunk
//! suppression, and `@@@` headers.

use similar::{capture_diff_slices, Algorithm, DiffOp};

/// Whitespace handling for combined diffs (Git `xdl_opts` subset).
#[derive(Debug, Clone, Copy, Default)]
pub struct CombinedDiffWsOptions {
    pub ignore_all_space: bool,
    pub ignore_space_change: bool,
    pub ignore_space_at_eol: bool,
    pub ignore_cr_at_eol: bool,
}

impl CombinedDiffWsOptions {
    #[must_use]
    pub fn any(self) -> bool {
        self.ignore_all_space
            || self.ignore_space_change
            || self.ignore_space_at_eol
            || self.ignore_cr_at_eol
    }
}

#[derive(Clone)]
struct LostSeg {
    text: String,
    parent_map: u32,
}

fn strip_trailing_cr(s: &str, ignore: bool) -> &str {
    if ignore && s.ends_with('\r') {
        &s[..s.len().saturating_sub(1)]
    } else {
        s
    }
}

fn line_key(line: &str, ws: CombinedDiffWsOptions) -> String {
    let s = strip_trailing_cr(line, ws.ignore_cr_at_eol);
    if ws.ignore_all_space {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    } else if ws.ignore_space_change {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    } else if ws.ignore_space_at_eol {
        s.trim_end_matches(|c: char| c.is_whitespace()).to_string()
    } else {
        s.to_string()
    }
}

fn lines_match(a: &str, b: &str, ws: CombinedDiffWsOptions) -> bool {
    line_key(a, ws) == line_key(b, ws)
}

/// Coalesce `incoming` lost segments into `base` using LCS (Git `coalesce_lines`).
fn coalesce_lost(
    base: Vec<LostSeg>,
    incoming: Vec<LostSeg>,
    parent_bit: u32,
    ws: CombinedDiffWsOptions,
) -> Vec<LostSeg> {
    if incoming.is_empty() {
        return base;
    }
    if base.is_empty() {
        return incoming;
    }
    let ob = base.len();
    let nw = incoming.len();
    #[derive(Clone, Copy)]
    enum Dir {
        Match,
        Base,
        New,
    }
    let mut lcs = vec![vec![0usize; nw + 1]; ob + 1];
    let mut dir = vec![vec![Dir::Base; nw + 1]; ob + 1];
    for item in dir[0].iter_mut().skip(1) {
        *item = Dir::New;
    }
    for row in dir.iter_mut().skip(1) {
        row[0] = Dir::Base;
    }
    for i in 1..=ob {
        for j in 1..=nw {
            if lines_match(&base[i - 1].text, &incoming[j - 1].text, ws) {
                lcs[i][j] = lcs[i - 1][j - 1] + 1;
                dir[i][j] = Dir::Match;
            } else if lcs[i][j - 1] >= lcs[i - 1][j] {
                lcs[i][j] = lcs[i][j - 1];
                dir[i][j] = Dir::New;
            } else {
                lcs[i][j] = lcs[i - 1][j];
                dir[i][j] = Dir::Base;
            }
        }
    }
    let mut out: Vec<LostSeg> = Vec::new();
    let mut i = ob;
    let mut j = nw;
    while i > 0 || j > 0 {
        match dir[i][j] {
            Dir::Match => {
                let mut seg = base[i - 1].clone();
                seg.parent_map |= parent_bit;
                out.push(seg);
                i -= 1;
                j -= 1;
            }
            Dir::New => {
                out.push(incoming[j - 1].clone());
                j -= 1;
            }
            Dir::Base => {
                out.push(base[i - 1].clone());
                i -= 1;
            }
        }
    }
    out.reverse();
    out
}

struct Sline {
    /// Lost lines shown before this result line (`-` / `--` rows).
    lost: Vec<LostSeg>,
    /// Pending lost lines for this result line before coalescing.
    plost: Vec<LostSeg>,
    /// Bitmask: parent P had this result line unchanged.
    flag: u32,
    bol: String,
    p_lno: Vec<u32>,
}

fn split_lines_with_incomplete(text: &str) -> (Vec<String>, usize) {
    if text.is_empty() {
        return (Vec::new(), 0);
    }
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let cnt = lines.len();
    (lines, cnt)
}

fn combine_one_parent(
    slines: &mut [Sline],
    cnt: usize,
    parent_lines: &[String],
    n: usize,
    _num_parent: usize,
    ws: CombinedDiffWsOptions,
) {
    let nmask = 1u32 << n;
    let old_keys: Vec<String> = parent_lines.iter().map(|l| line_key(l, ws)).collect();
    let new_keys: Vec<String> = slines[..cnt].iter().map(|s| line_key(&s.bol, ws)).collect();
    let ops = capture_diff_slices(Algorithm::Myers, &old_keys, &new_keys);

    // Group consecutive non-Equal ops into hunks, mirroring xdiff's hunk emission.
    // Within each hunk we replicate combine-diff.c's consume_hunk()/consume_line():
    // the per-hunk "lost bucket" is chosen from the result-side hunk start, and all
    // '-' lines of the hunk are appended to that one bucket in order.
    let mut idx = 0usize;
    while idx < ops.len() {
        if matches!(ops[idx], DiffOp::Equal { .. }) {
            idx += 1;
            continue;
        }
        // Collect a maximal run of change ops as one hunk.
        let start = idx;
        let mut del_lines: Vec<usize> = Vec::new();
        let mut ins_lines: Vec<usize> = Vec::new();
        // new-begin (nb) and new-count (nn): result-side coordinates of the hunk.
        let mut nb: Option<usize> = None;
        let mut nn = 0usize;
        while idx < ops.len() && !matches!(ops[idx], DiffOp::Equal { .. }) {
            match ops[idx] {
                DiffOp::Delete {
                    old_index,
                    old_len,
                    new_index,
                    ..
                } => {
                    if nb.is_none() {
                        nb = Some(new_index);
                    }
                    for k in 0..old_len {
                        del_lines.push(old_index + k);
                    }
                }
                DiffOp::Insert {
                    old_index: _,
                    new_index,
                    new_len,
                } => {
                    if nb.is_none() {
                        nb = Some(new_index);
                    }
                    for k in 0..new_len {
                        ins_lines.push(new_index + k);
                        nn += 1;
                    }
                }
                DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => {
                    if nb.is_none() {
                        nb = Some(new_index);
                    }
                    for k in 0..old_len {
                        del_lines.push(old_index + k);
                    }
                    for k in 0..new_len {
                        ins_lines.push(new_index + k);
                        nn += 1;
                    }
                }
                DiffOp::Equal { .. } => unreachable!(),
            }
            idx += 1;
        }
        let _ = start;
        // nb here is 0-based (result index where the hunk's first change sits).
        // Git's consume_hunk uses 1-based nb: bucket = sline[nb-1] normally, or
        // sline[nb] when the hunk adds nothing to the result (nn == 0). With our
        // 0-based nb, that translates to: bucket = nb (nn == 0) or nb (since
        // sline[(nb+1)-1] == sline[nb]). Concretely git's nb_1based = nb0 + 1.
        let nb0 = nb.unwrap_or(0);
        let bucket = if nn == 0 {
            // Lost lines hang on the result line *after* the removed span.
            (nb0).min(cnt)
        } else {
            // Lost lines hang on the first result line of the hunk.
            nb0.min(cnt)
        };
        let bucket = bucket.min(cnt);
        for &oi in &del_lines {
            slines[bucket].plost.push(LostSeg {
                text: parent_lines[oi].clone(),
                parent_map: nmask,
            });
        }
        for &ni in &ins_lines {
            if ni < cnt {
                slines[ni].flag |= nmask;
            }
        }
    }

    let mut p_lno = 1u32;
    for (lno, sline) in slines.iter_mut().take(cnt + 1).enumerate() {
        sline.p_lno[n] = p_lno;
        if !sline.plost.is_empty() {
            let incoming = std::mem::take(&mut sline.plost);
            sline.lost = coalesce_lost(std::mem::take(&mut sline.lost), incoming, nmask, ws);
        }
        for seg in &sline.lost {
            if seg.parent_map & nmask != 0 {
                p_lno = p_lno.saturating_add(1);
            }
        }
        if lno < cnt && sline.flag & nmask == 0 {
            p_lno = p_lno.saturating_add(1);
        }
    }
    // Trailer: p_lno[cnt + 1] is the end line number, read when a hunk extends to
    // the end of the file (matches Git's `sline[cnt + 1].p_lno[n] = p_lno`).
    if let Some(trailer) = slines.get_mut(cnt + 1) {
        trailer.p_lno[n] = p_lno;
    }
}

fn interesting(s: &Sline, all_mask: u32) -> bool {
    (s.flag & all_mask) != 0 || !s.lost.is_empty()
}

fn find_next(slines: &[Sline], mark: u32, mut i: usize, cnt: usize, want_unmarked: bool) -> usize {
    while i <= cnt {
        let marked = slines[i].flag & mark != 0;
        if want_unmarked != marked {
            return i;
        }
        i += 1;
    }
    cnt + 1
}

fn adjust_hunk_tail(slines: &[Sline], all_mask: u32, hunk_begin: usize, mut i: usize) -> usize {
    if hunk_begin < i && slines[i - 1].flag & all_mask == 0 {
        i -= 1;
    }
    i
}

fn give_context(slines: &mut [Sline], cnt: usize, num_parent: usize, context: usize) {
    let all_mask = (1u32 << num_parent) - 1;
    let mark = 1u32 << num_parent;
    let no_pre_delete = 2u32 << num_parent;

    let mut i = find_next(slines, mark, 0, cnt, false);
    if cnt < i {
        return;
    }

    while i <= cnt {
        let mut j = i.saturating_sub(context);
        while j < i {
            if slines[j].flag & mark == 0 {
                slines[j].flag |= no_pre_delete;
            }
            slines[j].flag |= mark;
            j += 1;
        }

        loop {
            j = find_next(slines, mark, i, cnt, true);
            if cnt < j {
                return;
            }
            let k = find_next(slines, mark, j, cnt, false);
            let j_adj = adjust_hunk_tail(slines, all_mask, i, j);

            if k < j_adj + context {
                let mut t = j;
                while t < k {
                    slines[t].flag |= mark;
                    t += 1;
                }
                i = k;
                continue;
            }

            i = k;
            let k_end = (j + context).min(cnt + 1);
            let mut t = j;
            while t < k_end {
                slines[t].flag |= mark;
                t += 1;
            }
            break;
        }
    }
}

fn make_hunks(slines: &mut [Sline], cnt: usize, num_parent: usize, dense: bool, context: usize) {
    let all_mask = (1u32 << num_parent) - 1;
    let mark = 1u32 << num_parent;

    for sline in slines.iter_mut().take(cnt + 1) {
        if interesting(sline, all_mask) {
            sline.flag |= mark;
        } else {
            sline.flag &= !mark;
        }
    }

    if dense {
        let mut i = 0usize;
        while i <= cnt {
            while i <= cnt && slines[i].flag & mark == 0 {
                i += 1;
            }
            if cnt < i {
                break;
            }
            let hunk_begin = i;
            let mut j = i + 1;
            while j <= cnt {
                if slines[j].flag & mark == 0 {
                    let j_adj = adjust_hunk_tail(slines, all_mask, hunk_begin, j);
                    let la = (j_adj + context).min(cnt + 1);
                    let mut contin = false;
                    let mut la2 = la;
                    while la2 > j {
                        la2 -= 1;
                        if slines[la2].flag & mark != 0 {
                            contin = true;
                            break;
                        }
                    }
                    if !contin {
                        break;
                    }
                    j = la2;
                }
                j += 1;
            }
            let hunk_end = j;

            let mut same_diff = 0u32;
            let mut has_interesting = false;
            let mut jj = hunk_begin;
            while jj < hunk_end && !has_interesting {
                let mut this_diff = slines[jj].flag & all_mask;
                if this_diff != 0 {
                    if same_diff == 0 {
                        same_diff = this_diff;
                    } else if same_diff != this_diff {
                        has_interesting = true;
                        break;
                    }
                }
                let ll_iter = slines[jj].lost.iter();
                for seg in ll_iter {
                    if has_interesting {
                        break;
                    }
                    this_diff = seg.parent_map;
                    if same_diff == 0 {
                        same_diff = this_diff;
                    } else if same_diff != this_diff {
                        has_interesting = true;
                    }
                }
                jj += 1;
            }
            if !has_interesting && same_diff != 0 && same_diff != all_mask {
                for sline in slines.iter_mut().take(hunk_end).skip(hunk_begin) {
                    sline.flag &= !mark;
                }
            }
            i = hunk_end;
        }
    }

    give_context(slines, cnt, num_parent, context);
}

fn show_parent_lno(slines: &[Sline], l0: usize, l1: usize, n: usize, null_ctx: u32) -> String {
    let a = slines[l0].p_lno[n];
    let b = slines[l1].p_lno[n];
    format!(" -{a},{}", b.saturating_sub(a).saturating_sub(null_ctx))
}

fn dump_slines(slines: &[Sline], cnt: usize, num_parent: usize, context: usize) -> String {
    let mark = 1u32 << num_parent;
    let no_pre_delete = 2u32 << num_parent;
    let mut out = String::new();
    let mut lno = 0usize;

    loop {
        while lno <= cnt && slines[lno].flag & mark == 0 {
            lno += 1;
        }
        if cnt < lno {
            break;
        }
        let h_start = lno;
        let mut h_end = lno + 1;
        while h_end <= cnt && slines[h_end].flag & mark != 0 {
            h_end += 1;
        }
        let mut rlines = h_end - h_start;
        if cnt < h_end {
            rlines = rlines.saturating_sub(1);
        }
        let mut null_ctx = 0u32;
        if context == 0 {
            for sl in slines.iter().take(h_end).skip(h_start) {
                if sl.flag & (mark - 1) == 0 {
                    null_ctx = null_ctx.saturating_add(1);
                }
            }
            rlines = rlines.saturating_sub(null_ctx as usize);
        }

        // Git emits `num_parent + 1` `@` characters on each side of a combined hunk
        // header (e.g. `@@@` for a two-parent merge).
        for _ in 0..=num_parent {
            out.push('@');
        }
        for n in 0..num_parent {
            out.push_str(&show_parent_lno(slines, h_start, h_end, n, null_ctx));
        }
        out.push_str(&format!(" +{},{} ", h_start + 1, rlines));
        for _ in 0..=num_parent {
            out.push('@');
        }
        out.push('\n');

        while lno < h_end {
            let sl = &slines[lno];
            lno += 1;
            let show_lost = if sl.flag & no_pre_delete == 0 {
                sl.lost.as_slice()
            } else {
                &[][..]
            };
            // Each combined line is prefixed with exactly `num_parent` columns
            // (one per parent), like Git's dump_sline(); there is no extra leading
            // result-marker column.
            for seg in show_lost {
                for j in 0..num_parent {
                    if seg.parent_map & (1u32 << j) != 0 {
                        out.push('-');
                    } else {
                        out.push(' ');
                    }
                }
                out.push_str(&seg.text);
                out.push('\n');
            }
            if cnt < lno {
                break;
            }
            let sl = &slines[lno - 1];
            if sl.flag & (mark - 1) == 0 && context == 0 {
                continue;
            }
            let mut p_mask = 1u32;
            for _ in 0..num_parent {
                if p_mask & sl.flag != 0 {
                    out.push('+');
                } else {
                    out.push(' ');
                }
                p_mask <<= 1;
            }
            out.push_str(&sl.bol);
            out.push('\n');
        }
    }
    out
}

fn reuse_parent(slines: &mut [Sline], cnt: usize, i: usize, j: usize) {
    let im = 1u32 << i;
    let jm = 1u32 << j;
    for sline in slines.iter_mut().take(cnt + 1) {
        for seg in &mut sline.lost {
            if seg.parent_map & jm != 0 {
                seg.parent_map |= im;
            }
        }
        if sline.flag & jm != 0 {
            sline.flag |= im;
        }
        sline.p_lno[i] = sline.p_lno[j];
    }
    // Mirror the trailer (sline[cnt + 1]) so an EOF-spanning hunk reports the right
    // end line number for the reused parent.
    if let Some(trailer) = slines.get_mut(cnt + 1) {
        trailer.p_lno[i] = trailer.p_lno[j];
    }
}

/// Combined diff body: `@@@` hunks and parent/result lines (no `diff --cc` header).
#[must_use]
pub fn format_combined_diff_body(
    parent_texts: &[String],
    result_text: &str,
    context: usize,
    dense: bool,
    ws: CombinedDiffWsOptions,
) -> String {
    let num_parent = parent_texts.len();
    if num_parent == 0 {
        return String::new();
    }

    let (res_lines, cnt) = split_lines_with_incomplete(result_text);
    // An empty result with non-empty parents still produces a combined diff that
    // shows every parent line as a deletion (e.g. a merge that empties a file).
    // Only bail out when there is genuinely nothing on either side.
    if cnt == 0 && result_text.is_empty() && parent_texts.iter().all(String::is_empty) {
        return String::new();
    }

    let mut slines: Vec<Sline> = (0..=cnt + 1)
        .map(|idx| Sline {
            lost: Vec::new(),
            plost: Vec::new(),
            flag: 0,
            bol: if idx < cnt {
                res_lines[idx].clone()
            } else {
                String::new()
            },
            p_lno: vec![0; num_parent],
        })
        .collect();

    let parents: Vec<Vec<String>> = parent_texts
        .iter()
        .map(|p| p.lines().map(str::to_owned).collect())
        .collect();

    for i in 0..num_parent {
        let mut reused = false;
        for j in 0..i {
            if parents[i] == parents[j] {
                reuse_parent(&mut slines, cnt, i, j);
                reused = true;
                break;
            }
        }
        if !reused {
            combine_one_parent(&mut slines, cnt, &parents[i], i, num_parent, ws);
        }
    }

    make_hunks(&mut slines, cnt, num_parent, dense, context);
    dump_slines(&slines, cnt, num_parent, context)
}
