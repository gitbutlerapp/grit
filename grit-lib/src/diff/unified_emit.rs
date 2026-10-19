//! Histogram unified-diff emission with ignorable-change and function-context rules.

use crate::userdiff::FuncnameMatcher;

use super::hunk_groups::{hunk_edit_spans, LineEdit};

fn is_func_line(line: &str, matcher: Option<&FuncnameMatcher>) -> bool {
    if let Some(m) = matcher {
        return m.match_line(line).is_some();
    }
    let t = line.trim_end_matches(['\n', '\r']);
    if t.is_empty() {
        return false;
    }
    let b = t.as_bytes()[0];
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn push_line(out: &mut String, prefix: char, text: &str) {
    out.push(prefix);
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
}

fn fmt_hunk_side(start: i64, count: i64) -> String {
    if count == 1 {
        format!("{start}")
    } else {
        format!("{start},{count}")
    }
}

/// Build unified-diff body (no `---`/`+++`) from histogram diff with Git `-I` / blank-line ignore.
pub(crate) fn histogram_unified_body_ignore_fc<F>(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    function_context: bool,
    funcname_matcher: Option<&FuncnameMatcher>,
    is_ignorable_change: F,
) -> String
where
    F: Fn(&[&str], &[&str]) -> bool,
{
    use imara_diff::{Algorithm, Diff, InternedInput};
    use std::fmt::Write as _;

    let input = InternedInput::new(old_content, new_content);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let raw_hunks = diff.hunks().collect::<Vec<_>>();
    if raw_hunks.is_empty() {
        return String::new();
    }

    let before_len = input.before.len() as i64;
    let after_len = input.after.len() as i64;
    let ctxlen = context_lines as i64;
    let fuse_limit = ctxlen
        .saturating_add(ctxlen)
        .saturating_add(inter_hunk_context as i64);
    let nearby_limit = ctxlen;

    let mut edits: Vec<LineEdit> = Vec::with_capacity(raw_hunks.len());
    for h in &raw_hunks {
        let removed: Vec<&str> = input.before[h.before.start as usize..h.before.end as usize]
            .iter()
            .map(|&t| input.interner[t])
            .collect();
        let added: Vec<&str> = input.after[h.after.start as usize..h.after.end as usize]
            .iter()
            .map(|&t| input.interner[t])
            .collect();
        edits.push(LineEdit {
            old_at: h.before.start as i64,
            old_run: (h.before.end - h.before.start) as i64,
            new_at: h.after.start as i64,
            new_run: (h.after.end - h.after.start) as i64,
            ignorable: is_ignorable_change(&removed, &added),
        });
    }

    let before_line = |idx: i64| -> &str { input.interner[input.before[idx as usize]] };
    let after_line = |idx: i64| -> &str { input.interner[input.after[idx as usize]] };

    let spans = hunk_edit_spans(&edits, fuse_limit, nearby_limit);
    let mut out = String::new();

    for (head, tail) in spans {
        let first = &edits[head];
        let last = &edits[tail];

        let mut s1 = (first.old_at - ctxlen).max(0);
        let mut s2 = (first.new_at - ctxlen).max(0);
        let mut lctx = ctxlen;
        lctx = lctx.min(before_len - (last.old_at + last.old_run));
        lctx = lctx.min(after_len - (last.new_at + last.new_run));
        let mut e1 = last.old_at + last.old_run + lctx;
        let mut e2 = last.new_at + last.new_run + lctx;

        if function_context {
            let mut fs1 = first.old_at;
            while fs1 >= 0 && !is_func_line(before_line(fs1), funcname_matcher) {
                fs1 -= 1;
            }
            while fs1 > 0
                && !before_line(fs1 - 1).trim().is_empty()
                && !is_func_line(before_line(fs1 - 1), funcname_matcher)
            {
                fs1 -= 1;
            }
            if fs1 < 0 {
                fs1 = 0;
            }
            if fs1 < s1 {
                s2 = (s2 - (s1 - fs1)).max(0);
                s1 = fs1;
            }

            let end1 = last.old_at + last.old_run;
            let mut fe1 = end1;
            while fe1 < before_len && !is_func_line(before_line(fe1), funcname_matcher) {
                fe1 += 1;
            }
            while fe1 > 0 && before_line(fe1 - 1).trim().is_empty() {
                fe1 -= 1;
            }
            if fe1 > before_len {
                fe1 = before_len;
            }
            if fe1 > e1 {
                e2 = (e2 + (fe1 - e1)).min(after_len);
                e1 = fe1;
            }
        }

        let _ = writeln!(
            out,
            "@@ -{} +{} @@",
            fmt_hunk_side(s1 + 1, e1 - s1),
            fmt_hunk_side(s2 + 1, e2 - s2)
        );

        let mut s2c = s2;
        while s2c < first.new_at {
            push_line(&mut out, ' ', after_line(s2c));
            s2c += 1;
        }

        let mut s1c = first.old_at;
        let mut s2c = first.new_at;
        for c in edits.iter().take(tail + 1).skip(head) {
            while s1c < c.old_at && s2c < c.new_at {
                push_line(&mut out, ' ', after_line(s2c));
                s1c += 1;
                s2c += 1;
            }
            let mut r = c.old_at;
            while r < c.old_at + c.old_run {
                push_line(&mut out, '-', before_line(r));
                r += 1;
            }
            let mut a = c.new_at;
            while a < c.new_at + c.new_run {
                push_line(&mut out, '+', after_line(a));
                a += 1;
            }
            s1c = c.old_at + c.old_run;
            s2c = c.new_at + c.new_run;
        }

        let mut s2p = last.new_at + last.new_run;
        while s2p < e2 {
            push_line(&mut out, ' ', after_line(s2p));
            s2p += 1;
        }
    }

    out
}
