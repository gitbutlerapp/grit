//! Diff machinery — compare trees, index entries, and working tree files.
//!
//! # Overview
//!
//! This module provides the core diffing infrastructure shared by `diff`,
//! `diff-index`, `status`, `log`, `show`, `commit`, and `merge`.
//!
//! ## Levels of comparison
//!
//! 1. **Tree-to-tree** — compare two tree objects (e.g. for `log`/`show`).
//! 2. **Tree-to-index** — compare a tree (usually HEAD) against the index
//!    (staged changes, used by `diff --cached` and `status`).
//! 3. **Index-to-worktree** — compare index against the working directory
//!    (unstaged changes, used by `diff` and `status`).
//!
//! ## Content diff
//!
//! Line-level diffing uses the `similar` crate (Myers, patience, minimal) and,
//! for Git's `histogram` algorithm, `imara-diff` for output compatible with upstream Git.
//! Output formats: unified patch, raw (`:old-mode new-mode ...`), stat,
//! numstat.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::diff_indent_heuristic;
use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry};
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;
use crate::userdiff::FuncnameMatcher;

/// Splits imara-diff unified body (concatenated hunks) into per-hunk slices for post-processing.
fn imara_unified_hunk_slices(body: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    if body.starts_with("@@") {
        starts.push(0);
    }
    for (idx, _) in body.match_indices("\n@@ ") {
        starts.push(idx + 1);
    }
    starts.push(body.len());
    starts.windows(2).map(|w| &body[w[0]..w[1]]).collect()
}

fn histogram_unified_body_raw(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
) -> String {
    use imara_diff::{Algorithm, Diff, Hunk, InternedInput};
    use std::fmt::Write as _;

    let input = InternedInput::new(old_content, new_content);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    // Assemble hunks ourselves: imara's `UnifiedDiff` printer starts the first
    // hunk's context at line 0 whenever the first change is within
    // `2 * context_len` of the file start, emitting more leading context than
    // its own header claims (t4061), and its gap threshold cannot express
    // Git's odd `2 * U + inter_hunk_context` fuse limits (t4032).
    let hunks: Vec<Hunk> = diff.hunks().collect();
    if hunks.is_empty() {
        return String::new();
    }

    let ctx = context_lines.min(u32::MAX as usize) as u32;
    let max_gap = (2usize.saturating_mul(context_lines))
        .saturating_add(inter_hunk_context)
        .min(u32::MAX as usize) as u32;
    let before_len = input.before.len() as u32;
    let after_len = input.after.len() as u32;

    // Fuse hunks whose unchanged gap is at most `max_gap` (Git xdl_get_hunk).
    let mut groups: Vec<&[Hunk]> = Vec::new();
    let mut group_start = 0usize;
    for i in 1..hunks.len() {
        if hunks[i].before.start - hunks[i - 1].before.end > max_gap {
            groups.push(&hunks[group_start..i]);
            group_start = i;
        }
    }
    groups.push(&hunks[group_start..]);

    fn push_line(out: &mut String, prefix: char, text: &str) {
        out.push(prefix);
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }

    // Git hunk header range: 1-based start (the preceding line when the range
    // is empty) with the `,count` part omitted when the count is exactly 1.
    fn fmt_side(start: u32, count: u32) -> String {
        let shown_start = if count == 0 { start } else { start + 1 };
        if count == 1 {
            format!("{shown_start}")
        } else {
            format!("{shown_start},{count}")
        }
    }

    let mut out = String::new();
    for group in groups {
        let first = &group[0];
        let last = &group[group.len() - 1];
        let b_start = first.before.start.saturating_sub(ctx);
        let a_start = first.after.start.saturating_sub(ctx);
        let b_end = (last.before.end.saturating_add(ctx)).min(before_len);
        let a_end = (last.after.end.saturating_add(ctx)).min(after_len);

        let _ = writeln!(
            out,
            "@@ -{} +{} @@",
            fmt_side(b_start, b_end - b_start),
            fmt_side(a_start, a_end - a_start)
        );

        let mut pos = b_start;
        for hunk in group {
            for &token in &input.before[pos as usize..hunk.before.start as usize] {
                push_line(&mut out, ' ', input.interner[token]);
            }
            for &token in &input.before[hunk.before.start as usize..hunk.before.end as usize] {
                push_line(&mut out, '-', input.interner[token]);
            }
            for &token in &input.after[hunk.after.start as usize..hunk.after.end as usize] {
                push_line(&mut out, '+', input.interner[token]);
            }
            pos = hunk.before.end;
        }
        for &token in &input.before[pos as usize..b_end as usize] {
            push_line(&mut out, ' ', input.interner[token]);
        }
    }

    out
}

/// Build the unified-diff body (no `---`/`+++` header) using Git's histogram
/// algorithm, but applying `--ignore-blank-lines` / `-I` semantics through a
/// Git-compatible implementation of the xdiff change-record machinery
/// (`xdl_mark_ignorable_lines` + `xdl_get_hunk` + `xdl_emit_diff`).
///
/// `is_ignorable_change` is called with the slices of removed and added lines
/// for one change record (each item is the line text without its trailing
/// newline); it returns `true` when the whole change is ignorable (Git's
/// `xch->ignore`). Ignorable changes that are far from substantive changes are
/// dropped, and hunk boundaries are computed with Git's `max_ignorable`
/// distance rules so the line numbers and surviving context match Git exactly.
///
/// Returns the body only; an empty string means no substantive change survives.
/// Optional `--function-context` (`-W`) hunk expansion is applied *after*
/// ignorable change-record pruning so the boundaries/funcname header match Git.
#[allow(clippy::too_many_arguments)]
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
    use imara_diff::{Algorithm, Diff, Hunk, InternedInput};
    use std::fmt::Write as _;

    let input = InternedInput::new(old_content, new_content);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let raw_hunks: Vec<Hunk> = diff.hunks().collect();
    if raw_hunks.is_empty() {
        return String::new();
    }

    let before_len = input.before.len() as i64;
    let after_len = input.after.len() as i64;
    let ctxlen = context_lines as i64;
    let interhunk = inter_hunk_context as i64;
    // Git's xdl_get_hunk fuse limit: 2*ctxlen + interhunkctxlen.
    let max_common = ctxlen.saturating_add(ctxlen).saturating_add(interhunk);
    let max_ignorable = ctxlen;

    // A change record mirrors Git's xdchange_t.
    struct Change {
        i1: i64,
        chg1: i64,
        i2: i64,
        chg2: i64,
        ignore: bool,
    }

    let mut changes: Vec<Change> = Vec::with_capacity(raw_hunks.len());
    for h in &raw_hunks {
        let i1 = h.before.start as i64;
        let chg1 = (h.before.end - h.before.start) as i64;
        let i2 = h.after.start as i64;
        let chg2 = (h.after.end - h.after.start) as i64;
        let removed: Vec<&str> = input.before[h.before.start as usize..h.before.end as usize]
            .iter()
            .map(|&t| input.interner[t])
            .collect();
        let added: Vec<&str> = input.after[h.after.start as usize..h.after.end as usize]
            .iter()
            .map(|&t| input.interner[t])
            .collect();
        let ignore = is_ignorable_change(&removed, &added);
        changes.push(Change {
            i1,
            chg1,
            i2,
            chg2,
            ignore,
        });
    }

    // Helper to fetch a line of either side (token interner over lines).
    let before_line = |idx: i64| -> &str { input.interner[input.before[idx as usize]] };
    let after_line = |idx: i64| -> &str { input.interner[input.after[idx as usize]] };

    // Port of xdl_get_hunk: starting at change index `start`, return
    // `(new_start, last)` — the possibly-advanced first change index (leading
    // ignorable changes dropped) and the index of the last change to include in
    // this hunk — or `None` when the leading ignorable changes are all dropped
    // and nothing remains.
    let get_hunk = |start: usize| -> Option<(usize, usize)> {
        // Remove ignorable changes that are too far before other changes.
        // Faithful port of xdl_get_hunk's leading loop: walk every leading
        // ignorable change; whenever its successor is absent or far enough away
        // (>= max_ignorable), advance the hunk start past it. `scr` is the new
        // start index, or `changes.len()` when everything is dropped.
        let mut scr = start;
        let mut xchp = start;
        while xchp < changes.len() && changes[xchp].ignore {
            let next = xchp + 1;
            if next >= changes.len()
                || changes[next].i1 - (changes[xchp].i1 + changes[xchp].chg1) >= max_ignorable
            {
                scr = next;
            }
            xchp = next;
        }
        if scr >= changes.len() {
            return None;
        }

        let mut lxch = scr;
        let mut ignored: i64 = 0;
        let mut xchp = scr;
        let mut idx = scr + 1;
        while idx < changes.len() {
            let xch = &changes[idx];
            let prev = &changes[xchp];
            let distance = xch.i1 - (prev.i1 + prev.chg1);
            if distance > max_common {
                break;
            }
            if distance < max_ignorable && (!xch.ignore || lxch == xchp) {
                lxch = idx;
                ignored = 0;
            } else if distance < max_ignorable && xch.ignore {
                ignored += xch.chg2;
            } else if lxch != xchp
                && xch.i1 + ignored - (changes[lxch].i1 + changes[lxch].chg1) > max_common
            {
                break;
            } else if !xch.ignore {
                lxch = idx;
                ignored = 0;
            } else {
                ignored += xch.chg2;
            }
            xchp = idx;
            idx += 1;
        }
        Some((scr, lxch))
    };

    fn push_line(out: &mut String, prefix: char, text: &str) {
        out.push(prefix);
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }

    // Git hunk header range (xdl_emit_hunk_hdr): `,count` part omitted when 1.
    fn fmt_side(start: i64, count: i64) -> String {
        if count == 1 {
            format!("{start}")
        } else {
            format!("{start},{count}")
        }
    }

    let mut out = String::new();
    let mut cursor = 0usize;
    while cursor < changes.len() {
        let Some((xch_idx, xche_idx)) = get_hunk(cursor) else {
            break;
        };
        let xch = &changes[xch_idx];
        let xche = &changes[xche_idx];

        // pre-context
        let mut s1 = (xch.i1 - ctxlen).max(0);
        let mut s2 = (xch.i2 - ctxlen).max(0);
        // post-context (clamped so it does not run past either side's EOF).
        let mut lctx = ctxlen;
        lctx = lctx.min(before_len - (xche.i1 + xche.chg1));
        lctx = lctx.min(after_len - (xche.i2 + xche.chg2));
        let mut e1 = xche.i1 + xche.chg1 + lctx;
        let mut e2 = xche.i2 + xche.chg2 + lctx;

        // `--function-context`: expand the hunk up to the enclosing function's
        // header and down to the line before the next function (Git's
        // XDL_EMIT_FUNCCONTEXT in xemit.c), using the already ignore-pruned
        // change boundaries.
        if function_context {
            // Upward: find the function line at or before xch.i1, then climb to
            // the start of that function (past non-empty non-func lines).
            let mut fs1 = xch.i1;
            while fs1 >= 0 && !is_func_line(before_line(fs1), funcname_matcher) {
                fs1 -= 1;
            }
            while fs1 > 0
                && before_line(fs1 - 1).trim().is_empty() == false
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
            // Downward: find the next function line after the hunk end, climb up
            // past trailing empty lines, and extend e1/e2 to it.
            let end1 = xche.i1 + xche.chg1;
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
            fmt_side(s1 + 1, e1 - s1),
            fmt_side(s2 + 1, e2 - s2)
        );

        // Emit pre-context from the new side.
        let mut s2c = s2;
        while s2c < xch.i2 {
            push_line(&mut out, ' ', after_line(s2c));
            s2c += 1;
        }

        // Emit each change record and the context between merged records.
        let mut s1c = xch.i1;
        let mut s2c = xch.i2;
        let mut k = xch_idx;
        loop {
            let c = &changes[k];
            // Context between previous and current change atom.
            while s1c < c.i1 && s2c < c.i2 {
                push_line(&mut out, ' ', after_line(s2c));
                s1c += 1;
                s2c += 1;
            }
            // Removed lines from old side.
            let mut r = c.i1;
            while r < c.i1 + c.chg1 {
                push_line(&mut out, '-', before_line(r));
                r += 1;
            }
            // Added lines from new side.
            let mut a = c.i2;
            while a < c.i2 + c.chg2 {
                push_line(&mut out, '+', after_line(a));
                a += 1;
            }
            if k == xche_idx {
                break;
            }
            s1c = c.i1 + c.chg1;
            s2c = c.i2 + c.chg2;
            k += 1;
        }

        // Post-context.
        let mut s2p = xche.i2 + xche.chg2;
        while s2p < e2 {
            push_line(&mut out, ' ', after_line(s2p));
            s2p += 1;
        }

        cursor = xche_idx + 1;
    }

    out
}

/// Unified diff hunks for Git's histogram algorithm (no `---` / `+++` lines).
///
/// Used by `--no-index` when whitespace normalization is off so the patch matches upstream Git.
#[must_use]
pub fn unified_diff_histogram_hunks_only(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    inter_hunk_context: usize,
) -> String {
    histogram_unified_body_raw(old_content, new_content, context_lines, inter_hunk_context)
}

/// Full unified diff (`---` / `+++` / hunks) using Git's histogram algorithm.
#[must_use]
pub fn unified_diff_histogram_with_prefix_and_funcname(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;

    let body =
        histogram_unified_body_raw(old_content, new_content, context_lines, inter_hunk_context);

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher)
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(hunk_str);
            }
        } else {
            output.push_str(hunk_str);
        }
    }

    output
}

/// Full unified diff (`---` / `+++` / hunks) using Git's histogram algorithm,
/// applying `--ignore-blank-lines` / `-I` change-record suppression
/// ([`histogram_unified_body_ignore`]) and then attaching function-name hunk
/// headers exactly like [`unified_diff_histogram_with_prefix_and_funcname`].
///
/// `is_ignorable_change` is given the removed/added line slices (without
/// trailing newlines) of one change record and returns whether the whole change
/// is ignorable. Returns an empty string when no substantive change survives so
/// the caller can hide the file entry.
#[allow(clippy::too_many_arguments)]
pub fn unified_diff_histogram_ignore_with_prefix_and_funcname<F>(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    quote_path_fully: bool,
    function_context: bool,
    is_ignorable_change: F,
) -> String
where
    F: Fn(&[&str], &[&str]) -> bool,
{
    use crate::quote_path::format_diff_path_with_prefix;

    let body = histogram_unified_body_ignore_fc(
        old_content,
        new_content,
        context_lines,
        inter_hunk_context,
        function_context,
        funcname_matcher,
        is_ignorable_change,
    );
    if body.is_empty() {
        return String::new();
    }

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();
    for hunk_str in imara_unified_hunk_slices(&body) {
        if hunk_str.is_empty() {
            continue;
        }
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher)
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(hunk_str);
            }
        } else {
            output.push_str(hunk_str);
        }
    }

    output
}

/// `diff.indentHeuristic` from config (Git defaults to true when unset).
#[must_use]
pub fn indent_heuristic_from_config(config: &ConfigSet) -> bool {
    match config.get_bool("diff.indentHeuristic") {
        Some(Ok(b)) => b,
        Some(Err(_)) | None => true,
    }
}

/// Resolve indent heuristic: `--no-indent-heuristic` and `--indent-heuristic` override config.
#[must_use]
pub fn resolve_indent_heuristic(
    config: &ConfigSet,
    cli_indent_heuristic: bool,
    cli_no_indent_heuristic: bool,
) -> bool {
    if cli_no_indent_heuristic {
        false
    } else if cli_indent_heuristic {
        true
    } else {
        indent_heuristic_from_config(config)
    }
}

/// Parse `--indent-heuristic` / `--no-indent-heuristic` from a plumbing argv slice (last occurrence wins).
#[must_use]
pub fn parse_indent_heuristic_cli_flags(argv: &[String]) -> (bool, bool) {
    let mut indent_heuristic = false;
    let mut no_indent_heuristic = false;
    for a in argv {
        match a.as_str() {
            "--indent-heuristic" => {
                indent_heuristic = true;
                no_indent_heuristic = false;
            }
            "--no-indent-heuristic" => {
                no_indent_heuristic = true;
                indent_heuristic = false;
            }
            _ => {}
        }
    }
    (indent_heuristic, no_indent_heuristic)
}

// ---------------------------------------------------------------------------
// Git-compatible implementation of the xdiff Myers engine,
// restricted to what the word-diff path needs (no rename/ignore-whitespace
// flags). Git's word diff runs the *default* xdiff Myers over the per-word
// token streams; matching Git's exact change-record selection requires
// reproducing its preprocessing (record classification + `xdl_cleanup_records`
// + `xdl_trim_ends`) and its divide-and-conquer split, because `imara-diff`
// (and `similar`) pick different — but equally minimal — alignments for inputs
// with many repeated tokens (e.g. the `{`/`}`/`,` in the `bibtex` driver).
mod git_xdiff {
    const XDL_MAX_COST_MIN: i64 = 256;
    const XDL_HEUR_MIN_COST: i64 = 256;
    const XDL_SNAKE_CNT: i64 = 20;
    const XDL_K_HEUR: i64 = 4;
    const XDL_LINE_MAX: i64 = i64::MAX;
    const XDL_KPDIS_RUN: i64 = 4;
    const XDL_MAX_EQLIMIT: i64 = 1024;
    const XDL_SIMSCAN_WINDOW: i64 = 100;

    // record-classification actions (xprepare.c)
    const DISCARD: u8 = 0;
    const KEEP: u8 = 1;
    const INVESTIGATE: u8 = 2;

    /// Classical integer square root approximation using shifts (`xdl_bogosqrt`).
    fn bogosqrt(mut n: i64) -> i64 {
        let mut i: i64 = 1;
        while n > 0 {
            i <<= 1;
            n >>= 2;
        }
        i
    }

    struct XdAlgoEnv {
        mxcost: i64,
        snake_cnt: i64,
        heur_min: i64,
    }

    struct XdpSplit {
        i1: i64,
        i2: i64,
        min_lo: bool,
        min_hi: bool,
    }

    /// A single side of the diff: token ids plus the `changed` flags and the
    /// `reference_index` mapping from effective record -> original record.
    struct XdFile<'a> {
        ids: &'a [u32],
        changed: Vec<bool>,
        reference_index: Vec<usize>,
        dstart: i64,
        dend: i64,
        nreff: i64,
    }

    /// Hash/id lookup over the *effective* records: index into `reference_index`.
    #[inline]
    fn get_id(xdf: &XdFile<'_>, idx: i64) -> u32 {
        xdf.ids[xdf.reference_index[idx as usize]]
    }

    /// Faithful port of `xdl_split` (the middle-snake finder). `kvdf`/`kvdb` are
    /// the forward/backward k-vectors, offset so index 0 maps to diagonal 0.
    #[allow(clippy::too_many_arguments)]
    fn xdl_split(
        xdf1: &XdFile<'_>,
        off1: i64,
        lim1: i64,
        xdf2: &XdFile<'_>,
        off2: i64,
        lim2: i64,
        kvdf: &mut [i64],
        kvdb: &mut [i64],
        koff: i64,
        need_min: bool,
        spl: &mut XdpSplit,
        xenv: &XdAlgoEnv,
    ) -> i64 {
        let dmin = off1 - lim2;
        let dmax = lim1 - off2;
        let fmid = off1 - off2;
        let bmid = lim1 - lim2;
        let odd = ((fmid - bmid) & 1) != 0;
        let mut fmin = fmid;
        let mut fmax = fmid;
        let mut bmin = bmid;
        let mut bmax = bmid;

        let kf = |k: i64| -> usize { (k + koff) as usize };

        kvdf[kf(fmid)] = off1;
        kvdb[kf(bmid)] = lim1;

        let mut ec: i64 = 1;
        loop {
            let mut got_snake = false;

            if fmin > dmin {
                fmin -= 1;
                kvdf[kf(fmin - 1)] = -1;
            } else {
                fmin += 1;
            }
            if fmax < dmax {
                fmax += 1;
                kvdf[kf(fmax + 1)] = -1;
            } else {
                fmax -= 1;
            }

            let mut d = fmax;
            while d >= fmin {
                let mut i1 = if kvdf[kf(d - 1)] >= kvdf[kf(d + 1)] {
                    kvdf[kf(d - 1)] + 1
                } else {
                    kvdf[kf(d + 1)]
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 < lim1 && i2 < lim2 && get_id(xdf1, i1) == get_id(xdf2, i2) {
                    i1 += 1;
                    i2 += 1;
                }
                if i1 - prev1 > xenv.snake_cnt {
                    got_snake = true;
                }
                kvdf[kf(d)] = i1;
                if odd && bmin <= d && d <= bmax && kvdb[kf(d)] <= i1 {
                    spl.i1 = i1;
                    spl.i2 = i2;
                    spl.min_lo = true;
                    spl.min_hi = true;
                    return ec;
                }
                d -= 2;
            }

            if bmin > dmin {
                bmin -= 1;
                kvdb[kf(bmin - 1)] = XDL_LINE_MAX;
            } else {
                bmin += 1;
            }
            if bmax < dmax {
                bmax += 1;
                kvdb[kf(bmax + 1)] = XDL_LINE_MAX;
            } else {
                bmax -= 1;
            }

            let mut d = bmax;
            while d >= bmin {
                let mut i1 = if kvdb[kf(d - 1)] < kvdb[kf(d + 1)] {
                    kvdb[kf(d - 1)]
                } else {
                    kvdb[kf(d + 1)] - 1
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 > off1 && i2 > off2 && get_id(xdf1, i1 - 1) == get_id(xdf2, i2 - 1) {
                    i1 -= 1;
                    i2 -= 1;
                }
                if prev1 - i1 > xenv.snake_cnt {
                    got_snake = true;
                }
                kvdb[kf(d)] = i1;
                if !odd && fmin <= d && d <= fmax && i1 <= kvdf[kf(d)] {
                    spl.i1 = i1;
                    spl.i2 = i2;
                    spl.min_lo = true;
                    spl.min_hi = true;
                    return ec;
                }
                d -= 2;
            }

            if need_min {
                ec += 1;
                continue;
            }

            if got_snake && ec > xenv.heur_min {
                let mut best = 0i64;
                let mut d = fmax;
                while d >= fmin {
                    let dd = if d > fmid { d - fmid } else { fmid - d };
                    let i1 = kvdf[kf(d)];
                    let i2 = i1 - d;
                    let v = (i1 - off1) + (i2 - off2) - dd;
                    if v > XDL_K_HEUR * ec
                        && v > best
                        && off1 + xenv.snake_cnt <= i1
                        && i1 < lim1
                        && off2 + xenv.snake_cnt <= i2
                        && i2 < lim2
                    {
                        let mut k = 1i64;
                        while get_id(xdf1, i1 - k) == get_id(xdf2, i2 - k) {
                            if k == xenv.snake_cnt {
                                best = v;
                                spl.i1 = i1;
                                spl.i2 = i2;
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if best > 0 {
                    spl.min_lo = true;
                    spl.min_hi = false;
                    return ec;
                }

                let mut best = 0i64;
                let mut d = bmax;
                while d >= bmin {
                    let dd = if d > bmid { d - bmid } else { bmid - d };
                    let i1 = kvdb[kf(d)];
                    let i2 = i1 - d;
                    let v = (lim1 - i1) + (lim2 - i2) - dd;
                    if v > XDL_K_HEUR * ec
                        && v > best
                        && off1 < i1
                        && i1 <= lim1 - xenv.snake_cnt
                        && off2 < i2
                        && i2 <= lim2 - xenv.snake_cnt
                    {
                        let mut k = 0i64;
                        while get_id(xdf1, i1 + k) == get_id(xdf2, i2 + k) {
                            if k == xenv.snake_cnt - 1 {
                                best = v;
                                spl.i1 = i1;
                                spl.i2 = i2;
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if best > 0 {
                    spl.min_lo = false;
                    spl.min_hi = true;
                    return ec;
                }
            }

            if ec >= xenv.mxcost {
                let mut fbest = -1i64;
                let mut fbest1 = -1i64;
                let mut d = fmax;
                while d >= fmin {
                    let mut i1 = kvdf[kf(d)].min(lim1);
                    let mut i2 = i1 - d;
                    if lim2 < i2 {
                        i1 = lim2 + d;
                        i2 = lim2;
                    }
                    if fbest < i1 + i2 {
                        fbest = i1 + i2;
                        fbest1 = i1;
                    }
                    d -= 2;
                }

                let mut bbest = XDL_LINE_MAX;
                let mut bbest1 = XDL_LINE_MAX;
                let mut d = bmax;
                while d >= bmin {
                    let mut i1 = off1.max(kvdb[kf(d)]);
                    let mut i2 = i1 - d;
                    if i2 < off2 {
                        i1 = off2 + d;
                        i2 = off2;
                    }
                    if i1 + i2 < bbest {
                        bbest = i1 + i2;
                        bbest1 = i1;
                    }
                    d -= 2;
                }

                if (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                    spl.i1 = fbest1;
                    spl.i2 = fbest - fbest1;
                    spl.min_lo = true;
                    spl.min_hi = false;
                } else {
                    spl.i1 = bbest1;
                    spl.i2 = bbest - bbest1;
                    spl.min_lo = false;
                    spl.min_hi = true;
                }
                return ec;
            }

            ec += 1;
        }
    }

    /// Faithful port of `xdl_recs_cmp` (divide & conquer). Marks `changed` flags.
    #[allow(clippy::too_many_arguments)]
    fn xdl_recs_cmp(
        xdf1: &mut XdFile<'_>,
        mut off1: i64,
        mut lim1: i64,
        xdf2: &mut XdFile<'_>,
        mut off2: i64,
        mut lim2: i64,
        kvdf: &mut [i64],
        kvdb: &mut [i64],
        koff: i64,
        need_min: bool,
        xenv: &XdAlgoEnv,
    ) {
        while off1 < lim1 && off2 < lim2 && get_id(xdf1, off1) == get_id(xdf2, off2) {
            off1 += 1;
            off2 += 1;
        }
        while off1 < lim1 && off2 < lim2 && get_id(xdf1, lim1 - 1) == get_id(xdf2, lim2 - 1) {
            lim1 -= 1;
            lim2 -= 1;
        }

        if off1 == lim1 {
            while off2 < lim2 {
                let r = xdf2.reference_index[off2 as usize];
                xdf2.changed[r] = true;
                off2 += 1;
            }
        } else if off2 == lim2 {
            while off1 < lim1 {
                let r = xdf1.reference_index[off1 as usize];
                xdf1.changed[r] = true;
                off1 += 1;
            }
        } else {
            let mut spl = XdpSplit {
                i1: 0,
                i2: 0,
                min_lo: false,
                min_hi: false,
            };
            xdl_split(
                xdf1, off1, lim1, xdf2, off2, lim2, kvdf, kvdb, koff, need_min, &mut spl, xenv,
            );
            xdl_recs_cmp(
                xdf1, off1, spl.i1, xdf2, off2, spl.i2, kvdf, kvdb, koff, spl.min_lo, xenv,
            );
            xdl_recs_cmp(
                xdf1, spl.i1, lim1, xdf2, spl.i2, lim2, kvdf, kvdb, koff, spl.min_hi, xenv,
            );
        }
    }

    /// `xdl_clean_mmatch`: decide whether a multimatch record should be discarded.
    fn clean_mmatch(action: &[u8], i: i64, mut s: i64, mut e: i64) -> bool {
        if i - s > XDL_SIMSCAN_WINDOW {
            s = i - XDL_SIMSCAN_WINDOW;
        }
        if e - i > XDL_SIMSCAN_WINDOW {
            e = i + XDL_SIMSCAN_WINDOW;
        }

        let mut rdis0 = 0i64;
        let mut rpdis0 = 1i64;
        let mut r = 1i64;
        while i - r >= s {
            match action[(i - r) as usize] {
                DISCARD => rdis0 += 1,
                INVESTIGATE => rpdis0 += 1,
                _ => break, // KEEP
            }
            r += 1;
        }
        if rdis0 == 0 {
            return false;
        }
        let mut rdis1 = 0i64;
        let mut rpdis1 = 1i64;
        let mut r = 1i64;
        while i + r <= e {
            match action[(i + r) as usize] {
                DISCARD => rdis1 += 1,
                INVESTIGATE => rpdis1 += 1,
                _ => break, // KEEP
            }
            r += 1;
        }
        if rdis1 == 0 {
            return false;
        }
        rdis1 += rdis0;
        rpdis1 += rpdis0;
        rpdis1 * XDL_KPDIS_RUN < (rpdis1 + rdis1)
    }

    /// Build the `changed` flags for both token streams using Git's xdiff Myers
    /// pipeline. `ids1`/`ids2` are interned token ids (equal id <=> equal token).
    /// Returns `(changed1, changed2)`, one bool per original token.
    pub fn changed_flags(ids1: &[u32], ids2: &[u32]) -> (Vec<bool>, Vec<bool>) {
        let nrec1 = ids1.len();
        let nrec2 = ids2.len();

        // Per-token occurrence counts on each side (class len1/len2).
        use std::collections::HashMap;
        let mut count1: HashMap<u32, i64> = HashMap::new();
        let mut count2: HashMap<u32, i64> = HashMap::new();
        for &id in ids1 {
            *count1.entry(id).or_insert(0) += 1;
        }
        for &id in ids2 {
            *count2.entry(id).or_insert(0) += 1;
        }

        let mut xdf1 = XdFile {
            ids: ids1,
            changed: vec![false; nrec1],
            reference_index: Vec::new(),
            dstart: 0,
            dend: nrec1 as i64 - 1,
            nreff: 0,
        };
        let mut xdf2 = XdFile {
            ids: ids2,
            changed: vec![false; nrec2],
            reference_index: Vec::new(),
            dstart: 0,
            dend: nrec2 as i64 - 1,
            nreff: 0,
        };

        // xdl_trim_ends: trim leading/trailing matching records.
        let lim = nrec1.min(nrec2) as i64;
        let mut i = 0i64;
        while i < lim && ids1[i as usize] == ids2[i as usize] {
            i += 1;
        }
        xdf1.dstart = i;
        xdf2.dstart = i;
        let mut j = 0i64;
        let rem = lim - i;
        while j < rem && ids1[nrec1 - 1 - j as usize] == ids2[nrec2 - 1 - j as usize] {
            j += 1;
        }
        xdf1.dend = nrec1 as i64 - j - 1;
        xdf2.dend = nrec2 as i64 - j - 1;

        // xdl_cleanup_records: classify and reduce to effective records.
        let mut action1 = vec![0u8; nrec1 + 1];
        let mut action2 = vec![0u8; nrec2 + 1];

        let mut mlim = bogosqrt(nrec1 as i64);
        if mlim > XDL_MAX_EQLIMIT {
            mlim = XDL_MAX_EQLIMIT;
        }
        let mut idx = xdf1.dstart;
        while idx <= xdf1.dend {
            let id = ids1[idx as usize];
            let nm = *count2.get(&id).unwrap_or(&0);
            action1[idx as usize] = if nm == 0 {
                DISCARD
            } else if nm >= mlim {
                INVESTIGATE
            } else {
                KEEP
            };
            idx += 1;
        }

        let mut mlim = bogosqrt(nrec2 as i64);
        if mlim > XDL_MAX_EQLIMIT {
            mlim = XDL_MAX_EQLIMIT;
        }
        let mut idx = xdf2.dstart;
        while idx <= xdf2.dend {
            let id = ids2[idx as usize];
            let nm = *count1.get(&id).unwrap_or(&0);
            action2[idx as usize] = if nm == 0 {
                DISCARD
            } else if nm >= mlim {
                INVESTIGATE
            } else {
                KEEP
            };
            idx += 1;
        }

        let mut idx = xdf1.dstart;
        while idx <= xdf1.dend {
            let a = action1[idx as usize];
            if a == KEEP
                || (a == INVESTIGATE && !clean_mmatch(&action1, idx, xdf1.dstart, xdf1.dend))
            {
                xdf1.reference_index.push(idx as usize);
            } else {
                xdf1.changed[idx as usize] = true;
            }
            idx += 1;
        }
        xdf1.nreff = xdf1.reference_index.len() as i64;

        let mut idx = xdf2.dstart;
        while idx <= xdf2.dend {
            let a = action2[idx as usize];
            if a == KEEP
                || (a == INVESTIGATE && !clean_mmatch(&action2, idx, xdf2.dstart, xdf2.dend))
            {
                xdf2.reference_index.push(idx as usize);
            } else {
                xdf2.changed[idx as usize] = true;
            }
            idx += 1;
        }
        xdf2.nreff = xdf2.reference_index.len() as i64;

        // Allocate K vectors (xdl_do_diff). koff lets us use negative diagonals.
        let ndiags = xdf1.nreff + xdf2.nreff + 3;
        let kvd_len = (2 * ndiags + 2) as usize;
        let mut kvd = vec![0i64; kvd_len];
        // kvdf base offset = nreff2 + 1; kvdb base offset = ndiags + nreff2 + 1.
        let koff = xdf2.nreff + 1;
        let (kvdf_slice, kvdb_slice) = kvd.split_at_mut(ndiags as usize);
        // Both slices are indexed as [k + koff]; their length covers the diagonal range.

        let xenv = XdAlgoEnv {
            mxcost: bogosqrt(ndiags).max(XDL_MAX_COST_MIN),
            snake_cnt: XDL_SNAKE_CNT,
            heur_min: XDL_HEUR_MIN_COST,
        };

        let nreff1 = xdf1.nreff;
        let nreff2 = xdf2.nreff;
        xdl_recs_cmp(
            &mut xdf1, 0, nreff1, &mut xdf2, 0, nreff2, kvdf_slice, kvdb_slice, koff, false, &xenv,
        );

        (xdf1.changed, xdf2.changed)
    }
}

/// Convert per-token `changed` flags (Git xdiff style) into `similar::DiffOp`s.
fn changed_flags_to_ops(
    changed1: &[bool],
    changed2: &[bool],
    old_len: usize,
    new_len: usize,
) -> Vec<similar::DiffOp> {
    use similar::DiffOp;
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_len || j < new_len {
        let del = i < old_len && changed1[i];
        let ins = j < new_len && changed2[j];
        if !del && !ins {
            // Equal run.
            let start_i = i;
            let start_j = j;
            while i < old_len && j < new_len && !changed1[i] && !changed2[j] {
                i += 1;
                j += 1;
            }
            ops.push(DiffOp::Equal {
                old_index: start_i,
                new_index: start_j,
                len: i - start_i,
            });
        } else {
            // Changed run: consecutive deletions then/and insertions.
            let start_i = i;
            let start_j = j;
            while i < old_len && changed1[i] {
                i += 1;
            }
            while j < new_len && changed2[j] {
                j += 1;
            }
            let dlen = i - start_i;
            let ilen = j - start_j;
            if dlen > 0 && ilen > 0 {
                ops.push(DiffOp::Replace {
                    old_index: start_i,
                    old_len: dlen,
                    new_index: start_j,
                    new_len: ilen,
                });
            } else if dlen > 0 {
                ops.push(DiffOp::Delete {
                    old_index: start_i,
                    old_len: dlen,
                    new_index: start_j,
                });
            } else if ilen > 0 {
                ops.push(DiffOp::Insert {
                    old_index: start_i,
                    new_index: start_j,
                    new_len: ilen,
                });
            }
        }
    }
    ops
}

/// Diff two token streams with a Git-compatible implementation of the xdiff Myers engine and return the
/// result as `similar::DiffOp`s. Used by the word-diff machinery, where matching Git's exact
/// record selection and tie-breaking matters: `imara-diff`/`similar` pick different — but
/// equally minimal — alignments for streams with many repeated tokens (e.g. the `bibtex`
/// driver's `{`/`}`/`,`), mismatching Git's reference output.
#[must_use]
pub fn word_diff_ops_imara(old_words: &[&str], new_words: &[&str]) -> Vec<similar::DiffOp> {
    use std::collections::HashMap;

    // Intern tokens to ids (equal id <=> equal token), mirroring xdiff's record hashing.
    let mut interner: HashMap<&str, u32> = HashMap::new();
    let mut ids1: Vec<u32> = Vec::with_capacity(old_words.len());
    for &w in old_words {
        let next = interner.len() as u32;
        let id = *interner.entry(w).or_insert(next);
        ids1.push(id);
    }
    let mut ids2: Vec<u32> = Vec::with_capacity(new_words.len());
    for &w in new_words {
        let next = interner.len() as u32;
        let id = *interner.entry(w).or_insert(next);
        ids2.push(id);
    }

    let (changed1, changed2) = git_xdiff::changed_flags(&ids1, &ids2);
    let ops = changed_flags_to_ops(&changed1, &changed2, old_words.len(), new_words.len());

    // Slide changed runs to Git's canonical position (`xdl_change_compact`); the word
    // diff never enables the indent heuristic.
    diff_indent_heuristic::apply_change_compact_to_ops(&ops, old_words, new_words, false)
}

/// Line-diff ops for string slices after Git `xdl_change_compact` (and optional indent heuristic).
#[must_use]
pub fn diff_slice_ops_compacted(
    old_lines: &[&str],
    new_lines: &[&str],
    algorithm: similar::Algorithm,
    indent_heuristic: bool,
) -> Vec<similar::DiffOp> {
    diff_indent_heuristic::diff_slice_ops_compacted(
        old_lines,
        new_lines,
        algorithm,
        indent_heuristic,
    )
}

/// Map each line in `new_joined` to its origin in `old_joined` after Git-style compaction (for blame).
#[must_use]
pub fn map_new_to_old_lines_compacted(
    old_joined: &str,
    new_joined: &str,
    algorithm: similar::Algorithm,
    indent_heuristic: bool,
    new_line_count: usize,
) -> Vec<Option<usize>> {
    let ops = diff_indent_heuristic::diff_lines_ops_compacted(
        old_joined,
        new_joined,
        algorithm,
        indent_heuristic,
    );
    diff_indent_heuristic::map_new_to_old_from_ops(&ops, new_line_count)
}

/// The kind of change between two sides of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    /// File was added.
    Added,
    /// File was deleted.
    Deleted,
    /// File was modified (content or mode change).
    Modified,
    /// File was renamed (with optional content change).
    Renamed,
    /// File was copied.
    Copied,
    /// File type changed (e.g. regular → symlink).
    TypeChanged,
    /// Unmerged (conflict).
    Unmerged,
}

impl DiffStatus {
    /// Single-character status letter used in raw diff output.
    #[must_use]
    pub fn letter(&self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::TypeChanged => 'T',
            Self::Unmerged => 'U',
        }
    }
}

/// A single diff entry representing one changed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    /// The status of this change.
    pub status: DiffStatus,
    /// Path in the "old" side (None for Added).
    pub old_path: Option<String>,
    /// Path in the "new" side (None for Deleted).
    pub new_path: Option<String>,
    /// Old file mode (as octal string, e.g. "100644").
    pub old_mode: String,
    /// New file mode.
    pub new_mode: String,
    /// Old object ID (zero OID for Added).
    pub old_oid: ObjectId,
    /// New object ID (zero OID for Deleted).
    pub new_oid: ObjectId,
    /// Similarity score (0–100) for renames/copies.
    pub score: Option<u32>,
}

impl DiffEntry {
    /// The primary path for display (new_path for adds, old_path for deletes).
    #[must_use]
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    /// Return a human-oriented path display for this entry.
    ///
    /// For renames and copies this returns `old -> new`; for all other entry
    /// kinds this returns the primary path.
    #[must_use]
    pub fn display_path(&self) -> String {
        match self.status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let old = self.old_path.as_deref().unwrap_or("");
                let new = self.new_path.as_deref().unwrap_or("");
                if old.is_empty() || new.is_empty() {
                    self.path().to_owned()
                } else {
                    format!("{old} -> {new}")
                }
            }
            _ => self.path().to_owned(),
        }
    }
}

/// The zero (null) object ID used for "no object" in diff output.
pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// Return the zero ObjectId.
#[must_use]
pub fn zero_oid() -> ObjectId {
    ObjectId::from_bytes(&[0u8; 20]).unwrap_or_else(|_| {
        // This should never fail since we pass exactly 20 bytes
        panic!("internal error: failed to create zero OID");
    })
}

/// Return the ObjectId for the empty blob object.
#[must_use]
pub fn empty_blob_oid() -> ObjectId {
    ObjectId::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap_or_else(|_| {
        // This should never fail since the object ID literal is valid.
        panic!("internal error: failed to create empty blob OID");
    })
}

// ── Tree-to-tree diff ───────────────────────────────────────────────

/// Compare two trees and return the list of changed entries.
///
/// # Parameters
///
/// - `odb` — object database to read tree objects from.
/// - `old_tree_oid` — OID of the old tree (or `None` for comparison against empty).
/// - `new_tree_oid` — OID of the new tree (or `None` for comparison against empty).
/// - `prefix` — path prefix for nested tree recursion (empty string for root).
///
/// # Errors
///
/// Returns errors from object database reads.
pub fn diff_trees(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
) -> Result<Vec<DiffEntry>> {
    diff_trees_opts(odb, old_tree_oid, new_tree_oid, prefix, false)
}

/// Like `diff_trees` but with `show_trees` flag: when true, emit entries for
/// tree objects themselves in addition to their recursive contents (the `-t`
/// flag of `diff-tree`).
pub fn diff_trees_show_tree_entries(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
) -> Result<Vec<DiffEntry>> {
    diff_trees_opts(odb, old_tree_oid, new_tree_oid, prefix, true)
}

fn diff_trees_opts(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
    prefix: &str,
    show_trees: bool,
) -> Result<Vec<DiffEntry>> {
    let old_entries = match old_tree_oid {
        Some(oid) => read_tree(odb, oid)?,
        None => Vec::new(),
    };
    let new_entries = match new_tree_oid {
        Some(oid) => read_tree(odb, oid)?,
        None => Vec::new(),
    };

    let mut result = Vec::new();
    diff_tree_entries_opts(
        odb,
        &old_entries,
        &new_entries,
        prefix,
        show_trees,
        &mut result,
    )?;
    Ok(result)
}

/// Read and parse a tree object from the ODB.
fn read_tree(odb: &Odb, oid: &ObjectId) -> Result<Vec<TreeEntry>> {
    let obj = odb.read(oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::CorruptObject(format!(
            "expected tree, got {}",
            obj.kind.as_str()
        )));
    }
    parse_tree(&obj.data)
}

/// Compare two sorted lists of tree entries, recursing into subtrees.
fn diff_tree_entries_opts(
    odb: &Odb,
    old: &[TreeEntry],
    new: &[TreeEntry],
    prefix: &str,
    show_trees: bool,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() || ni < new.len() {
        match (old.get(oi), new.get(ni)) {
            (Some(o), Some(n)) => {
                let cmp = crate::objects::tree_entry_cmp(
                    &o.name,
                    is_tree_mode(o.mode),
                    &n.name,
                    is_tree_mode(n.mode),
                );
                match cmp {
                    std::cmp::Ordering::Less => {
                        // Old entry not in new → deleted
                        emit_deleted_opts(odb, o, prefix, show_trees, result)?;
                        oi += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        // New entry not in old → added
                        emit_added_opts(odb, n, prefix, show_trees, result)?;
                        ni += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        // Both present — check for changes
                        if o.oid != n.oid || o.mode != n.mode {
                            let name_str = String::from_utf8_lossy(&o.name);
                            let path = format_path(prefix, &name_str);
                            if is_tree_mode(o.mode) && is_tree_mode(n.mode) {
                                // Both are trees
                                if show_trees {
                                    result.push(DiffEntry {
                                        status: DiffStatus::Modified,
                                        old_path: Some(path.clone()),
                                        new_path: Some(path.clone()),
                                        old_mode: format_mode(o.mode),
                                        new_mode: format_mode(n.mode),
                                        old_oid: o.oid,
                                        new_oid: n.oid,
                                        score: None,
                                    });
                                }
                                // Recurse
                                let nested = diff_trees_opts(
                                    odb,
                                    Some(&o.oid),
                                    Some(&n.oid),
                                    &path,
                                    show_trees,
                                )?;
                                result.extend(nested);
                            } else if is_tree_mode(o.mode) && !is_tree_mode(n.mode) {
                                // Tree → blob: delete tree contents, add blob
                                emit_deleted_opts(odb, o, prefix, show_trees, result)?;
                                emit_added_opts(odb, n, prefix, show_trees, result)?;
                            } else if !is_tree_mode(o.mode) && is_tree_mode(n.mode) {
                                // Blob → tree: delete blob, add tree contents
                                emit_deleted_opts(odb, o, prefix, show_trees, result)?;
                                emit_added_opts(odb, n, prefix, show_trees, result)?;
                            } else {
                                // Both blobs — modified.
                                // A mode-only change (e.g. chmod) is Modified.
                                // TypeChanged is only for actual type changes (blob ↔ symlink).
                                let old_type = o.mode & 0o170000;
                                let new_type = n.mode & 0o170000;
                                result.push(DiffEntry {
                                    status: if old_type != new_type {
                                        DiffStatus::TypeChanged
                                    } else {
                                        DiffStatus::Modified
                                    },
                                    old_path: Some(path.clone()),
                                    new_path: Some(path),
                                    old_mode: format_mode(o.mode),
                                    new_mode: format_mode(n.mode),
                                    old_oid: o.oid,
                                    new_oid: n.oid,
                                    score: None,
                                });
                            }
                        }
                        oi += 1;
                        ni += 1;
                    }
                }
            }
            (Some(o), None) => {
                emit_deleted_opts(odb, o, prefix, show_trees, result)?;
                oi += 1;
            }
            (None, Some(n)) => {
                emit_added_opts(odb, n, prefix, show_trees, result)?;
                ni += 1;
            }
            (None, None) => break,
        }
    }

    Ok(())
}

fn emit_deleted_opts(
    odb: &Odb,
    entry: &TreeEntry,
    prefix: &str,
    show_trees: bool,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    let name_str = String::from_utf8_lossy(&entry.name);
    let path = format_path(prefix, &name_str);
    if is_tree_mode(entry.mode) {
        if show_trees {
            result.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path.clone()),
                new_path: None,
                old_mode: format_mode(entry.mode),
                new_mode: "000000".to_owned(),
                old_oid: entry.oid,
                new_oid: zero_oid(),
                score: None,
            });
        }
        // Recurse into deleted tree
        let nested = diff_trees_opts(odb, Some(&entry.oid), None, &path, show_trees)?;
        result.extend(nested);
    } else {
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path.clone()),
            new_path: None,
            old_mode: format_mode(entry.mode),
            new_mode: "000000".to_owned(),
            old_oid: entry.oid,
            new_oid: zero_oid(),
            score: None,
        });
    }
    Ok(())
}

fn emit_added_opts(
    odb: &Odb,
    entry: &TreeEntry,
    prefix: &str,
    show_trees: bool,
    result: &mut Vec<DiffEntry>,
) -> Result<()> {
    let name_str = String::from_utf8_lossy(&entry.name);
    let path = format_path(prefix, &name_str);
    if is_tree_mode(entry.mode) {
        if show_trees {
            result.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path.clone()),
                old_mode: "000000".to_owned(),
                new_mode: format_mode(entry.mode),
                old_oid: zero_oid(),
                new_oid: entry.oid,
                score: None,
            });
        }
        // Recurse into added tree
        let nested = diff_trees_opts(odb, None, Some(&entry.oid), &path, show_trees)?;
        result.extend(nested);
    } else {
        result.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(path),
            old_mode: "000000".to_owned(),
            new_mode: format_mode(entry.mode),
            old_oid: zero_oid(),
            new_oid: entry.oid,
            score: None,
        });
    }
    Ok(())
}

// ── Index-to-tree diff (staged changes) ─────────────────────────────

/// Compare the index against a tree (usually HEAD's tree).
///
/// This shows "staged" changes — what would be committed.
///
/// # Parameters
///
/// - `odb` — object database.
/// - `index` — the current index.
/// - `tree_oid` — the tree to compare against (e.g. HEAD's tree), or `None`
///   for comparison against an empty tree (initial commit).
///
/// # Errors
///
/// Returns errors from ODB reads.
///
/// When `ignore_submodules` is true, gitlink (`160000`) paths are omitted from the diff, matching
/// Git's `require_clean_work_tree(..., ignore_submodules=1)` used by `git rebase` / `git pull`.
pub fn diff_index_to_tree(
    odb: &Odb,
    index: &Index,
    tree_oid: Option<&ObjectId>,
    ignore_submodules: bool,
) -> Result<Vec<DiffEntry>> {
    // Flatten the tree into a sorted list of (path, mode, oid)
    let tree_entries = match tree_oid {
        Some(oid) => flatten_tree(odb, oid, "")?,
        None => Vec::new(),
    };

    // Build maps keyed by path
    let mut tree_map: std::collections::BTreeMap<&str, &FlatEntry> =
        std::collections::BTreeMap::new();
    for entry in &tree_entries {
        tree_map.insert(&entry.path, entry);
    }

    let mut result = Vec::new();
    let mut stage0_paths = std::collections::BTreeSet::new();
    let mut unmerged_modes: std::collections::BTreeMap<String, (u8, u32)> =
        std::collections::BTreeMap::new();

    // Check index entries against tree
    for ie in &index.entries {
        let path = String::from_utf8_lossy(&ie.path).to_string();
        if ie.stage() == 0 && ie.intent_to_add() {
            // Intent-to-add entries are not "staged" for diff-index / status
            // (matches Git: `git diff --cached` is empty for `-N` paths).
            continue;
        }
        if ie.stage() != 0 {
            let rank = match ie.stage() {
                2 => 0u8,
                3 => 1u8,
                1 => 2u8,
                _ => 3u8,
            };
            match unmerged_modes.get(&path) {
                Some((existing_rank, _)) if *existing_rank <= rank => {}
                _ => {
                    unmerged_modes.insert(path, (rank, ie.mode));
                }
            }
            continue;
        }
        if ignore_submodules && ie.mode == 0o160000 {
            let _ = tree_map.remove(path.as_str());
            stage0_paths.insert(path.clone());
            continue;
        }
        stage0_paths.insert(path.clone());
        match tree_map.remove(path.as_str()) {
            Some(te) => {
                // Present in both — check for differences
                if te.oid != ie.oid || te.mode != ie.mode {
                    result.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(path.clone()),
                        new_path: Some(path),
                        old_mode: format_mode(te.mode),
                        new_mode: format_mode(ie.mode),
                        old_oid: te.oid,
                        new_oid: ie.oid,
                        score: None,
                    });
                }
            }
            None => {
                // In index but not tree → added
                result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path),
                    old_mode: "000000".to_owned(),
                    new_mode: format_mode(ie.mode),
                    old_oid: zero_oid(),
                    new_oid: ie.oid,
                    score: None,
                });
            }
        }
    }

    for (path, (_, mode)) in &unmerged_modes {
        if stage0_paths.contains(path) {
            continue;
        }
        tree_map.remove(path.as_str());
        result.push(DiffEntry {
            status: DiffStatus::Unmerged,
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            old_mode: "000000".to_owned(),
            new_mode: format_mode(*mode),
            old_oid: zero_oid(),
            new_oid: zero_oid(),
            score: None,
        });
    }

    // Remaining tree entries not in index → deleted
    for (path, te) in tree_map {
        if ignore_submodules && te.mode == 0o160000 {
            continue;
        }
        result.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(path.to_owned()),
            new_path: None,
            old_mode: format_mode(te.mode),
            new_mode: "000000".to_owned(),
            old_oid: te.oid,
            new_oid: zero_oid(),
            score: None,
        });
    }

    result.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(result)
}

// ── Index-to-worktree diff (unstaged changes) ───────────────────────

/// Compare the index against the working tree.
///
/// This shows "unstaged" changes — modifications not yet staged.
///
/// Entries with [`IndexEntry::assume_unchanged`] or [`IndexEntry::skip_worktree`] are treated as
/// matching the work tree without examining the filesystem (Git `CE_VALID` / skip-worktree).
///
/// # Parameters
///
/// - `odb` — object database (for hashing worktree files).
/// - `index` — the current index.
/// - `work_tree` — path to the working tree root.
/// - `ignore_submodule_untracked` — when true, gitlink entries are not dirty solely from untracked
///   files inside the submodule (matches `git status -uno`).
/// - `simplify_gitlinks` — when true, nested gitlink entries only compare the submodule checkout
///   HEAD to the recorded OID (ignore dirty work trees inside nested submodules). Used when
///   computing `submodule_porcelain_flags` so untracked files under a nested submodule do not set
///   the parent submodule's `modified` bit (Git `DIRTY_SUBMODULE_MODIFIED`; t7506).
///
/// # Errors
///
/// Returns errors from I/O or hashing.
pub fn diff_index_to_worktree(
    odb: &Odb,
    index: &Index,
    work_tree: &Path,
    ignore_submodule_untracked: bool,
    simplify_gitlinks: bool,
) -> Result<Vec<DiffEntry>> {
    diff_index_to_worktree_with_options(
        odb,
        index,
        work_tree,
        DiffIndexToWorktreeOptions {
            ignore_submodule_untracked,
            simplify_gitlinks,
            ..DiffIndexToWorktreeOptions::default()
        },
    )
}

/// Additional inputs for [`diff_index_to_worktree_with_options`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DiffIndexToWorktreeOptions {
    /// Optional index mtime pair `(sec, nsec)` sampled when the index was read.
    ///
    /// When provided, entries with matching stat data are still considered dirty candidates if
    /// their recorded mtime is "racy" (at or after this timestamp), matching Git's
    /// `is_racy_timestamp` behavior.
    pub index_mtime: Option<(u32, u32)>,
    /// When true, gitlink entries are not dirty solely from untracked files inside the submodule.
    pub ignore_submodule_untracked: bool,
    /// When true, nested gitlink entries only compare the submodule checkout HEAD to the recorded OID.
    pub simplify_gitlinks: bool,
    /// When true, a populated gitlink checkout whose `.git` indirection cannot resolve to a HEAD
    /// is returned as an error instead of a normal modified gitlink.
    pub error_on_broken_gitlinks: bool,
}

/// Compare the index against the working tree with optional racy-timestamp context.
///
/// This variant enables a stat-trust fast path: if an entry's stat tuple matches and the mode is
/// unchanged, the worktree blob hash is skipped unless the entry is racy relative to the supplied
/// index mtime.
///
/// # Parameters
///
/// - `odb` — object database (for hashing worktree files).
/// - `index` — the current index.
/// - `work_tree` — path to the working tree root.
/// - `options` — optional context for racy timestamp checks.
///
/// # Errors
///
/// Returns errors from I/O or hashing.
pub fn diff_index_to_worktree_with_options(
    odb: &Odb,
    index: &Index,
    work_tree: &Path,
    options: DiffIndexToWorktreeOptions,
) -> Result<Vec<DiffEntry>> {
    use crate::config::ConfigSet;
    use crate::crlf;

    let ignore_submodule_untracked = options.ignore_submodule_untracked;
    let simplify_gitlinks = options.simplify_gitlinks;

    let git_dir = work_tree.join(".git");
    let config = ConfigSet::load(Some(&git_dir), true).unwrap_or_else(|_| ConfigSet::new());
    let conv = crlf::ConversionConfig::from_config(&config);
    let attrs = crlf::load_gitattributes(work_tree);

    let mut result = Vec::new();
    let mut unmerged_base: std::collections::BTreeMap<String, (u8, &IndexEntry)> =
        std::collections::BTreeMap::new();

    // Cache of ancestor-directory symlink-ness so we stat each directory at most
    // once, rather than re-lstat'ing every ancestor of every index entry
    // (O(unique dirs) instead of O(files × depth)).
    let mut dir_symlinks = SymlinkDirCache::default();

    for ie in &index.entries {
        if ie.stage() != 0 {
            let path = String::from_utf8_lossy(&ie.path).to_string();
            let rank = match ie.stage() {
                2 => 0u8,
                3 => 1u8,
                1 => 2u8,
                _ => 3u8,
            };
            match unmerged_base.get(&path) {
                Some((existing_rank, _)) if *existing_rank <= rank => {}
                _ => {
                    unmerged_base.insert(path, (rank, ie));
                }
            }
            continue;
        }
        // Sparse checkout: paths outside the cone are not expected on disk; `assume_unchanged`
        // is treated as clean without reading the filesystem (wt-status.c).
        if ie.skip_worktree() || ie.assume_unchanged() {
            continue;
        }
        // Use str slice directly to avoid allocation for path joining;
        // only allocate String if we need it for DiffEntry output.
        let path_str_ref = std::str::from_utf8(&ie.path).unwrap_or("");
        let is_intent_to_add = ie.intent_to_add();

        // Gitlink entries (submodules): Git's `diff-index` reports `M` when the recorded
        // commit differs from the submodule checkout **or** when the submodule work tree is
        // dirty (staged/unstaged/untracked) even if HEAD still matches the gitlink. For the
        // latter case the "new" OID column is the null OID (see `git diff-index` / t7506).
        if ie.mode == 0o160000 {
            let sub_dir = work_tree.join(path_str_ref);
            let sub_head_oid = read_submodule_head_oid(&sub_dir);
            // A gitlink whose worktree directory is entirely absent is a deleted submodule. Git's
            // `check_removed` (diff-lib.c) `lstat`s the path first: a missing directory is reported
            // as a removal (`D`, new mode 000000), *before* the submodule "not checked out" special
            // case (which only applies when the directory exists). An empty directory that exists is
            // a placeholder and stays unchanged. Skipped when `simplify_gitlinks` so callers that
            // only compare recorded HEADs keep their behaviour. (t4060 #50/#51.)
            if !simplify_gitlinks && sub_head_oid.is_none() && !sub_dir.exists() {
                let path_owned = path_str_ref.to_owned();
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path_owned.clone()),
                    new_path: Some(path_owned),
                    old_mode: format_mode(ie.mode),
                    new_mode: "000000".to_owned(),
                    old_oid: ie.oid,
                    new_oid: zero_oid(),
                    score: None,
                });
                continue;
            }
            let ref_matches = if let Some(oid) = sub_head_oid {
                oid == ie.oid
            } else {
                let is_placeholder = submodule_worktree_is_unpopulated_placeholder(&sub_dir);
                if options.error_on_broken_gitlinks
                    && !is_placeholder
                    && submodule_embedded_git_dir(&sub_dir).is_some()
                {
                    return Err(Error::ConfigError(format!(
                        "could not read submodule HEAD for '{path_str_ref}'"
                    )));
                }
                is_placeholder
            };
            if simplify_gitlinks {
                if !ref_matches {
                    let path_owned = path_str_ref.to_owned();
                    let new_oid = sub_head_oid.unwrap_or_else(zero_oid);
                    result.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(path_owned.clone()),
                        new_path: Some(path_owned),
                        old_mode: format_mode(ie.mode),
                        new_mode: format_mode(ie.mode),
                        old_oid: ie.oid,
                        new_oid,
                        score: None,
                    });
                }
                continue;
            }
            // A populated submodule whose HEAD points at a commit object that is missing from its
            // own object store is corrupt. Git's `is_submodule_modified` shells `git status` into
            // the submodule, which fails, and Git aborts the surrounding status/diff. Mirror that:
            // a broken submodule is a hard error rather than a silently-clean gitlink (t5526 #38).
            if sub_head_oid.is_some() && submodule_head_object_broken(&sub_dir) {
                return Err(Error::ConfigError(format!(
                    "'git status --porcelain=2' failed in submodule {path_str_ref}"
                )));
            }
            let mut flags = submodule_porcelain_flags(work_tree, path_str_ref, ie.oid);
            if ignore_submodule_untracked {
                flags.untracked = false;
            }
            let inner_dirty = flags.modified || flags.untracked;
            if !ref_matches || inner_dirty {
                let path_owned = path_str_ref.to_owned();
                let new_oid = if !ref_matches {
                    sub_head_oid.unwrap_or_else(zero_oid)
                } else {
                    zero_oid()
                };
                result.push(DiffEntry {
                    status: DiffStatus::Modified,
                    old_path: Some(path_owned.clone()),
                    new_path: Some(path_owned),
                    old_mode: format_mode(ie.mode),
                    new_mode: format_mode(ie.mode),
                    old_oid: ie.oid,
                    new_oid,
                    score: None,
                });
            }
            continue;
        }

        let file_path = work_tree.join(path_str_ref);

        if is_intent_to_add {
            match fs::symlink_metadata(&file_path) {
                Ok(meta) => {
                    let file_attrs = crlf::get_file_attrs(&attrs, path_str_ref, false, &config);
                    let worktree_oid = hash_worktree_file(
                        odb,
                        &file_path,
                        &meta,
                        &conv,
                        &file_attrs,
                        path_str_ref,
                        None,
                    )?;
                    let worktree_mode = mode_from_metadata(&meta);
                    result.push(DiffEntry {
                        status: DiffStatus::Added,
                        old_path: None,
                        new_path: Some(path_str_ref.to_owned()),
                        old_mode: "000000".to_owned(),
                        new_mode: format_mode(worktree_mode),
                        // `ita_invisible_in_index`: null OID on the index side for patch output
                        // (`index 0000000..`, t2203); index entry still stores the empty blob.
                        old_oid: zero_oid(),
                        new_oid: worktree_oid,
                        score: None,
                    });
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
                {
                    result.push(DiffEntry {
                        status: DiffStatus::Deleted,
                        old_path: Some(path_str_ref.to_owned()),
                        new_path: None,
                        old_mode: format_mode(ie.mode),
                        new_mode: "000000".to_owned(),
                        old_oid: ie.oid,
                        new_oid: zero_oid(),
                        score: None,
                    });
                }
                Err(e) => return Err(Error::Io(e)),
            }
            continue;
        }

        // If any parent component of the path is a symlink, the file is effectively
        // deleted from the working tree (a symlink replaced a directory).
        if dir_symlinks.has_symlink_in_path(work_tree, path_str_ref) {
            result.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path_str_ref.to_owned()),
                new_path: None,
                old_mode: format_mode(ie.mode),
                new_mode: "000000".to_owned(),
                old_oid: ie.oid,
                new_oid: zero_oid(),
                score: None,
            });
            continue;
        }

        match fs::symlink_metadata(&file_path) {
            Ok(meta) if meta.is_dir() => {
                // A directory exists where the index expects a file. A populated submodule
                // checkout (`.git` present) is a blob→gitlink typechange with the submodule HEAD on
                // the new side (raw output re-zeros it); otherwise the indexed file is effectively
                // deleted. See t4041/t4060 #13.
                if file_path.join(".git").exists() {
                    let head = read_submodule_head_oid(&file_path).unwrap_or_else(zero_oid);
                    let path_owned = path_str_ref.to_owned();
                    result.push(DiffEntry {
                        status: DiffStatus::TypeChanged,
                        old_path: Some(path_owned.clone()),
                        new_path: Some(path_owned),
                        old_mode: format_mode(ie.mode),
                        new_mode: format_mode(0o160000),
                        old_oid: ie.oid,
                        new_oid: head,
                        score: None,
                    });
                    continue;
                }
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path_str_ref.to_owned()),
                    new_path: None,
                    old_mode: format_mode(ie.mode),
                    new_mode: String::new(),
                    old_oid: ie.oid,
                    new_oid: zero_oid(),
                    score: None,
                });
            }
            Ok(meta) => {
                let worktree_mode = mode_from_metadata(&meta);
                let stat_same = stat_matches(ie, &meta);
                // Mode-only change: stat still matches the index entry but executable bit differs.
                if stat_same && worktree_mode != ie.mode {
                    let path_owned = path_str_ref.to_owned();
                    result.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(path_owned.clone()),
                        new_path: Some(path_owned),
                        old_mode: format_mode(ie.mode),
                        new_mode: format_mode(worktree_mode),
                        old_oid: ie.oid,
                        new_oid: ie.oid,
                        score: None,
                    });
                    continue;
                }

                // Fast path: unchanged stat + unchanged mode + non-racy timestamp means this entry
                // is clean without re-hashing blob data.
                if stat_same && worktree_mode == ie.mode && !entry_is_racy(ie, options.index_mtime) {
                    continue;
                }

                // Hash the worktree blob for uncertain/racy entries.
                let file_attrs = crlf::get_file_attrs(&attrs, path_str_ref, false, &config);
                let worktree_oid = hash_worktree_file(
                    odb,
                    &file_path,
                    &meta,
                    &conv,
                    &file_attrs,
                    path_str_ref,
                    Some(ie),
                )?;

                // If clean conversion disagrees with the index but raw bytes match the
                // blob (e.g. mixed line endings committed with autocrlf off), Git reports
                // no diff (t0020: touch + git diff --exit-code).
                let mut eff_oid = worktree_oid;
                if eff_oid != ie.oid {
                    if let Ok(raw) = fs::read(&file_path) {
                        let raw_oid = Odb::hash_object_data(ObjectKind::Blob, &raw);
                        if raw_oid == ie.oid {
                            eff_oid = ie.oid;
                        }
                    }
                }

                if eff_oid != ie.oid || worktree_mode != ie.mode {
                    let path_owned = path_str_ref.to_owned();
                    result.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(path_owned.clone()),
                        new_path: Some(path_owned),
                        old_mode: format_mode(ie.mode),
                        new_mode: format_mode(worktree_mode),
                        old_oid: ie.oid,
                        new_oid: eff_oid,
                    score: None,
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(20) /* ENOTDIR */ => {
                // File deleted from working tree (or parent replaced by a file)
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path_str_ref.to_owned()),
                    new_path: None,
                    old_mode: format_mode(ie.mode),
                    new_mode: "000000".to_owned(),
                    old_oid: ie.oid,
                    new_oid: zero_oid(),
                    score: None,
                });
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }

    for (path, (_, base_entry)) in unmerged_base {
        let file_path = work_tree.join(&path);
        let wt_meta = match fs::symlink_metadata(&file_path) {
            Ok(meta) => Some(meta),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
            {
                None
            }
            Err(e) => return Err(Error::Io(e)),
        };

        let new_mode = wt_meta.as_ref().map_or_else(
            || "000000".to_owned(),
            |meta| format_mode(mode_from_metadata(meta)),
        );
        result.push(DiffEntry {
            status: DiffStatus::Unmerged,
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            old_mode: "000000".to_owned(),
            new_mode,
            old_oid: zero_oid(),
            new_oid: zero_oid(),
            score: None,
        });

        if let Some(meta) = wt_meta {
            let file_attrs = crlf::get_file_attrs(&attrs, &path, false, &config);
            let wt_oid = hash_worktree_file(
                odb,
                &file_path,
                &meta,
                &conv,
                &file_attrs,
                &path,
                Some(base_entry),
            )?;
            let wt_mode = mode_from_metadata(&meta);
            if wt_oid != base_entry.oid || wt_mode != base_entry.mode {
                result.push(DiffEntry {
                    status: DiffStatus::Modified,
                    old_path: Some(path.clone()),
                    new_path: Some(path),
                    old_mode: format_mode(base_entry.mode),
                    new_mode: format_mode(wt_mode),
                    old_oid: base_entry.oid,
                    new_oid: wt_oid,
                    score: None,
                });
            }
        }
    }

    Ok(result)
}

/// Memoized cache of which ancestor directories are symlinks, so each directory
/// is lstat'd at most once per `diff_index_to_worktree` call.
#[derive(Default)]
struct SymlinkDirCache {
    /// Relative dir prefixes confirmed to be symlinks.
    symlink: std::collections::HashSet<String>,
    /// Relative dir prefixes confirmed not to be symlinks.
    plain: std::collections::HashSet<String>,
}

impl SymlinkDirCache {
    /// Whether any parent component of `rel_path` is a symlink.
    fn has_symlink_in_path(&mut self, work_tree: &Path, rel_path: &str) -> bool {
        let components: Vec<&str> = rel_path.split('/').collect();
        let mut prefix = String::new();
        // Check every ancestor directory (all components except the file itself).
        for component in &components[..components.len().saturating_sub(1)] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if self.symlink.contains(&prefix) {
                return true;
            }
            if self.plain.contains(&prefix) {
                continue;
            }
            match fs::symlink_metadata(work_tree.join(&prefix)) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    self.symlink.insert(prefix.clone());
                    return true;
                }
                _ => {
                    self.plain.insert(prefix.clone());
                }
            }
        }
        false
    }
}

/// Whether an index entry's cached mtime is "racy" relative to the index file's own mtime.
///
/// A racy entry was written to the worktree at (or after) the moment the index was saved, so a
/// matching stat tuple cannot prove the content is unchanged — callers must re-hash. This is
/// Git's `is_racy_timestamp` rule.
///
/// # Parameters
/// - `ie` — the index entry to test.
/// - `index_mtime` — the on-disk index file's `(mtime_sec, mtime_nsec)`; when `None` (or the
///   seconds value is zero) racy detection is skipped and this returns `false`.
#[must_use]
pub fn entry_is_racy(ie: &IndexEntry, index_mtime: Option<(u32, u32)>) -> bool {
    let Some((index_mtime_sec, index_mtime_nsec)) = index_mtime else {
        return false;
    };
    if index_mtime_sec == 0 {
        return false;
    }
    index_mtime_sec < ie.mtime_sec
        || (index_mtime_sec == ie.mtime_sec && index_mtime_nsec <= ie.mtime_nsec)
}

/// Quick stat check: does the index entry's cached stat data match the file?
/// Returns true when the file at `ie`'s path differs from the index entry (mode or blob).
///
/// Used by commands such as `git mv` to detect "dirty" paths under sparse checkout.
/// Symlinks and submodules are compared in a Git-compatible way.
///
/// `ignore_submodule_untracked` mirrors [`diff_index_to_worktree`]'s same flag for gitlinks.
pub fn worktree_differs_from_index_entry(
    odb: &Odb,
    work_tree: &Path,
    ie: &IndexEntry,
    ignore_submodule_untracked: bool,
) -> Result<bool> {
    use crate::config::ConfigSet;
    use crate::crlf;

    let path_str_ref = std::str::from_utf8(&ie.path).unwrap_or("");
    let file_path = work_tree.join(path_str_ref);

    if ie.mode == 0o160000 {
        let sub_head_oid = read_submodule_head(&file_path);
        let ref_matches = match sub_head_oid {
            Some(oid) => oid == ie.oid,
            None => submodule_worktree_is_unpopulated_placeholder(&file_path),
        };
        let mut flags = submodule_porcelain_flags(work_tree, path_str_ref, ie.oid);
        if ignore_submodule_untracked {
            flags.untracked = false;
        }
        return Ok(!ref_matches || flags.modified || flags.untracked);
    }

    let meta = match fs::symlink_metadata(&file_path) {
        Ok(m) => m,
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
        {
            return Ok(true);
        }
        Err(e) => return Err(Error::Io(e)),
    };

    if meta.is_dir() {
        return Ok(true);
    }

    let worktree_mode = mode_from_metadata(&meta);
    if worktree_mode != ie.mode {
        return Ok(true);
    }

    let git_dir = work_tree.join(".git");
    let config = ConfigSet::load(Some(&git_dir), true).unwrap_or_else(|_| ConfigSet::new());
    let conv = crlf::ConversionConfig::from_config(&config);
    let attrs = crlf::load_gitattributes(work_tree);
    let file_attrs = crlf::get_file_attrs(&attrs, path_str_ref, false, &config);
    let worktree_oid = hash_worktree_file(
        odb,
        &file_path,
        &meta,
        &conv,
        &file_attrs,
        path_str_ref,
        Some(ie),
    )?;

    let mut eff_oid = worktree_oid;
    if eff_oid != ie.oid {
        if let Ok(raw) = fs::read(&file_path) {
            let raw_oid = Odb::hash_object_data(ObjectKind::Blob, &raw);
            if raw_oid == ie.oid {
                eff_oid = ie.oid;
            }
        }
    }

    Ok(eff_oid != ie.oid)
}

pub fn stat_matches(ie: &IndexEntry, meta: &fs::Metadata) -> bool {
    // Compare size
    if meta.len() as u32 != ie.size {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Compare mtime (seconds + nanoseconds)
        if meta.mtime() as u32 != ie.mtime_sec {
            return false;
        }
        if meta.mtime_nsec() as u32 != ie.mtime_nsec {
            return false;
        }
        // Compare ctime (seconds + nanoseconds)
        if meta.ctime() as u32 != ie.ctime_sec {
            return false;
        }
        if meta.ctime_nsec() as u32 != ie.ctime_nsec {
            return false;
        }
        // Compare inode. Device (`st_dev`) is deliberately NOT compared: Git
        // leaves `USE_STDEV` off by default because device numbers aren't stable
        // (across remounts, and some index writers store 0), so comparing it
        // forces a needless re-hash of the whole tree. Match Git and ignore it.
        if meta.ino() as u32 != ie.ino {
            return false;
        }
    }
    #[cfg(not(unix))]
    {
        use std::time::UNIX_EPOCH;
        if let Ok(mtime) = meta.modified() {
            if let Ok(dur) = mtime.duration_since(UNIX_EPOCH) {
                if dur.as_secs() as u32 != ie.mtime_sec {
                    return false;
                }
                if dur.subsec_nanos() != ie.mtime_nsec {
                    return false;
                }
            }
        }
    }
    true
}

/// Refresh cached stat data for stage-0 file/symlink entries whose worktree content still matches
/// the recorded OID but whose on-disk stat went stale.
///
/// This mirrors Git's `refresh_index` / `refresh_cache_ent`: an entry is only marked clean (stat
/// adopted from the worktree) after its content is re-verified against the index OID. A genuinely
/// modified entry keeps its stale stat so `diff-files` / `status` continue to report it. Operations
/// that rewrite the worktree (`status`, `reset --mixed`, `stash`) call this before writing the
/// index so a subsequent `git diff-files` sees refreshed entries as clean.
///
/// Gitlinks, sparse (`skip_worktree`), `assume_unchanged` and intent-to-add entries are skipped.
/// The blob comparison is a raw-content hash, so a CRLF-smudged match is conservatively missed
/// (the entry simply stays stat-dirty and is re-hashed next time — never the reverse).
///
/// `index_mtime` is the on-disk index file's `(mtime_sec, mtime_nsec)` (see
/// `entry_is_racy` / Git `is_racy_timestamp`); pass `None` when unknown — racy detection is
/// then skipped, which is conservative for tree-built indexes whose zeroed stat never matches.
///
/// Returns `true` when at least one entry was refreshed or invalidated, so callers can write
/// the index opportunistically (Git only persists a refresh that changed something).
pub fn refresh_index_stat_content_verified(
    index: &mut Index,
    work_tree: &Path,
    index_mtime: Option<(u32, u32)>,
) -> bool {
    use crate::index::{MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};
    let mut changed = false;
    for ie in &mut index.entries {
        if ie.stage() != 0 || ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
            continue;
        }
        if ie.mode != MODE_REGULAR && ie.mode != MODE_EXECUTABLE && ie.mode != MODE_SYMLINK {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&ie.path) else {
            continue;
        };
        let abs = work_tree.join(path);
        let Ok(meta) = fs::symlink_metadata(&abs) else {
            continue;
        };
        if stat_matches(ie, &meta) {
            // Git `ie_match_stat`: a clean stat is trusted without reading the file unless the
            // entry is racy (written within the index's own mtime). Only then re-verify content;
            // stat can be refreshed from the work tree without matching the indexed blob (e.g.
            // after merge stat refresh while local edits remain) — invalidate so diff/status
            // re-hash.
            if entry_is_racy(ie, index_mtime)
                && !worktree_content_matches_index_oid(ie, &abs, &meta)
            {
                invalidate_index_stat_cache(ie);
                changed = true;
            }
            continue;
        }
        if !worktree_content_matches_index_oid(ie, &abs, &meta) {
            continue;
        }
        let refreshed = crate::index::entry_from_metadata(&meta, &ie.path, ie.oid, ie.mode);
        ie.ctime_sec = refreshed.ctime_sec;
        ie.ctime_nsec = refreshed.ctime_nsec;
        ie.mtime_sec = refreshed.mtime_sec;
        ie.mtime_nsec = refreshed.mtime_nsec;
        ie.dev = refreshed.dev;
        ie.ino = refreshed.ino;
        ie.uid = refreshed.uid;
        ie.gid = refreshed.gid;
        ie.size = refreshed.size;
        changed = true;
    }
    changed
}

/// Symlink target as the byte string Git hashes for the blob OID.
///
/// On Unix the raw `OsStr` bytes are used verbatim. On Windows `OsStr` is WTF-8
/// and has no stable byte view, so the lossy UTF-8 form is used (symlink targets
/// in Git trees are UTF-8 in practice).
fn symlink_target_bytes(target: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        target.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        target.as_os_str().to_string_lossy().as_bytes().to_vec()
    }
}

/// Whether the work tree blob at `abs` matches the index entry OID (raw bytes, no CRLF smudge).
fn worktree_content_matches_index_oid(ie: &IndexEntry, abs: &Path, meta: &fs::Metadata) -> bool {
    use crate::index::{MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};
    if ie.mode == MODE_SYMLINK {
        if !meta.file_type().is_symlink() {
            return false;
        }
        fs::read_link(abs)
            .map(|t| Odb::hash_object_data(ObjectKind::Blob, &symlink_target_bytes(&t)) == ie.oid)
            .unwrap_or(false)
    } else if ie.mode == MODE_REGULAR || ie.mode == MODE_EXECUTABLE {
        if !meta.file_type().is_file() {
            return false;
        }
        fs::read(abs)
            .map(|bytes| Odb::hash_object_data(ObjectKind::Blob, &bytes) == ie.oid)
            .unwrap_or(false)
    } else {
        false
    }
}

/// Clear cached stat fields so the next diff/status pass re-reads the work tree.
fn invalidate_index_stat_cache(ie: &mut IndexEntry) {
    ie.ctime_sec = 0;
    ie.ctime_nsec = 0;
    ie.mtime_sec = 0;
    ie.mtime_nsec = 0;
    ie.dev = 0;
    ie.ino = 0;
    ie.size = 0;
}

pub fn hash_worktree_file(
    odb: &Odb,
    path: &Path,
    meta: &fs::Metadata,
    conv: &crate::crlf::ConversionConfig,
    file_attrs: &crate::crlf::FileAttrs,
    rel_path: &str,
    index_entry: Option<&IndexEntry>,
) -> Result<ObjectId> {
    let prior_blob: Option<Vec<u8>> = index_entry
        .filter(|e| e.oid != zero_oid())
        .and_then(|e| odb.read(&e.oid).ok().map(|o| o.data));
    let data = if meta.file_type().is_symlink() {
        // For symlinks, hash the target path
        let target = fs::read_link(path)?;
        target.to_string_lossy().into_owned().into_bytes()
    } else if meta.is_dir() {
        // `read()` on a directory fails with EISDIR; unmerged paths may leave an empty
        // placeholder directory (e.g. t4027 combined submodule conflict).
        Vec::new()
    } else {
        let raw = fs::read(path)?;
        // Apply clean conversion (CRLF→LF) so hash matches index blob.
        // Do not run safecrlf here: diff/commit use this for hashing and must not print warnings.
        let opts = crate::crlf::ConvertToGitOpts {
            index_blob: prior_blob.as_deref(),
            renormalize: false,
            check_safecrlf: false,
        };
        crate::crlf::convert_to_git_with_opts(&raw, rel_path, conv, file_attrs, opts).unwrap_or(raw)
    };

    Ok(Odb::hash_object_data(ObjectKind::Blob, &data))
}

/// Derive a Git file mode from filesystem metadata.
pub fn mode_from_metadata(meta: &fs::Metadata) -> u32 {
    if meta.file_type().is_symlink() {
        0o120000
    } else {
        #[cfg(unix)]
        {
            if meta.mode() & 0o111 != 0 {
                return 0o100755;
            }
        }
        0o100644
    }
}

/// Compare a tree against the working tree.
///
/// Shows changes from `tree_oid` to the current working directory state.
/// Files tracked in the index but not in the tree are shown as Added.
/// Files in the tree but missing from the working tree are shown as Deleted.
///
/// # Parameters
///
/// - `odb` — object database.
/// - `tree_oid` — the tree to compare against (`None` for empty tree).
/// - `work_tree` — path to the working tree root.
/// - `index` — current index (used to discover new tracked files not in tree).
///
/// # Errors
///
/// Returns errors from ODB reads or I/O.
pub fn diff_tree_to_worktree(
    odb: &Odb,
    tree_oid: Option<&ObjectId>,
    work_tree: &Path,
    index: &Index,
) -> Result<Vec<DiffEntry>> {
    use crate::config::ConfigSet;
    use crate::crlf;

    let git_dir = work_tree.join(".git");
    let config = ConfigSet::load(Some(&git_dir), true).unwrap_or_else(|_| ConfigSet::new());
    let conv = crlf::ConversionConfig::from_config(&config);
    let attrs = crlf::load_gitattributes(work_tree);

    // Flatten the tree into a BTreeMap keyed by path
    let tree_flat = match tree_oid {
        Some(oid) => flatten_tree(odb, oid, "")?,
        None => Vec::new(),
    };
    let tree_map: std::collections::BTreeMap<String, &FlatEntry> =
        tree_flat.iter().map(|e| (e.path.clone(), e)).collect();

    // Build index lookup: path → &IndexEntry (stage 0 only)
    let mut index_entries: std::collections::BTreeMap<&[u8], &IndexEntry> =
        std::collections::BTreeMap::new();
    let mut index_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stage0_paths: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    for ie in &index.entries {
        if ie.stage() != 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&ie.path).to_string();
        index_entries.insert(&ie.path, ie);
        index_paths.insert(path);
        stage0_paths.insert(ie.path.clone());
    }

    // Paths with only unmerged stages (1–3) and no stage 0 — `git diff <rev>` must still list them
    // so combined `diff --cc` conflict hunks can be emitted (`t4108-apply-threeway`).
    let mut unmerged_only_paths: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for ie in &index.entries {
        if !(1..=3).contains(&ie.stage()) {
            continue;
        }
        if stage0_paths.contains(&ie.path) {
            continue;
        }
        unmerged_only_paths.insert(String::from_utf8_lossy(&ie.path).into_owned());
    }

    // Union of tree paths + index paths
    let mut all_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    all_paths.extend(tree_map.keys().cloned());
    all_paths.extend(index_paths.iter().cloned());
    all_paths.extend(unmerged_only_paths.iter().cloned());

    let mut result = Vec::new();

    for path in &all_paths {
        if index_entries
            .get(path.as_bytes())
            .is_some_and(|ie| ie.skip_worktree())
        {
            // Sparse checkout: `git diff <rev>` does not report tree↔worktree drift for
            // skip-worktree paths (they are outside the sparse cone). Matches t7012 stash flow.
            continue;
        }

        let tree_entry = tree_map.get(path.as_str());

        // Gitlink entries (submodules) — compare HEAD commit, not file content.
        let is_gitlink = tree_entry.is_some_and(|te| te.mode == 0o160000)
            || index_entries
                .get(path.as_bytes())
                .is_some_and(|ie| ie.mode == 0o160000);
        if is_gitlink {
            let sub_dir = work_tree.join(path);
            let index_gitlink_oid = index_entries
                .get(path.as_bytes())
                .filter(|ie| ie.mode == 0o160000)
                .map(|ie| ie.oid);
            match (tree_entry, index_gitlink_oid) {
                (Some(te), _) => {
                    let sub_head = read_submodule_head_oid(&sub_dir);
                    // A gitlink whose worktree directory no longer exists is a deleted submodule:
                    // Git's `diff-lib.c` reports it as a deletion (status `D`, new mode 000000) and
                    // `--submodule` renders the `(submodule deleted)` summary (t4041 #46).
                    if sub_head.is_none() && !sub_dir.exists() {
                        result.push(DiffEntry {
                            status: DiffStatus::Deleted,
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: format_mode(te.mode),
                            new_mode: "000000".to_string(),
                            old_oid: te.oid,
                            new_oid: zero_oid(),
                            score: None,
                        });
                        continue;
                    }
                    let index_matches_tree = index_gitlink_oid.is_some_and(|oid| oid == te.oid);
                    let head_differs = sub_head.as_ref() != Some(&te.oid);
                    let dirty_while_aligned = index_matches_tree
                        && !head_differs
                        && submodule_has_dirty_worktree_for_super_diff(work_tree, path, &te.oid);
                    if head_differs || dirty_while_aligned {
                        // Raw `git diff <tree>` lines use a null OID on the worktree side when the
                        // checked-out submodule HEAD differs from the tree's gitlink; patch output
                        // still resolves the real commit from the submodule directory.
                        let new_oid = if head_differs { zero_oid() } else { te.oid };
                        result.push(DiffEntry {
                            status: DiffStatus::Modified,
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: format_mode(te.mode),
                            new_mode: format_mode(te.mode),
                            old_oid: te.oid,
                            new_oid,
                            score: None,
                        });
                    }
                }
                (None, Some(idx_oid)) => {
                    // Gitlink staged in the index but absent from the tree: a new submodule. Git
                    // reports it as an addition (status `A`) with the index gitlink on the new side,
                    // so `--submodule` renders `(new submodule)` (t4041 #46). The patch renderer
                    // resolves the real commit from the index OID / submodule HEAD.
                    let new_oid = read_submodule_head_oid(&sub_dir).unwrap_or(idx_oid);
                    result.push(DiffEntry {
                        status: DiffStatus::Added,
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: "000000".to_string(),
                        new_mode: format_mode(0o160000),
                        old_oid: zero_oid(),
                        new_oid,
                        score: None,
                    });
                }
                (None, None) => {}
            }
            continue;
        }

        let file_path = work_tree.join(path);

        let wt_meta = match fs::symlink_metadata(&file_path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::Io(e)),
        };

        if unmerged_only_paths.contains(path) {
            if let (Some(te), Some(meta)) = (tree_entry, wt_meta.as_ref()) {
                let file_attrs = crlf::get_file_attrs(&attrs, path, false, &config);
                let wt_oid =
                    hash_worktree_file(odb, &file_path, meta, &conv, &file_attrs, path, None)?;
                let wt_mode = mode_from_metadata(meta);
                if wt_oid != te.oid || wt_mode != te.mode {
                    result.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: format_mode(te.mode),
                        new_mode: format_mode(wt_mode),
                        old_oid: te.oid,
                        new_oid: wt_oid,
                        score: None,
                    });
                }
            }
            continue;
        }

        match (tree_entry, wt_meta) {
            (Some(te), Some(ref meta)) => {
                let wt_mode = mode_from_metadata(meta);
                let Some(ie) = index_entries.get(path.as_bytes()) else {
                    continue;
                };

                let index_matches_tree = ie.oid == te.oid && ie.mode == te.mode;

                // Fully clean: index matches `HEAD`, worktree matches index, stat cache fresh.
                if index_matches_tree && wt_mode == te.mode && stat_matches(ie, meta) {
                    continue;
                }

                let file_attrs = crlf::get_file_attrs(&attrs, path, false, &config);
                let idx_ent = index_entries.get(path.as_bytes()).copied();

                // Staged mode (same blob as `HEAD`, different mode recorded in the index).
                if ie.oid == te.oid && ie.mode != te.mode {
                    result.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: format_mode(te.mode),
                        new_mode: format_mode(ie.mode),
                        old_oid: te.oid,
                        new_oid: te.oid,
                        score: None,
                    });
                    continue;
                }

                // Index still matches `HEAD`: only unstaged worktree drift (content and/or
                // worktree-only exec bit when `update-index` was not run — t4049 harness).
                if index_matches_tree {
                    let wt_oid = hash_worktree_file(
                        odb,
                        &file_path,
                        meta,
                        &conv,
                        &file_attrs,
                        path,
                        idx_ent,
                    )?;
                    let mut eff_oid = wt_oid;
                    if eff_oid != te.oid {
                        if let Ok(raw) = fs::read(&file_path) {
                            let raw_oid = Odb::hash_object_data(ObjectKind::Blob, &raw);
                            if raw_oid == te.oid {
                                eff_oid = te.oid;
                            }
                        }
                    }
                    if eff_oid != te.oid {
                        result.push(DiffEntry {
                            status: DiffStatus::Modified,
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: format_mode(te.mode),
                            new_mode: format_mode(wt_mode),
                            old_oid: te.oid,
                            new_oid: eff_oid,
                            score: None,
                        });
                    } else if wt_mode != te.mode {
                        result.push(DiffEntry {
                            status: DiffStatus::Modified,
                            old_path: Some(path.clone()),
                            new_path: Some(path.clone()),
                            old_mode: format_mode(te.mode),
                            new_mode: format_mode(wt_mode),
                            old_oid: te.oid,
                            new_oid: te.oid,
                            score: None,
                        });
                    }
                    continue;
                }

                // Staged content (and possibly mode): `git diff <rev>` is tree vs working tree.
                let wt_oid =
                    hash_worktree_file(odb, &file_path, meta, &conv, &file_attrs, path, idx_ent)?;
                let mut eff_oid = wt_oid;
                if eff_oid != te.oid {
                    if let Ok(raw) = fs::read(&file_path) {
                        let raw_oid = Odb::hash_object_data(ObjectKind::Blob, &raw);
                        if raw_oid == te.oid {
                            eff_oid = te.oid;
                        }
                    }
                }
                if eff_oid != te.oid || wt_mode != te.mode {
                    result.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        old_mode: format_mode(te.mode),
                        new_mode: format_mode(wt_mode),
                        old_oid: te.oid,
                        new_oid: eff_oid,
                        score: None,
                    });
                }
            }
            (Some(te), None) => {
                // In tree but missing from worktree
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path.clone()),
                    new_path: None,
                    old_mode: format_mode(te.mode),
                    new_mode: "000000".to_owned(),
                    old_oid: te.oid,
                    new_oid: zero_oid(),
                    score: None,
                });
            }
            (None, Some(ref meta)) => {
                // In index but not in tree, and exists in worktree
                let file_attrs = crlf::get_file_attrs(&attrs, path, false, &config);
                let wt_oid = hash_worktree_file(
                    odb,
                    &file_path,
                    meta,
                    &conv,
                    &file_attrs,
                    path,
                    index_entries.get(path.as_bytes()).copied(),
                )?;
                let wt_mode = mode_from_metadata(meta);
                result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path.clone()),
                    old_mode: "000000".to_owned(),
                    new_mode: format_mode(wt_mode),
                    old_oid: zero_oid(),
                    new_oid: wt_oid,
                    score: None,
                });
            }
            (None, None) => {
                // Tracked in index but neither in tree nor worktree — skip
            }
        }
    }

    result.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(result)
}

// ── Rename detection ────────────────────────────────────────────────

fn read_added_entry_bytes(
    odb: &Odb,
    entry: &DiffEntry,
    work_root: Option<&Path>,
) -> Option<Vec<u8>> {
    if entry.new_oid != zero_oid() {
        return odb.read(&entry.new_oid).ok().map(|obj| obj.data);
    }
    let path = entry.new_path.as_deref()?;
    let root = work_root?;
    fs::read(root.join(path)).ok()
}

fn modified_as_copy_from_sources(
    odb: &Odb,
    work_root: Option<&Path>,
    e: &DiffEntry,
    threshold: u32,
    sources: &[(String, ObjectId, bool)],
    source_contents: &[Option<Vec<u8>>],
    source_tree_entries: &[(String, String, ObjectId)],
) -> Option<DiffEntry> {
    fn regular_file_mode(mode: &str) -> bool {
        mode == "100644" || mode == "100755"
    }

    if e.status != DiffStatus::Modified || !regular_file_mode(&e.new_mode) {
        return None;
    }
    let new_data = read_added_entry_bytes(odb, e, work_root)?;
    let new_oid_eff = if e.new_oid != zero_oid() {
        e.new_oid
    } else {
        Odb::hash_object_data(ObjectKind::Blob, &new_data)
    };

    let mut best: Option<(usize, u32)> = None;
    for (si, (src_path, src_oid, is_deleted)) in sources.iter().enumerate() {
        if *is_deleted {
            continue;
        }
        if e.new_path.as_deref() == Some(src_path.as_str()) {
            continue;
        }
        let src_mode_str = source_tree_entries
            .iter()
            .find(|(p, _, _)| p == src_path)
            .map(|(_, m, _)| m.as_str())
            .unwrap_or("100644");
        if !regular_file_mode(src_mode_str) {
            continue;
        }

        let score = if *src_oid == new_oid_eff {
            100
        } else {
            match (&source_contents[si], Some(new_data.as_slice())) {
                (Some(old_data), Some(nd)) => compute_similarity(old_data, nd),
                _ => 0,
            }
        };
        if score >= threshold {
            let replace = match best {
                None => true,
                Some((_, s)) => score > s,
            };
            if replace {
                best = Some((si, score));
            }
        }
    }

    let (si, score) = best?;
    let (src_path, src_oid, _) = &sources[si];
    let src_mode = source_tree_entries
        .iter()
        .find(|(p, _, _)| p == src_path)
        .map(|(_, m, _)| m.clone())
        .unwrap_or_else(|| e.old_mode.clone());

    Some(DiffEntry {
        status: DiffStatus::Copied,
        old_path: Some(src_path.clone()),
        new_path: e.new_path.clone(),
        old_mode: src_mode,
        new_mode: e.new_mode.clone(),
        old_oid: *src_oid,
        new_oid: e.new_oid,
        score: Some(score),
    })
}

/// Detect renames by pairing Deleted and Added entries with similar content.
///
/// `threshold` is the minimum similarity percentage (0–100) for a pair to
/// be considered a rename (Git's default is 50%).  The function reads blob
/// content from the ODB to compute a line-level similarity score.
///
/// Exact-OID matches are always 100% similar regardless of content.
///
/// When `work_root` is set, added entries whose `new_oid` is the zero placeholder (as in
/// uncached `diff-index` when the work tree diverged from the index) load content from disk
/// under that root instead of the object database.
pub fn detect_renames(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    threshold: u32,
) -> Vec<DiffEntry> {
    // Split entries into deleted, added, and others.
    let mut deleted: Vec<DiffEntry> = Vec::new();
    let mut added: Vec<DiffEntry> = Vec::new();
    let mut others: Vec<DiffEntry> = Vec::new();

    for entry in entries {
        match entry.status {
            DiffStatus::Deleted => deleted.push(entry),
            DiffStatus::Added => added.push(entry),
            _ => others.push(entry),
        }
    }

    if deleted.is_empty() || added.is_empty() {
        // Nothing to pair — return original order.
        let mut result = others;
        result.extend(deleted);
        result.extend(added);
        result.sort_by(|a, b| a.path().cmp(b.path()));
        return result;
    }

    // Read content for all deleted blobs.
    let deleted_contents: Vec<Option<Vec<u8>>> = deleted
        .iter()
        .map(|d| odb.read(&d.old_oid).ok().map(|obj| obj.data))
        .collect();

    // Read content for all added blobs.
    let added_contents: Vec<Option<Vec<u8>>> = added
        .iter()
        .map(|a| read_added_entry_bytes(odb, a, work_root))
        .collect();

    // Build a matrix of similarity scores and find the best pairings.
    // We use a greedy approach: pick the highest-scoring pair first.
    let mut scores: Vec<(u32, usize, usize)> = Vec::new();

    fn is_regularish_mode(mode: &str) -> bool {
        mode == "100644" || mode == "100755"
    }

    fn same_path_same_blob(del: &DiffEntry, add: &DiffEntry) -> bool {
        del.old_path == add.new_path && del.old_oid == add.new_oid && del.old_mode == add.new_mode
    }

    for (di, del) in deleted.iter().enumerate() {
        for (ai, add) in added.iter().enumerate() {
            // Exact OID match → 100%
            if del.old_oid == add.new_oid {
                scores.push((100, di, ai));
                continue;
            }

            // Do not use line similarity across file types (e.g. regular ↔ symlink); Git keeps these
            // as separate changes (`t4008-diff-break-rewrite` #7).
            if !is_regularish_mode(&del.old_mode) || !is_regularish_mode(&add.new_mode) {
                continue;
            }

            let score = match (&deleted_contents[di], &added_contents[ai]) {
                (Some(old_data), Some(new_data)) => compute_similarity(old_data, new_data),
                _ => 0,
            };

            if score >= threshold {
                scores.push((score, di, ai));
            }
        }
    }

    // Sort: prefer real path-changing pairs before same-path no-op pairs, then
    // same-basename pairs, then by score descending.
    // This matches Git's behavior where basename matches are checked first.
    scores.sort_by(|a, b| {
        let a_noop = same_path_same_blob(&deleted[a.1], &added[a.2]);
        let b_noop = same_path_same_blob(&deleted[b.1], &added[b.2]);
        let a_same = same_basename(&deleted[a.1], &added[a.2]);
        let b_same = same_basename(&deleted[b.1], &added[b.2]);
        a_noop
            .cmp(&b_noop)
            .then_with(|| b_same.cmp(&a_same))
            .then_with(|| b.0.cmp(&a.0))
    });

    let mut used_deleted = vec![false; deleted.len()];
    let mut used_added = vec![false; added.len()];
    let mut renames: Vec<DiffEntry> = Vec::new();

    for (score, di, ai) in &scores {
        if used_deleted[*di] || used_added[*ai] {
            continue;
        }
        used_deleted[*di] = true;
        used_added[*ai] = true;

        let del = &deleted[*di];
        let add = &added[*ai];

        // A "rename" whose source and destination are the same path with the
        // same blob is not a change at all (this arises with pathological
        // duplicate tree entries, t4058). Git pairs and then drops it, leaving
        // no diff entry; mirror that by skipping emission.
        if same_path_same_blob(del, add) {
            continue;
        }

        renames.push(DiffEntry {
            status: DiffStatus::Renamed,
            old_path: del.old_path.clone(),
            new_path: add.new_path.clone(),
            old_mode: del.old_mode.clone(),
            new_mode: add.new_mode.clone(),
            old_oid: del.old_oid,
            new_oid: add.new_oid,
            score: Some(*score),
        });
    }

    // Collect unmatched entries.
    let mut result = others;
    result.extend(renames);
    for (i, entry) in deleted.into_iter().enumerate() {
        if !used_deleted[i] {
            result.push(entry);
        }
    }
    for (i, entry) in added.into_iter().enumerate() {
        if !used_added[i] {
            result.push(entry);
        }
    }

    result.sort_by(|a, b| a.path().cmp(b.path()));
    result
}

/// Detect copies among diff entries.
///
/// This first runs rename detection (pairing Deleted+Added), then for any
/// remaining Added entries, looks for copy sources.
///
/// - `find_copies_harder` = false: only Modified entries are copy source candidates.
/// - `find_copies_harder` = true: also examine unmodified files from `source_tree_entries`.
///
/// `source_tree_entries` should be a list of (path, mode, oid) from the source tree;
/// used when `find_copies_harder` is true to consider unmodified files as copy sources.
pub fn detect_copies(
    odb: &Odb,
    work_root: Option<&Path>,
    entries: Vec<DiffEntry>,
    threshold: u32,
    find_copies_harder: bool,
    source_tree_entries: &[(String, String, ObjectId)],
) -> Vec<DiffEntry> {
    use std::collections::{HashMap, HashSet};

    // Separate entries by status.
    let mut deleted: Vec<DiffEntry> = Vec::new();
    let mut added: Vec<DiffEntry> = Vec::new();
    let mut others: Vec<DiffEntry> = Vec::new();

    for entry in entries {
        match entry.status {
            DiffStatus::Deleted => deleted.push(entry),
            DiffStatus::Added => added.push(entry),
            _ => others.push(entry),
        }
    }

    // Build source candidates: deleted files, modified files, and optionally tree entries.
    // Track which sources are from deleted files (can become renames).
    let mut sources: Vec<(String, ObjectId, bool)> = Vec::new(); // (path, oid, is_deleted)
    let mut deleted_source_idx: HashMap<String, usize> = HashMap::new();

    for entry in &deleted {
        if let Some(ref path) = entry.old_path {
            deleted_source_idx.insert(path.clone(), sources.len());
            sources.push((path.clone(), entry.old_oid, true));
        }
    }

    // Modified and type-changed files are candidates for `-C` (e.g. symlink rewrite leaves the
    // old blob available as a copy source for another path; see `t4008-diff-break-rewrite`).
    for entry in &others {
        if matches!(entry.status, DiffStatus::Modified | DiffStatus::TypeChanged) {
            if let Some(ref old_path) = entry.old_path {
                if !sources.iter().any(|(p, _, _)| p == old_path) {
                    sources.push((old_path.clone(), entry.old_oid, false));
                }
            }
        }
    }

    // With find_copies_harder, add all source tree entries.
    if find_copies_harder {
        for (path, _mode, oid) in source_tree_entries {
            if !sources.iter().any(|(p, _, _)| p == path) {
                sources.push((path.clone(), *oid, false));
            }
        }
    }

    if sources.is_empty() {
        let mut result = others;
        result.extend(deleted);
        result.extend(added);
        result.sort_by(|a, b| a.path().cmp(b.path()));
        return result;
    }

    // Read content for sources.
    let source_contents: Vec<Option<Vec<u8>>> = sources
        .iter()
        .map(|(_, oid, _)| odb.read(oid).ok().map(|obj| obj.data))
        .collect();

    let mut result_entries: Vec<DiffEntry> = Vec::new();
    let mut renamed_deleted: HashSet<usize> = HashSet::new();
    let mut used_added2 = vec![false; added.len()];

    if !added.is_empty() {
        // Read content for added blobs.
        let added_contents: Vec<Option<Vec<u8>>> = added
            .iter()
            .map(|a| read_added_entry_bytes(odb, a, work_root))
            .collect();

        // Build score matrix: (score, source_idx, added_idx)
        let mut scores: Vec<(u32, usize, usize)> = Vec::new();
        for (si, (src_path, src_oid, _)) in sources.iter().enumerate() {
            for (ai, add) in added.iter().enumerate() {
                // Never pair a path with itself as copy source (matches Git; avoids
                // arbitrary tie-breaking when several sources share the same blob).
                if add.new_path.as_deref() == Some(src_path.as_str()) {
                    continue;
                }
                let add_oid = if add.new_oid != zero_oid() {
                    add.new_oid
                } else if let Some(ref data) = added_contents[ai] {
                    Odb::hash_object_data(ObjectKind::Blob, data)
                } else {
                    zero_oid()
                };
                if *src_oid == add_oid {
                    scores.push((100, si, ai));
                    continue;
                }
                let score = match (&source_contents[si], &added_contents[ai]) {
                    (Some(old_data), Some(new_data)) => compute_similarity(old_data, new_data),
                    _ => 0,
                };
                if score >= threshold {
                    scores.push((score, si, ai));
                }
            }
        }

        // Sort by score descending.
        scores.sort_by(|a, b| b.0.cmp(&a.0));

        // Build source->added mappings, each added file assigned to best source.
        let mut used_added = vec![false; added.len()];
        let mut source_to_added: HashMap<usize, Vec<(usize, u32)>> = HashMap::new();
        for &(score, si, ai) in &scores {
            if used_added[ai] {
                continue;
            }
            used_added[ai] = true;
            source_to_added.entry(si).or_default().push((ai, score));
        }

        // For each deleted source, pick one assignment as Rename, rest as Copy.
        for (&si, assignments_for_src) in &source_to_added {
            let (_, _, is_deleted) = &sources[si];
            if *is_deleted && !assignments_for_src.is_empty() {
                // Pick the last one (by path) as the rename target.
                // Git tends to pick the rename as the last alphabetically.
                let rename_ai = assignments_for_src
                    .iter()
                    .max_by_key(|(ai, _score)| added[*ai].path().to_string())
                    .map(|(ai, _)| *ai);

                for &(ai, score) in assignments_for_src {
                    let (ref src_path, _, _) = sources[si];
                    let add = &added[ai];
                    let src_mode = source_tree_entries
                        .iter()
                        .find(|(p, _, _)| p == src_path)
                        .map(|(_, m, _)| m.clone())
                        .unwrap_or_else(|| add.old_mode.clone());

                    let is_rename = Some(ai) == rename_ai;
                    result_entries.push(DiffEntry {
                        status: if is_rename {
                            DiffStatus::Renamed
                        } else {
                            DiffStatus::Copied
                        },
                        old_path: Some(src_path.clone()),
                        new_path: add.new_path.clone(),
                        old_mode: src_mode,
                        new_mode: add.new_mode.clone(),
                        old_oid: sources[si].1,
                        new_oid: add.new_oid,
                        score: Some(score),
                    });
                    used_added2[ai] = true;
                }
                renamed_deleted.insert(si);
            } else {
                // Non-deleted source: all assignments are copies.
                for &(ai, score) in assignments_for_src {
                    let (ref src_path, _, _) = sources[si];
                    let add = &added[ai];
                    let src_mode = source_tree_entries
                        .iter()
                        .find(|(p, _, _)| p == src_path)
                        .map(|(_, m, _)| m.clone())
                        .unwrap_or_else(|| add.old_mode.clone());

                    result_entries.push(DiffEntry {
                        status: DiffStatus::Copied,
                        old_path: Some(src_path.clone()),
                        new_path: add.new_path.clone(),
                        old_mode: src_mode,
                        new_mode: add.new_mode.clone(),
                        old_oid: sources[si].1,
                        new_oid: add.new_oid,
                        score: Some(score),
                    });
                    used_added2[ai] = true;
                }
            }
        }
    }

    // Keep deleted entries that weren't consumed by a rename.
    for entry in deleted.into_iter() {
        if let Some(ref path) = entry.old_path {
            if let Some(&si) = deleted_source_idx.get(path) {
                if renamed_deleted.contains(&si) {
                    // This deletion was consumed by a rename; skip it.
                    continue;
                }
            }
        }
        result_entries.push(entry);
    }

    let mut result = others;
    result.extend(result_entries);
    // Keep unmatched added entries.
    for (i, entry) in added.into_iter().enumerate() {
        if !used_added2[i] {
            result.push(entry);
        }
    }

    let mut final_result = Vec::with_capacity(result.len());
    for e in result {
        if let Some(c) = modified_as_copy_from_sources(
            odb,
            work_root,
            &e,
            threshold,
            &sources,
            &source_contents,
            source_tree_entries,
        ) {
            final_result.push(c);
        } else {
            final_result.push(e);
        }
    }

    final_result.sort_by(|a, b| a.path().cmp(b.path()));
    final_result
}

/// Apply Git-style rename and optional copy detection for index↔worktree diffs.
///
/// When `copies` is true (Git `diff.renames` / `status.renames` set to `copy`/`copies`),
/// runs copy detection, which also pairs deleted sources with one rename and any additional
/// destinations as copies.
///
/// # Errors
///
/// Propagates errors from reading the `head_tree` object from `odb`.
pub fn status_apply_rename_copy_detection(
    odb: &Odb,
    unstaged_raw: Vec<DiffEntry>,
    threshold: u32,
    copies: bool,
    head_tree: Option<&ObjectId>,
) -> Result<Vec<DiffEntry>> {
    if !copies {
        return Ok(detect_renames(odb, None, unstaged_raw, threshold));
    }
    let source_tree_entries: Vec<(String, String, ObjectId)> = match head_tree {
        Some(oid) => flatten_tree(odb, oid, "")?
            .into_iter()
            .map(|e| (e.path, format_mode(e.mode), e.oid))
            .collect(),
        None => Vec::new(),
    };
    Ok(detect_copies(
        odb,
        None,
        unstaged_raw,
        threshold,
        false,
        &source_tree_entries,
    ))
}

/// Format a rename pair using Git's compact path format.
///
/// Examples:
/// - `a/b/c` → `c/b/a` → `a/b/c => c/b/a`
/// - `c/b/a` → `c/d/e` → `c/{b/a => d/e}`
/// - `c/d/e` → `d/e` → `{c/d => d}/e`
/// - `d/e` → `d/f/e` → `d/{ => f}/e`
pub fn format_rename_path(old: &str, new: &str) -> String {
    let ob = old.as_bytes();
    let nb = new.as_bytes();

    // Find common prefix length, snapped to '/' boundary.
    let pfx = {
        let mut last_sep = 0usize;
        let min_len = ob.len().min(nb.len());
        for i in 0..min_len {
            if ob[i] != nb[i] {
                break;
            }
            if ob[i] == b'/' {
                last_sep = i + 1;
            }
        }
        last_sep
    };

    // Find common suffix length, snapped to '/' boundary.
    let mut sfx = {
        let mut last_sep = 0usize;
        let min_len = ob.len().min(nb.len());
        for i in 0..min_len {
            let oi = ob.len() - 1 - i;
            let ni = nb.len() - 1 - i;
            if ob[oi] != nb[ni] {
                break;
            }
            if ob[oi] == b'/' {
                last_sep = i + 1;
            }
        }
        last_sep
    };

    // Suffix starts at this position in each string.
    let mut sfx_at_old = ob.len() - sfx;
    let mut sfx_at_new = nb.len() - sfx;

    // If prefix and suffix overlap in both strings (both middles empty),
    // reduce the suffix so that at least the longer string has a non-empty middle.
    while pfx > sfx_at_old && pfx > sfx_at_new && sfx > 0 {
        // Reduce suffix by snapping to the next smaller '/' boundary.
        let suffix_bytes = &ob[sfx_at_old..];
        let mut new_sfx = 0;
        // Find the next '/' after sfx_at_old (i.e., reduce suffix).
        for (i, &b) in suffix_bytes.iter().enumerate().skip(1) {
            if b == b'/' {
                new_sfx = sfx - i;
                break;
            }
        }
        if new_sfx == 0 || new_sfx >= sfx {
            sfx_at_old = ob.len();
            sfx_at_new = nb.len();
            break;
        }
        sfx = new_sfx;
        sfx_at_old = ob.len() - sfx;
        sfx_at_new = nb.len() - sfx;
    }

    // When prefix and suffix overlap in the shorter string, they share
    // the '/' boundary character. In the output format, the shared '/'
    // appears in both positions (e.g. "d/{ => f}/e" for d/e → d/f/e).
    // Compute the middle parts. When prefix and suffix overlap in a
    // string, the middle for that string is empty. The shared '/' shows
    // in both prefix (trailing) and suffix (leading) positions.
    let prefix = &old[..pfx];
    let suffix = &old[sfx_at_old..];
    let old_mid = if pfx <= sfx_at_old {
        &old[pfx..sfx_at_old]
    } else {
        ""
    };
    let new_mid = if pfx <= sfx_at_new {
        &new[pfx..sfx_at_new]
    } else {
        ""
    };

    if prefix.is_empty() && suffix.is_empty() {
        return format!("{old} => {new}");
    }

    format!("{prefix}{{{old_mid} => {new_mid}}}{suffix}")
}

/// Check if two entries share the same filename (basename).
fn same_basename(del: &DiffEntry, add: &DiffEntry) -> bool {
    let old = del.old_path.as_deref().unwrap_or("");
    let new = add.new_path.as_deref().unwrap_or("");
    let old_base = old.rsplit('/').next().unwrap_or(old);
    let new_base = new.rsplit('/').next().unwrap_or(new);
    old_base == new_base && !old_base.is_empty()
}

/// Compute a similarity percentage (0–100) between two byte slices.
///
/// Uses Git's approach: count the bytes that are "shared" (appear in
/// equal lines), then compute `score = shared_bytes * 2 * 100 / (src_size + dst_size)`.
fn compute_similarity(old: &[u8], new: &[u8]) -> u32 {
    // Normalize CRLF → LF before comparing so that files differing
    // only in line endings are detected as renames.
    let old_norm = crate::crlf::crlf_to_lf(old);
    let new_norm = crate::crlf::crlf_to_lf(new);

    let src_size = old_norm.len();
    let dst_size = new_norm.len();

    if src_size == 0 && dst_size == 0 {
        return 100;
    }
    let total = src_size + dst_size;
    if total == 0 {
        return 100;
    }

    // Use line-level diff to find shared content, then count bytes.
    use similar::{ChangeTag, TextDiff};
    let old_str = String::from_utf8_lossy(&old_norm);
    let new_str = String::from_utf8_lossy(&new_norm);
    let diff = TextDiff::from_lines(&old_str as &str, &new_str as &str);

    let mut shared_bytes = 0usize;
    for change in diff.iter_all_changes() {
        if change.tag() == ChangeTag::Equal {
            // Count bytes in the matching line (including newline).
            shared_bytes += change.value().len();
        }
    }

    // Git: score = copied * MAX_SCORE / max(src_size, dst_size)
    // We normalize to 0-100.
    let max_size = src_size.max(dst_size);

    ((shared_bytes * 100) / max_size).min(100) as u32
}

/// Compute rename/copy similarity percentage (0–100) between two byte slices.
///
/// This uses the same scoring logic as internal rename detection.
#[must_use]
pub fn rename_similarity_score(old: &[u8], new: &[u8]) -> u32 {
    compute_similarity(old, new)
}

// ── Output formatting ───────────────────────────────────────────────

/// Format a diff entry in Git's raw diff format.
///
/// Example: `:100644 100644 abc1234... def5678... M\tfile.txt`
pub fn format_raw(entry: &DiffEntry) -> String {
    let path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            format!(
                "{}\t{}",
                entry.old_path.as_deref().unwrap_or(""),
                entry.new_path.as_deref().unwrap_or("")
            )
        }
        _ => entry.path().to_owned(),
    };

    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{:03}", s),
        (DiffStatus::Copied, Some(s)) => format!("C{:03}", s),
        _ => entry.status.letter().to_string(),
    };

    let (old_hex, new_hex) = raw_oid_hex_pair(&entry.old_oid, &entry.new_oid);
    format!(
        ":{} {} {} {} {}\t{}",
        entry.old_mode, entry.new_mode, old_hex, new_hex, status_str, path
    )
}

/// Render a diff entry's `(old, new)` OIDs as full hex for `--raw` output,
/// widening any null OID to the repository's hash width.
///
/// Diff entries store null OIDs at SHA-1 width regardless of the repository
/// algorithm, but `git diff --raw` prints them at the real hash width (64
/// zeros in a SHA-256 repo). A diff entry never has both sides null, so the
/// width is taken from whichever side carries a real object.
fn raw_oid_hex_pair(old: &ObjectId, new: &ObjectId) -> (String, String) {
    let width = if !old.is_zero() {
        old.algo().hex_len()
    } else if !new.is_zero() {
        new.algo().hex_len()
    } else {
        old.algo().hex_len()
    };
    let render = |oid: &ObjectId| {
        if oid.is_zero() {
            "0".repeat(width)
        } else {
            oid.to_hex()
        }
    };
    (render(old), render(new))
}

/// Format a diff entry with abbreviated OIDs.
pub fn format_raw_abbrev(entry: &DiffEntry, abbrev_len: usize) -> String {
    let ellipsis = if std::env::var("GIT_PRINT_SHA1_ELLIPSIS").ok().as_deref() == Some("yes") {
        "..."
    } else {
        ""
    };
    let old_hex = format!("{}", entry.old_oid);
    let new_hex = format!("{}", entry.new_oid);
    let old_abbrev = &old_hex[..abbrev_len.min(old_hex.len())];
    let new_abbrev = &new_hex[..abbrev_len.min(new_hex.len())];

    // Renames/copies carry a similarity score and a `<old>\t<new>` path pair.
    let path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => format!(
            "{}\t{}",
            entry.old_path.as_deref().unwrap_or(""),
            entry.new_path.as_deref().unwrap_or("")
        ),
        _ => entry.path().to_owned(),
    };
    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{s:03}"),
        (DiffStatus::Copied, Some(s)) => format!("C{s:03}"),
        _ => entry.status.letter().to_string(),
    };

    format!(
        ":{} {} {}{} {}{} {}\t{}",
        entry.old_mode,
        entry.new_mode,
        old_abbrev,
        ellipsis,
        new_abbrev,
        ellipsis,
        status_str,
        path
    )
}

/// Generate a unified diff patch for two blobs.
///
/// # Parameters
///
/// - `old_content` — the old file content (empty for added files).
/// - `new_content` — the new file content (empty for deleted files).
/// - `old_path` — display path for the old side.
/// - `new_path` — display path for the new side.
/// - `context_lines` — number of context lines around changes (default: 3).
/// - Inter-hunk context defaults to `0` (see [`unified_diff_with_prefix`]).
///
/// # Returns
///
/// The unified diff as a string.
pub fn unified_diff(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    unified_diff_with_prefix(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        0,
        "a/",
        "b/",
        indent_heuristic,
        quote_path_fully,
    )
}

/// Same as `unified_diff` but with configurable source/destination prefixes.
///
/// `inter_hunk_context` is Git's `--inter-hunk-context`: adjacent hunks merge when
/// the unchanged gap between them is at most `2 * context_lines + inter_hunk_context` lines.
#[allow(clippy::too_many_arguments)] // Mirrors Git-style unified diff parameters.
pub fn unified_diff_with_prefix(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    unified_diff_with_prefix_and_funcname(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        None,
        indent_heuristic,
        quote_path_fully,
    )
}

/// Same as [`unified_diff_with_prefix`] with optional custom hunk-header
/// function-name matching.
#[allow(clippy::too_many_arguments)]
pub fn unified_diff_with_prefix_and_funcname(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    unified_diff_with_prefix_and_funcname_and_algorithm(
        old_content,
        new_content,
        old_path,
        new_path,
        context_lines,
        inter_hunk_context,
        src_prefix,
        dst_prefix,
        funcname_matcher,
        similar::Algorithm::Myers,
        false,
        false,
        indent_heuristic,
        quote_path_fully,
    )
}

/// Same as [`unified_diff_with_prefix_and_funcname`] but allows callers to
/// choose the line diff algorithm used for hunk generation.
///
/// When `function_context` is true (`git diff -W`), hunks are expanded to
/// whole logical functions using the same rules as Git's `XDL_EMIT_FUNCCONTEXT`.
#[allow(clippy::too_many_arguments)]
pub fn unified_diff_with_prefix_and_funcname_and_algorithm(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    algorithm: similar::Algorithm,
    function_context: bool,
    use_git_histogram: bool,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    // `--function-context` (`-W`) expansion must apply regardless of the line
    // algorithm; the histogram body printer below does not do it, so route `-W`
    // through the function-context emitter first (t4015 #136).
    if function_context {
        return unified_diff_with_function_context(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            inter_hunk_context,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            algorithm,
            indent_heuristic,
            quote_path_fully,
        );
    }

    if use_git_histogram {
        return unified_diff_histogram_with_prefix_and_funcname(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            inter_hunk_context,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            quote_path_fully,
        );
    }

    use crate::quote_path::format_diff_path_with_prefix;
    use similar::{udiff::UnifiedDiffHunk, TextDiff};

    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_lines(old_content, new_content);
    let compacted_ops = diff_indent_heuristic::diff_lines_ops_compacted(
        old_content,
        new_content,
        algorithm,
        indent_heuristic,
    );

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        // Callers (e.g. `diff-tree`, `diff-index`) may pass a fully formatted token
        // (already includes `a/` and any C-style quoting).
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    let old_lines: Vec<&str> = old_content.lines().collect();

    // Git's xdiff merges adjacent changes while the gap between them in the old file is at most
    // `2 * context_lines + inter_hunk_context` (see `xdl_get_hunk` in xemit.c).
    // `similar::group_diff_ops` couples the split threshold and the displayed edge context to a
    // single radius (split at `> 2n`), which over-merges when the gap limit is odd
    // (t4032: `-U0 --inter-hunk-context=1` with 2 common lines must stay 2 hunks).
    let max_common_gap = context_lines
        .saturating_mul(2)
        .saturating_add(inter_hunk_context);
    let op_groups = group_diff_ops_gap(compacted_ops, context_lines, max_common_gap);

    for ops in op_groups {
        if ops.is_empty() {
            continue;
        }
        let hunk = UnifiedDiffHunk::new(ops, &diff, true);
        let hunk_str = format!("{hunk}");
        // The similar crate outputs @@ -a,b +c,d @@\n but Git adds
        // function context after the closing @@. Extract the hunk header
        // and add function context.
        if let Some(first_newline) = hunk_str.find('\n') {
            let header_line = &hunk_str[..first_newline];
            let rest = &hunk_str[first_newline..];

            // Parse the old start line from the @@ header
            if let Some(func_ctx) =
                extract_function_context(header_line, &old_lines, funcname_matcher)
            {
                output.push_str(header_line);
                output.push(' ');
                output.push_str(&func_ctx);
                output.push_str(rest);
            } else {
                output.push_str(&hunk_str);
            }
        } else {
            output.push_str(&hunk_str);
        }
    }

    output
}

/// Group diff ops into hunks like Git's `xdl_get_hunk`: two changes merge into one hunk while
/// the run of unchanged lines between them is at most `max_common_gap`
/// (`2 * context + inter_hunk_context`), and each hunk keeps at most `context` unchanged lines
/// at its edges. Unlike `similar::group_diff_ops`, the split threshold is decoupled from the
/// edge context so odd gap limits group exactly like Git.
fn group_diff_ops_gap(
    mut ops: Vec<similar::DiffOp>,
    context: usize,
    max_common_gap: usize,
) -> Vec<Vec<similar::DiffOp>> {
    use similar::DiffOp;
    if ops.is_empty() {
        return vec![];
    }

    let mut pending_group = Vec::new();
    let mut rv = Vec::new();

    if let Some(DiffOp::Equal {
        old_index,
        new_index,
        len,
    }) = ops.first_mut()
    {
        let offset = (*len).saturating_sub(context);
        *old_index += offset;
        *new_index += offset;
        *len -= offset;
    }

    if let Some(DiffOp::Equal { len, .. }) = ops.last_mut() {
        *len -= (*len).saturating_sub(context);
    }

    for op in ops.into_iter() {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            // End the current group and start a new one whenever the unchanged
            // run is too long to fuse the surrounding changes.
            if len > max_common_gap {
                pending_group.push(DiffOp::Equal {
                    old_index,
                    new_index,
                    len: context,
                });
                rv.push(pending_group);
                let offset = len.saturating_sub(context);
                pending_group = vec![DiffOp::Equal {
                    old_index: old_index + offset,
                    new_index: new_index + offset,
                    len: len - offset,
                }];
                continue;
            }
        }
        pending_group.push(op);
    }

    match &pending_group[..] {
        &[] | &[similar::DiffOp::Equal { .. }] => {}
        _ => rv.push(pending_group),
    }

    rv
}

/// `git diff -W`: expand each hunk to include full function bodies (see Git `xemit.c`).
fn unified_diff_with_function_context(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    inter_hunk_context: usize,
    src_prefix: &str,
    dst_prefix: &str,
    funcname_matcher: Option<&FuncnameMatcher>,
    algorithm: similar::Algorithm,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;
    use similar::{udiff::UnifiedDiffHunk, TextDiff};

    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_lines(old_content, new_content);

    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let n_old = old_lines.len();
    let n_new = new_lines.len();

    // Group changes the way Git's xdl_get_hunk does: merge while the unchanged
    // gap is at most `2*context + inter_hunk_context`. `similar::group_diff_ops`
    // splits only at gap > `2*radius`, which over-merges changes in *different*
    // functions (e.g. a leading insertion and a body change) into one hunk and
    // then over-expands the function context (t4015 #136). Use the gap-correct
    // grouping (same as the non-function-context path).
    let max_common_gap = context_lines
        .saturating_mul(2)
        .saturating_add(inter_hunk_context);
    let all_ops = diff.ops().to_vec();
    let op_groups = group_diff_ops_gap(all_ops.clone(), context_lines, max_common_gap);

    let mut ranges: Vec<(usize, usize, usize, usize)> = Vec::new();

    for ops in op_groups {
        if ops.is_empty() {
            continue;
        }
        let i1_anchor = func_context_old_anchor(&ops, n_old);
        let i1_end = hunk_old_change_end_exclusive(&ops);
        let skip_preimage_pull =
            append_with_whole_function_added(&ops, n_old, n_new, &new_lines, funcname_matcher);
        let hunk = UnifiedDiffHunk::new(ops, &diff, true);
        let hunk_str = format!("{hunk}");
        let header_line = hunk_str
            .lines()
            .next()
            .unwrap_or("")
            .trim_end_matches(['\r', '\n']);
        let Some((base_s1, _base_e1, _base_s2, _base_e2)) =
            parse_unified_hunk_header_ranges(header_line)
        else {
            continue;
        };

        let ctx = context_lines;
        let (s1, e1, s2, e2) = if skip_preimage_pull {
            let s = n_old.saturating_sub(ctx);
            let s2 = map_old_line_to_new(&all_ops, s, n_new).min(n_new);
            (s, n_old, s2, n_new)
        } else {
            let mut s1 = base_s1.saturating_sub(ctx);
            let mut s2 = map_old_line_to_new(&all_ops, s1, n_new).min(n_new);

            let base_pre_s1 = i1_anchor.saturating_sub(ctx);
            if base_pre_s1 < s1 {
                s1 = base_pre_s1;
                s2 = map_old_line_to_new(&all_ops, s1, n_new).min(n_new);
            }

            let fs1 = expand_func_pre_start(s1, i1_anchor, n_old, &old_lines, funcname_matcher);
            if fs1 < s1 {
                s1 = fs1;
                s2 = map_old_line_to_new(&all_ops, s1, n_new).min(n_new);
            }

            // `i1_end` is the exclusive end of the changed region; its post-image
            // context is `ctx` lines (Git's `xche->i1 + xche->chg1 + lctx`). The
            // hunk's `base_e1` already includes the group's trailing context, so
            // use the change end + ctx directly to avoid double-counting context
            // (which over-extended the hunk and merged separate functions — t4015 #136).
            let mut e1 = (i1_end + ctx).min(n_old);
            let mut e2 = map_old_line_to_new(&all_ops, e1, n_new).min(n_new);
            let fe1 = expand_func_post_end(e1, i1_end, n_old, &old_lines, funcname_matcher);
            if fe1 > e1 {
                e1 = fe1;
                e2 = map_old_line_to_new(&all_ops, e1, n_new).min(n_new);
            }
            (s1, e1, s2, e2)
        };

        ranges.push((s1, e1, s2, e2));
    }

    // Merge ranges whose function-context expansion made them overlap on the old
    // side (Git emits a single hunk in that case).
    ranges.sort_by_key(|r| (r.0, r.2));
    let mut merged: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(ranges.len());
    for (s1, e1, s2, e2) in ranges {
        if let Some(last) = merged.last_mut() {
            if s1 < last.1 {
                last.1 = last.1.max(e1);
                last.3 = last.3.max(e2);
                continue;
            }
        }
        merged.push((s1, e1, s2, e2));
    }
    let ranges = merged;

    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else if src_prefix.is_empty() {
        output.push_str(&format!("--- {old_path}\n"));
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            src_prefix,
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else if dst_prefix.is_empty() {
        output.push_str(&format!("+++ {new_path}\n"));
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            dst_prefix,
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    for (s1, e1, s2, e2) in ranges {
        if s1 >= e1 && s2 >= e2 {
            continue;
        }
        let old_seg =
            line_slice_for_diff_with_eof_nl(&old_lines, s1, e1, old_content.ends_with('\n'));
        let new_seg =
            line_slice_for_diff_with_eof_nl(&new_lines, s2, e2, new_content.ends_with('\n'));
        let inner_ctx = old_seg.lines().count().max(new_seg.lines().count()).max(1);
        let piece = unified_diff_with_prefix_and_funcname_and_algorithm(
            &old_seg,
            &new_seg,
            old_path,
            new_path,
            inner_ctx,
            0,
            src_prefix,
            dst_prefix,
            funcname_matcher,
            algorithm,
            false,
            false,
            indent_heuristic,
            quote_path_fully,
        );
        let shifted = shift_unified_hunk_headers_to_full_file(&piece, s1, s2);
        let with_func =
            enrich_unified_hunk_headers_funcname(&shifted, &old_lines, funcname_matcher);
        for line in with_func.lines() {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                continue;
            }
            output.push_str(line);
            output.push('\n');
        }
    }

    output
}

/// `piece` is a unified diff for a slice of the file; hunk headers use 1-based
/// coordinates relative to that slice. Shift them by `delta_old` / `delta_new`
/// (0-based offsets of the slice in the full file) so the combined patch applies
/// to the whole file.
fn shift_unified_hunk_headers_to_full_file(
    patch: &str,
    delta_old: usize,
    delta_new: usize,
) -> String {
    if delta_old == 0 && delta_new == 0 {
        return patch.to_owned();
    }
    let mut out = String::with_capacity(patch.len());
    for line in patch.lines() {
        if let Some(shifted) = shift_one_unified_hunk_header(line, delta_old, delta_new) {
            out.push_str(&shifted);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn shift_one_unified_hunk_header(line: &str, delta_old: usize, delta_new: usize) -> Option<String> {
    let rest = line.strip_prefix("@@ ")?;
    let (old_chunk, after_plus) = rest.split_once(" +")?;
    let old_spec = old_chunk.strip_prefix('-')?;
    let (new_spec, suffix) = after_plus.split_once(" @@")?;
    let shifted_old = shift_unified_range_spec(old_spec, delta_old)?;
    let shifted_new = shift_unified_range_spec(new_spec, delta_new)?;
    Some(format!("@@ -{shifted_old} +{shifted_new} @@{suffix}"))
}

fn shift_unified_range_spec(spec: &str, delta: usize) -> Option<String> {
    let spec = spec.trim();
    if let Some((start_s, count_s)) = spec.split_once(',') {
        let start: usize = start_s.parse().ok()?;
        let count: usize = count_s.parse().ok()?;
        Some(format!("{},{}", start.saturating_add(delta), count))
    } else {
        let start: usize = spec.parse().ok()?;
        Some(format!("{}", start.saturating_add(delta)))
    }
}

/// Re-attach `@@ ... @@ <funcname>` using full-file line indices (inner diffs use slices).
fn enrich_unified_hunk_headers_funcname(
    patch: &str,
    full_old_lines: &[&str],
    funcname_matcher: Option<&FuncnameMatcher>,
) -> String {
    let mut out = String::with_capacity(patch.len());
    for line in patch.lines() {
        if let Some(fixed) = enrich_one_hunk_header_funcname(line, full_old_lines, funcname_matcher)
        {
            out.push_str(&fixed);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn enrich_one_hunk_header_funcname(
    line: &str,
    full_old_lines: &[&str],
    funcname_matcher: Option<&FuncnameMatcher>,
) -> Option<String> {
    let after_at = line.strip_prefix("@@ ")?;
    let idx = after_at.find(" @@")?;
    let mid = after_at[..idx].trim();
    let tail = after_at[idx + 3..].trim_start();
    let header_for_parse = format!("@@ {mid} @@");
    let func = extract_function_context(&header_for_parse, full_old_lines, funcname_matcher);
    Some(if let Some(f) = func {
        format!("@@ {mid} @@ {f}")
    } else if !tail.is_empty() {
        format!("@@ {mid} @@ {tail}")
    } else {
        format!("@@ {mid} @@")
    })
}

fn line_slice_for_diff_with_eof_nl(
    lines: &[&str],
    start: usize,
    end: usize,
    full_file_ends_with_newline: bool,
) -> String {
    if start >= end {
        return String::new();
    }
    let mut s = lines[start..end].join("\n");
    let slice_is_suffix_of_file = end == lines.len();
    let need_trailing_nl = if slice_is_suffix_of_file {
        full_file_ends_with_newline
    } else {
        true
    };
    if need_trailing_nl && !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Map a 0-based old line index to the corresponding 0-based new line index using the full-file
/// diff ops (Git aligns context across deletions/insertions).
fn map_old_line_to_new(ops: &[similar::DiffOp], old_line: usize, n_new: usize) -> usize {
    use similar::DiffOp;
    let mut n = 0usize;
    for op in ops {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                if old_index + len <= old_line {
                    n = new_index + len;
                    continue;
                }
                if old_index < old_line {
                    let take = old_line - old_index;
                    return (new_index + take).min(n_new);
                }
                return new_index.min(n_new);
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                if old_index + old_len <= old_line {
                    n = new_index;
                    continue;
                }
                if old_index < old_line {
                    return new_index.min(n_new);
                }
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                if old_index < old_line {
                    n = new_index + new_len;
                    continue;
                }
                if old_index == old_line {
                    // `old_line` is an exclusive end or insertion point aligned with this insert
                    // (e.g. EOF append maps to after the inserted block).
                    return (new_index + new_len).min(n_new);
                }
                return new_index.min(n_new);
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                if old_index + old_len <= old_line {
                    n = new_index + new_len;
                    continue;
                }
                if old_index < old_line {
                    let into_old = old_line - old_index;
                    let mapped = new_index + into_old.min(new_len);
                    return mapped.min(n_new);
                }
                return new_index.min(n_new);
            }
        }
    }
    n.min(n_new)
}

/// Parse `@@ -old +new @@` into 0-based half-open ranges in each file.
fn parse_unified_hunk_header_ranges(header: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = header.strip_prefix("@@ ")?;
    let (old_tok, rest2) = rest.split_once(" +")?;
    let old_tok = old_tok.strip_prefix('-')?;
    let new_tok = rest2.split_once(" @@").map(|(a, _)| a)?;

    fn parse_side(spec: &str) -> Option<(usize, usize)> {
        let spec = spec.trim();
        let (start_one_based, count) = if let Some((a, b)) = spec.split_once(',') {
            (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)
        } else {
            let s = spec.parse::<usize>().ok()?;
            (s, 1usize)
        };
        let s0 = start_one_based.saturating_sub(1);
        let e0 = s0.saturating_add(count);
        Some((s0, e0))
    }

    let (os, oe) = parse_side(old_tok)?;
    let (ns, ne) = parse_side(new_tok)?;
    Some((os, oe, ns, ne))
}

/// Git `xemit.c`: when a hunk only inserts at EOF (first inserted line is `new_index == n_old`)
/// and the added text already contains a funcname line, do not pull extra context from the preimage.
fn append_with_whole_function_added(
    ops: &[similar::DiffOp],
    n_old: usize,
    n_new: usize,
    new_lines: &[&str],
    matcher: Option<&FuncnameMatcher>,
) -> bool {
    use similar::DiffOp;
    if n_old == 0 {
        return false;
    }
    let mut only_ins_or_eq = true;
    let mut min_new_ins = usize::MAX;
    for op in ops {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                min_new_ins = min_new_ins.min(new_index);
                if new_len == 0 {
                    only_ins_or_eq = false;
                }
            }
            DiffOp::Delete { .. } | DiffOp::Replace { .. } => {
                only_ins_or_eq = false;
            }
        }
    }
    let mut insert_at_eof = false;
    for op in ops {
        if let DiffOp::Insert { old_index, .. } = *op {
            if old_index == n_old {
                insert_at_eof = true;
                break;
            }
        }
    }
    let append_at_eof = min_new_ins == n_old || insert_at_eof;
    if !only_ins_or_eq || !append_at_eof || min_new_ins == usize::MAX {
        return false;
    }
    // Git only skips preimage pull when the inserted block is clearly a new logical
    // function (see `xemit.c` walking `xdf2` for `is_func_rec`). A loose "any line
    // looks like a function" check would match `return` / `printf` and break `-W`
    // hunks that still need preimage context (t4051 `extended`).
    let mut j = min_new_ins;
    while j < n_new {
        let line = new_lines[j];
        if line.trim().is_empty() {
            j += 1;
            continue;
        }
        if let Some(m) = matcher {
            if m.match_line(line).is_some() {
                return true;
            }
        } else if inserted_block_starts_with_c_like_function_definition(line) {
            return true;
        }
        j += 1;
    }
    false
}

fn inserted_block_starts_with_c_like_function_definition(line: &str) -> bool {
    let t = line.trim_start();
    let Some(open_paren) = t.find('(') else {
        return false;
    };
    let head = &t[..open_paren];
    let tokens: Vec<&str> = head.split_whitespace().collect();
    if tokens.len() < 2 {
        // `printf(...)`, `return (`, etc. — not `return_type name(`.
        return false;
    }
    let nameish = tokens.last().copied().unwrap_or("");
    let name = nameish.trim_end_matches(['*', '&']);
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let type_or_modifier = |tok: &str| {
        matches!(
            tok,
            "static"
                | "extern"
                | "inline"
                | "void"
                | "int"
                | "char"
                | "short"
                | "long"
                | "float"
                | "double"
                | "unsigned"
                | "signed"
                | "struct"
                | "enum"
                | "union"
                | "const"
                | "volatile"
                | "typedef"
        )
    };
    tokens[..tokens.len() - 1]
        .iter()
        .any(|tok| type_or_modifier(tok))
}

fn hunk_old_change_end_exclusive(ops: &[similar::DiffOp]) -> usize {
    use similar::DiffOp;
    let mut max_o = 0usize;
    for op in ops {
        match *op {
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                max_o = max_o.max(old_index + old_len);
            }
            DiffOp::Replace {
                old_index, old_len, ..
            } => {
                max_o = max_o.max(old_index + old_len);
            }
            DiffOp::Insert { old_index, .. } => {
                // Pure insertions do not consume old lines; Git's post-context anchor is the
                // insertion point (`old_index`), not 0 (t4051 `extended`).
                max_o = max_o.max(old_index);
            }
            DiffOp::Equal { .. } => {}
        }
    }
    max_o
}

fn func_context_old_anchor(ops: &[similar::DiffOp], n_old: usize) -> usize {
    use similar::DiffOp;
    let mut has_delete_or_replace = false;
    let mut min_del = usize::MAX;
    let mut min_ins_old = usize::MAX;

    for op in ops {
        match *op {
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                has_delete_or_replace = true;
                min_del = min_del.min(old_index);
                min_del = min_del.min(old_index + old_len.saturating_sub(1));
            }
            DiffOp::Replace {
                old_index, old_len, ..
            } => {
                has_delete_or_replace = true;
                min_del = min_del.min(old_index);
                min_del = min_del.min(old_index + old_len.saturating_sub(1));
            }
            DiffOp::Insert { old_index, .. } => {
                min_ins_old = min_ins_old.min(old_index);
            }
            DiffOp::Equal { .. } => {}
        }
    }

    let mut i1 = if has_delete_or_replace {
        min_del
    } else if min_ins_old != usize::MAX {
        min_ins_old
    } else {
        0
    };

    let pure_insert = ops
        .iter()
        .all(|op| matches!(op, DiffOp::Insert { .. } | DiffOp::Equal { .. }))
        && ops.iter().any(|op| matches!(op, DiffOp::Insert { .. }));

    if pure_insert && i1 >= n_old && n_old > 0 {
        i1 = n_old - 1;
    }

    i1.min(n_old.saturating_sub(1))
}

fn expand_func_pre_start(
    s1: usize,
    i1: usize,
    n_old: usize,
    old_lines: &[&str],
    matcher: Option<&FuncnameMatcher>,
) -> usize {
    if n_old == 0 {
        return s1;
    }
    let i1 = i1.min(n_old.saturating_sub(1));
    let mut fs1 = get_func_line_backward(old_lines, i1, matcher).unwrap_or(i1);
    while fs1 > 0
        && !is_line_empty_for_func_context(old_lines[fs1 - 1])
        && !is_func_line(old_lines[fs1 - 1], matcher)
    {
        fs1 -= 1;
    }
    s1.min(fs1)
}

fn expand_func_post_end(
    e1: usize,
    i1_end: usize,
    n_old: usize,
    old_lines: &[&str],
    matcher: Option<&FuncnameMatcher>,
) -> usize {
    let from = i1_end.min(n_old);
    let fe1 = get_func_line_forward(old_lines, from, matcher).unwrap_or(n_old);
    let mut fe1_adj = fe1;
    while fe1_adj > 0 && is_line_empty_for_func_context(old_lines[fe1_adj - 1]) {
        fe1_adj -= 1;
    }
    e1.max(fe1_adj).min(n_old)
}

fn is_line_empty_for_func_context(line: &str) -> bool {
    line.chars().all(|c| c.is_whitespace())
}

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

fn get_func_line_backward(
    old_lines: &[&str],
    start: usize,
    matcher: Option<&FuncnameMatcher>,
) -> Option<usize> {
    let mut l = start.min(old_lines.len().saturating_sub(1));
    if old_lines.is_empty() {
        return None;
    }
    loop {
        if is_func_line(old_lines[l], matcher) {
            return Some(l);
        }
        if l == 0 {
            break;
        }
        l -= 1;
    }
    None
}

fn get_func_line_forward(
    old_lines: &[&str],
    start: usize,
    matcher: Option<&FuncnameMatcher>,
) -> Option<usize> {
    let mut l = start;
    while l < old_lines.len() {
        if is_func_line(old_lines[l], matcher) {
            return Some(l);
        }
        l += 1;
    }
    None
}

/// Compute a unified diff with anchored lines.
///
/// Anchored lines that appear exactly once in both old and new content are
/// forced to match, splitting the diff into segments around those anchor points.
/// This produces diffs where the anchored text stays as context and surrounding
/// lines are shown as additions/removals.
///
/// Segment diffs use `algorithm`. When `use_git_histogram` is true, histogram uses imara-diff
/// (Git-compatible); otherwise `algorithm` is passed to `similar`.
pub fn anchored_unified_diff(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    context_lines: usize,
    anchors: &[String],
    algorithm: similar::Algorithm,
    use_git_histogram: bool,
    indent_heuristic: bool,
    quote_path_fully: bool,
) -> String {
    use crate::quote_path::format_diff_path_with_prefix;
    use similar::TextDiff;

    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    // Find anchored lines that appear exactly once in both old and new
    let mut anchor_pairs: Vec<(usize, usize)> = Vec::new(); // (old_idx, new_idx)

    for anchor in anchors {
        let anchor_str = anchor.as_str();

        // Count occurrences in old
        let old_positions: Vec<usize> = old_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim_end() == anchor_str)
            .map(|(i, _)| i)
            .collect();

        // Count occurrences in new
        let new_positions: Vec<usize> = new_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim_end() == anchor_str)
            .map(|(i, _)| i)
            .collect();

        // Only anchor if unique in both
        if old_positions.len() == 1 && new_positions.len() == 1 {
            anchor_pairs.push((old_positions[0], new_positions[0]));
        }
    }

    // If no valid anchors, fall back to normal diff
    if anchor_pairs.is_empty() {
        return unified_diff_with_prefix_and_funcname_and_algorithm(
            old_content,
            new_content,
            old_path,
            new_path,
            context_lines,
            0,
            "a/",
            "b/",
            None,
            algorithm,
            false,
            use_git_histogram,
            indent_heuristic,
            quote_path_fully,
        );
    }

    // Sort anchor pairs by their position in the old file
    anchor_pairs.sort_by_key(|&(old_idx, _)| old_idx);

    // Filter to only keep pairs where new positions are also increasing
    // (longest increasing subsequence of new positions)
    let mut filtered: Vec<(usize, usize)> = Vec::new();
    for &pair in &anchor_pairs {
        if filtered.is_empty() || filtered.last().is_some_and(|last| pair.1 > last.1) {
            filtered.push(pair);
        }
    }
    let anchor_pairs = filtered;

    // Build a modified version of old/new where we diff segments between anchors.
    // We'll construct the diff by processing segments:
    // - Before first anchor
    // - Between consecutive anchors
    // - After last anchor
    // Each anchor line itself is a fixed context match.

    // Collect all diff operations
    struct LineDiffOp {
        tag: char, // ' ', '+', '-'
        line: String,
    }

    let append_segment_diff =
        |ops: &mut Vec<LineDiffOp>, old_seg_input: &str, new_seg_input: &str| {
            use similar::ChangeTag;
            let old_ls: Vec<&str> = old_seg_input.lines().collect();
            let new_ls: Vec<&str> = new_seg_input.lines().collect();
            if old_ls.is_empty() && new_ls.is_empty() {
                return;
            }
            let seg_diff = TextDiff::configure()
                .algorithm(algorithm)
                .diff_slices(&old_ls, &new_ls);
            let raw = seg_diff.ops().to_vec();
            let compacted = diff_indent_heuristic::apply_change_compact_to_ops(
                &raw,
                &old_ls,
                &new_ls,
                indent_heuristic,
            );
            for op in &compacted {
                for ch in op.iter_changes(&old_ls, &new_ls) {
                    let t = match ch.tag() {
                        ChangeTag::Equal => ' ',
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                    };
                    ops.push(LineDiffOp {
                        tag: t,
                        line: ch.value().to_string(),
                    });
                }
            }
        };

    let mut ops: Vec<LineDiffOp> = Vec::new();
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;

    for &(old_anchor, new_anchor) in &anchor_pairs {
        // Diff the segment before this anchor
        let old_segment: Vec<&str> = old_lines[old_pos..old_anchor].to_vec();
        let new_segment: Vec<&str> = new_lines[new_pos..new_anchor].to_vec();

        let old_seg_text = old_segment.join("\n");
        let new_seg_text = new_segment.join("\n");

        if !old_seg_text.is_empty() || !new_seg_text.is_empty() {
            let old_seg_input = if old_seg_text.is_empty() {
                String::new()
            } else {
                format!("{}\n", old_seg_text)
            };
            let new_seg_input = if new_seg_text.is_empty() {
                String::new()
            } else {
                format!("{}\n", new_seg_text)
            };
            append_segment_diff(&mut ops, &old_seg_input, &new_seg_input);
        }

        // The anchor line itself is always context
        ops.push(LineDiffOp {
            tag: ' ',
            line: old_lines[old_anchor].to_string(),
        });

        old_pos = old_anchor + 1;
        new_pos = new_anchor + 1;
    }

    // Diff the remaining segment after the last anchor
    let old_segment: Vec<&str> = old_lines[old_pos..].to_vec();
    let new_segment: Vec<&str> = new_lines[new_pos..].to_vec();
    let old_seg_text = old_segment.join("\n");
    let new_seg_text = new_segment.join("\n");

    if !old_seg_text.is_empty() || !new_seg_text.is_empty() {
        let old_seg_input = if old_seg_text.is_empty() {
            String::new()
        } else {
            format!("{}\n", old_seg_text)
        };
        let new_seg_input = if new_seg_text.is_empty() {
            String::new()
        } else {
            format!("{}\n", new_seg_text)
        };
        append_segment_diff(&mut ops, &old_seg_input, &new_seg_input);
    }

    // Now format as unified diff with hunks
    let mut output = String::new();
    if old_path == "/dev/null" {
        output.push_str("--- /dev/null\n");
    } else {
        output.push_str("--- ");
        output.push_str(&format_diff_path_with_prefix(
            "a/",
            old_path,
            quote_path_fully,
        ));
        output.push('\n');
    }
    if new_path == "/dev/null" {
        output.push_str("+++ /dev/null\n");
    } else {
        output.push_str("+++ ");
        output.push_str(&format_diff_path_with_prefix(
            "b/",
            new_path,
            quote_path_fully,
        ));
        output.push('\n');
    }

    // Group ops into hunks with context
    let total_ops = ops.len();
    if total_ops == 0 {
        return output;
    }

    // Find ranges of changes
    let mut hunks: Vec<(usize, usize)> = Vec::new(); // (start, end) indices into ops
    let mut i = 0;
    while i < total_ops {
        if ops[i].tag != ' ' {
            let start = i.saturating_sub(context_lines);
            let mut end = i;
            // Extend to include consecutive changes and their context
            while end < total_ops {
                if ops[end].tag != ' ' {
                    end += 1;
                    continue;
                }
                // Check if there's another change within context_lines
                let mut next_change = end;
                while next_change < total_ops && ops[next_change].tag == ' ' {
                    next_change += 1;
                }
                if next_change < total_ops && next_change - end <= context_lines * 2 {
                    end = next_change + 1;
                } else {
                    end = (end + context_lines).min(total_ops);
                    break;
                }
            }
            // Merge with previous hunk if overlapping
            if let Some(last) = hunks.last_mut() {
                if start <= last.1 {
                    last.1 = end;
                } else {
                    hunks.push((start, end));
                }
            } else {
                hunks.push((start, end));
            }
            i = end;
        } else {
            i += 1;
        }
    }

    // Output each hunk
    for (start, end) in hunks {
        // Count old/new lines in this hunk
        let mut old_start = 1usize;
        let mut new_start = 1usize;
        // Calculate line numbers by counting ops before this hunk
        for op in &ops[..start] {
            match op.tag {
                ' ' => {
                    old_start += 1;
                    new_start += 1;
                }
                '-' => {
                    old_start += 1;
                }
                '+' => {
                    new_start += 1;
                }
                _ => {}
            }
        }
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for op in &ops[start..end] {
            match op.tag {
                ' ' => {
                    old_count += 1;
                    new_count += 1;
                }
                '-' => {
                    old_count += 1;
                }
                '+' => {
                    new_count += 1;
                }
                _ => {}
            }
        }

        output.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start, old_count, new_start, new_count
        ));
        for op in &ops[start..end] {
            output.push(op.tag);
            output.push_str(&op.line);
            output.push('\n');
        }
    }

    output
}

/// Extract function context for a hunk header.
///
/// Given a hunk header like `@@ -8,7 +8,7 @@`, find the last line
/// before line 8 in the old content that looks like a function header
/// (starts with a non-whitespace character, like Git's default).
fn extract_function_context(
    header: &str,
    old_lines: &[&str],
    funcname_matcher: Option<&FuncnameMatcher>,
) -> Option<String> {
    // Parse the old start line number from "@@ -<start>,<count> ..."
    let at_pos = header.find("-")?;
    let rest = &header[at_pos + 1..];
    let comma_or_space = rest.find([',', ' '])?;
    let start_str = &rest[..comma_or_space];
    let start_line: usize = start_str.parse().ok()?;

    // Parse the old line count; "@@ -<start>,<count> ..." (no comma means count 1).
    // Only look for the comma inside the old-range token itself — searching the
    // whole remainder would pick up the comma from the new side (e.g. "+0,0").
    let old_token_end = rest.find([' ', '\t']).unwrap_or(rest.len());
    let old_token = &rest[..old_token_end];
    let old_count: usize = if let Some(comma) = old_token.find(',') {
        old_token[comma + 1..].parse().unwrap_or(1)
    } else {
        1
    };

    if start_line == 0 {
        return None;
    }

    // Look backwards for a line that matches the funcname pattern. start_line is
    // 1-indexed. For a normal hunk the first changed pre-image line is
    // old_lines[start_line-1], so we search lines strictly before it
    // (old_lines[0..start_line-1]). For a pure insertion (old count 0) the
    // content is inserted *after* old line start_line, so Git's function search
    // begins at that line itself: search old_lines[0..start_line].
    let search_end = if old_count == 0 {
        start_line.min(old_lines.len())
    } else {
        if start_line <= 1 {
            return None;
        }
        (start_line - 1).min(old_lines.len())
    };
    let truncate = |text: &str| {
        if text.len() > 80 {
            let mut end = 80;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text[..end].to_owned()
        } else {
            text.to_owned()
        }
    };

    for i in (0..search_end).rev() {
        let line = old_lines[i];
        if line.is_empty() {
            continue;
        }
        if let Some(matcher) = funcname_matcher {
            if let Some(matched) = matcher.match_line(line) {
                return Some(truncate(&matched));
            }
            continue;
        }

        let first = line.as_bytes()[0];
        if first.is_ascii_alphabetic() || first == b'_' || first == b'$' {
            return Some(truncate(line.trim_end_matches(char::is_whitespace)));
        }
    }
    None
}

/// Generate diff stat output (file name + insertions/deletions).
///
/// Returns a single line like: ` file.txt | 5 ++---`
pub fn format_stat_line(
    path: &str,
    insertions: usize,
    deletions: usize,
    max_path_len: usize,
) -> String {
    format_stat_line_width(path, insertions, deletions, max_path_len, 0)
}

pub fn format_stat_line_width(
    path: &str,
    insertions: usize,
    deletions: usize,
    max_path_len: usize,
    count_width: usize,
) -> String {
    let total = insertions + deletions;
    let plus = "+".repeat(insertions.min(50));
    let minus = "-".repeat(deletions.min(50));
    let cw = if count_width > 0 {
        count_width
    } else {
        format!("{}", total).len()
    };
    let bar = format!("{}{}", plus, minus);
    if bar.is_empty() {
        format!(
            " {:<width$} | {:>cw$}",
            path,
            total,
            width = max_path_len,
            cw = cw
        )
    } else {
        format!(
            " {:<width$} | {:>cw$} {}",
            path,
            total,
            bar,
            width = max_path_len,
            cw = cw
        )
    }
}

/// Normalise one line like Git's `-b` / `--ignore-space-change`.
#[must_use]
pub fn normalize_ignore_space_change_line(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut in_space = false;
    for c in line.chars() {
        if c.is_whitespace() {
            if !in_space {
                result.push(' ');
                in_space = true;
            }
        } else {
            result.push(c);
            in_space = false;
        }
    }
    while result.ends_with(' ') {
        result.pop();
    }
    result
}

/// Normalise text like Git's `-b` / `--ignore-space-change`: on each line, collapse runs of
/// whitespace to a single ASCII space and trim trailing spaces.
///
/// Line breaks are preserved by splitting on [`str::lines`] and rejoining with `\n` (same approach
/// as the porcelain `diff` whitespace handling in `grit`).
#[must_use]
pub fn normalize_ignore_space_change(content: &str) -> String {
    content
        .lines()
        .map(normalize_ignore_space_change_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Count insertions and deletions between two strings.
///
/// Returns `(insertions, deletions)`.
pub fn count_changes(old_content: &str, new_content: &str) -> (usize, usize) {
    count_changes_with_algorithm(old_content, new_content, similar::Algorithm::Myers, false)
}

/// Count insertions and deletions using the given line-diff algorithm.
///
/// Git's `--stat` / `--numstat` follow the configured diff algorithm; this mirrors that by
/// running [`similar::TextDiff`] with an explicit [`similar::Algorithm`].
#[must_use]
pub fn count_changes_with_algorithm(
    old_content: &str,
    new_content: &str,
    algorithm: similar::Algorithm,
    use_git_histogram: bool,
) -> (usize, usize) {
    if use_git_histogram {
        use imara_diff::{Algorithm, Diff, InternedInput};
        let input = InternedInput::new(old_content, new_content);
        let mut d = Diff::compute(Algorithm::Histogram, &input);
        d.postprocess_lines(&input);
        return (d.count_additions() as usize, d.count_removals() as usize);
    }

    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::configure()
        .algorithm(algorithm)
        .diff_lines(old_content, new_content);
    let mut ins = 0;
    let mut del = 0;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => ins += 1,
            ChangeTag::Delete => del += 1,
            ChangeTag::Equal => {}
        }
    }

    (ins, del)
}

/// Line count for diffstat/`--numstat`, matching Git's `count_lines()` in `diff.c`.
///
/// Counts newline-terminated lines; a final line without trailing newline still counts as one line.
/// An empty buffer yields `0`.
#[must_use]
pub fn count_git_lines(data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut nl_just_seen = false;
    for &ch in data {
        if ch == b'\n' {
            count += 1;
            nl_just_seen = true;
        } else {
            nl_just_seen = false;
        }
    }
    if !nl_just_seen {
        count += 1;
    }
    count
}

/// Internal maximum diff score used by Git rename/break heuristics (`MAX_SCORE` in `diffcore.h`).
pub const GIT_DIFF_MAX_SCORE: u64 = 60_000;
const DIFF_MAX_SCORE: u64 = GIT_DIFF_MAX_SCORE;
const DIFF_MINIMUM_BREAK_SIZE: usize = 400;
const DIFF_DEFAULT_BREAK_SCORE: u64 = 30_000;
/// Default break threshold (`DEFAULT_BREAK_SCORE` in `diffcore.h`), internal 0–[`GIT_DIFF_MAX_SCORE`] scale.
pub const GIT_DIFF_DEFAULT_BREAK_SCORE: u64 = DIFF_DEFAULT_BREAK_SCORE;
/// Default merge threshold after a break (`DEFAULT_MERGE_SCORE` in `diffcore.h`): pairs broken for
/// rename/copy but not consumed are merged back when deletion-weight is below this (60% by default).
pub const GIT_DIFF_DEFAULT_MERGE_SCORE_AFTER_BREAK: u64 = 36_000;
const DIFF_HASHBASE: u32 = 107_927;

#[derive(Clone, Copy, Default)]
struct SpanSlot {
    hashval: u32,
    cnt: u32,
}

struct SpanHashTop {
    alloc_log2: u8,
    free_slots: i32,
    data: Vec<SpanSlot>,
}

impl SpanHashTop {
    fn new(initial_log2: u8) -> Self {
        let cap = 1usize << initial_log2;
        Self {
            alloc_log2: initial_log2,
            free_slots: initial_free(initial_log2),
            data: vec![SpanSlot::default(); cap],
        }
    }

    fn len(&self) -> usize {
        1usize << self.alloc_log2
    }

    fn add_span(&mut self, hashval: u32, cnt: u32) {
        loop {
            let lim = self.len();
            let mut bucket = (hashval as usize) & (lim - 1);
            loop {
                let h = &mut self.data[bucket];
                if h.cnt == 0 {
                    h.hashval = hashval;
                    h.cnt = cnt;
                    self.free_slots -= 1;
                    if self.free_slots < 0 {
                        self.rehash();
                        break;
                    }
                    return;
                }
                if h.hashval == hashval {
                    h.cnt = h.cnt.saturating_add(cnt);
                    return;
                }
                bucket += 1;
                if bucket >= lim {
                    bucket = 0;
                }
            }
        }
    }

    fn rehash(&mut self) {
        let old = std::mem::take(&mut self.data);
        let old_log = self.alloc_log2;
        self.alloc_log2 = old_log.saturating_add(1);
        let new_len = 1usize << self.alloc_log2;
        self.free_slots = initial_free(self.alloc_log2);
        self.data = vec![SpanSlot::default(); new_len];
        let old_sz = 1usize << old_log;
        for o in old.iter().take(old_sz) {
            let o = *o;
            if o.cnt == 0 {
                continue;
            }
            self.add_span_after_rehash(o.hashval, o.cnt);
        }
    }

    fn add_span_after_rehash(&mut self, hashval: u32, cnt: u32) {
        loop {
            let lim = self.len();
            let mut bucket = (hashval as usize) & (lim - 1);
            loop {
                let h = &mut self.data[bucket];
                if h.cnt == 0 {
                    h.hashval = hashval;
                    h.cnt = cnt;
                    self.free_slots -= 1;
                    if self.free_slots < 0 {
                        self.rehash();
                        break;
                    }
                    return;
                }
                if h.hashval == hashval {
                    h.cnt = h.cnt.saturating_add(cnt);
                    return;
                }
                bucket += 1;
                if bucket >= lim {
                    bucket = 0;
                }
            }
        }
    }

    fn sort_by_hashval(&mut self) {
        let sz = self.len();
        self.data[..sz].sort_by(|a, b| {
            if a.cnt == 0 {
                return std::cmp::Ordering::Greater;
            }
            if b.cnt == 0 {
                return std::cmp::Ordering::Less;
            }
            a.hashval.cmp(&b.hashval)
        });
    }
}

fn initial_free(sz_log2: u8) -> i32 {
    let sz = sz_log2 as i32;
    ((1i32 << sz_log2) * (sz - 3) / sz).max(0)
}

fn hash_blob_spans(buf: &[u8], is_text: bool) -> SpanHashTop {
    let mut hash = SpanHashTop::new(9);
    let mut n = 0u32;
    let mut accum1: u32 = 0;
    let mut accum2: u32 = 0;
    let mut i = 0usize;
    while i < buf.len() {
        let c = buf[i] as u32;
        let old_1 = accum1;
        i += 1;

        if is_text && c == b'\r' as u32 && i < buf.len() && buf[i] == b'\n' {
            continue;
        }

        accum1 = accum1.wrapping_shl(7) ^ accum2.wrapping_shr(25);
        accum2 = accum2.wrapping_shl(7) ^ old_1.wrapping_shr(25);
        accum1 = accum1.wrapping_add(c);
        n += 1;
        if n < 64 && c != b'\n' as u32 {
            continue;
        }
        let hashval = (accum1.wrapping_add(accum2.wrapping_mul(0x61))) % DIFF_HASHBASE;
        hash.add_span(hashval, n);
        n = 0;
        accum1 = 0;
        accum2 = 0;
    }
    if n > 0 {
        let hashval = (accum1.wrapping_add(accum2.wrapping_mul(0x61))) % DIFF_HASHBASE;
        hash.add_span(hashval, n);
    }
    hash.sort_by_hashval();
    hash
}

/// Approximate copied vs added material between two blobs (Git `diffcore_count_changes`).
///
/// Returns `(copied_bytes_from_src, literal_added_bytes_in_dst)` matching Git's
/// `diffcore_count_changes` semantics (used for `--dirstat=changes` damage).
#[must_use]
pub fn diffcore_count_changes(old: &[u8], new: &[u8]) -> (u64, u64) {
    let src_is_text = !crate::merge_file::is_binary(old);
    let dst_is_text = !crate::merge_file::is_binary(new);
    let src_count = hash_blob_spans(old, src_is_text);
    let dst_count = hash_blob_spans(new, dst_is_text);
    let mut sc: u64 = 0;
    let mut la: u64 = 0;
    let mut si = 0usize;
    let mut di = 0usize;
    let src_len = src_count.len();
    let dst_len = dst_count.len();
    loop {
        if si >= src_len || src_count.data[si].cnt == 0 {
            break;
        }
        let s_hash = src_count.data[si].hashval;
        let s_cnt = u64::from(src_count.data[si].cnt);
        while di < dst_len && dst_count.data[di].cnt != 0 && dst_count.data[di].hashval < s_hash {
            la += u64::from(dst_count.data[di].cnt);
            di += 1;
        }
        let mut dst_cnt = 0u64;
        if di < dst_len && dst_count.data[di].cnt != 0 && dst_count.data[di].hashval == s_hash {
            dst_cnt = u64::from(dst_count.data[di].cnt);
            di += 1;
        }
        if s_cnt < dst_cnt {
            la += dst_cnt - s_cnt;
            sc += s_cnt;
        } else {
            sc += dst_cnt;
        }
        si += 1;
    }
    while di < dst_len && dst_count.data[di].cnt != 0 {
        la += u64::from(dst_count.data[di].cnt);
        di += 1;
    }
    (sc, la)
}

/// Whether this modified blob pair should use Git's "complete rewrite" diffstat path when
/// `--break-rewrites` is in effect (`should_break` in `diffcore-break.c`).
#[must_use]
pub fn should_break_rewrite_for_stat(old: &[u8], new: &[u8]) -> bool {
    should_break_rewrite_inner(old, new, DIFF_DEFAULT_BREAK_SCORE)
}

/// Whether an in-place blob edit should be split into delete+create for rename/copy (`should_break`
/// in `diffcore-break.c`). `break_score` is on the internal 0–[`GIT_DIFF_MAX_SCORE`] scale (default
/// [`DIFF_DEFAULT_BREAK_SCORE`]).
#[must_use]
pub fn should_break_rewrite_pair(old: &[u8], new: &[u8], break_score: u64) -> bool {
    should_break_rewrite_inner(old, new, break_score)
}

/// Parse a single Git `parse_rename_score` token (`50`, `50%`, decimal forms) into internal
/// 0–[`GIT_DIFF_MAX_SCORE`] units.
pub fn parse_diff_rename_score_token(arg: &str) -> Option<u64> {
    let mut num: u64 = 0;
    let mut scale: u64 = 1;
    let mut dot = false;
    let mut saw_digit = false;
    for ch in arg.chars() {
        if !dot && ch == '.' {
            scale = 1;
            dot = true;
            continue;
        }
        if ch == '%' {
            scale = if dot { scale.saturating_mul(100) } else { 100 };
            break;
        }
        if ch.is_ascii_digit() {
            saw_digit = true;
            if scale < 100_000 {
                scale = scale.saturating_mul(10);
                num = num.saturating_mul(10) + u64::from(ch as u8 - b'0');
            }
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    Some(if num >= scale {
        GIT_DIFF_MAX_SCORE
    } else {
        GIT_DIFF_MAX_SCORE * num / scale
    })
}

/// Git `merge_score` from `diffcore-break.c` when a pair is considered broken: how much of the
/// source blob was removed (0–[`DIFF_MAX_SCORE`] scale). Used for `dissimilarity index` metadata.
#[must_use]
pub fn rewrite_merge_score(old: &[u8], new: &[u8]) -> Option<u64> {
    if old.is_empty() {
        return None;
    }
    let max_size = old.len().max(new.len());
    if max_size < DIFF_MINIMUM_BREAK_SIZE {
        return None;
    }
    let (src_copied, _) = diffcore_count_changes(old, new);
    let src_copied = src_copied.min(old.len() as u64);
    let src_removed = (old.len() as u64).saturating_sub(src_copied);
    Some(src_removed * DIFF_MAX_SCORE / old.len() as u64)
}

/// Percentage shown in `dissimilarity index N%` for a rewrite (`similarity_index` in Git's diff.c).
#[must_use]
pub fn rewrite_dissimilarity_index_percent(old: &[u8], new: &[u8]) -> Option<u32> {
    let score = rewrite_merge_score(old, new)?;
    Some((score * 100 / DIFF_MAX_SCORE).min(100) as u32)
}

fn should_break_rewrite_inner(src: &[u8], dst: &[u8], break_score: u64) -> bool {
    if src.is_empty() {
        return false;
    }
    let max_size = src.len().max(dst.len());
    if max_size < DIFF_MINIMUM_BREAK_SIZE {
        return false;
    }
    let (src_copied, literal_added) = diffcore_count_changes(src, dst);
    let src_copied = src_copied.min(src.len() as u64);
    let mut literal_added = literal_added;
    let dst_len = dst.len() as u64;
    if src_copied < dst_len && literal_added + src_copied > dst_len {
        literal_added = dst_len.saturating_sub(src_copied);
    }
    let src_removed = (src.len() as u64).saturating_sub(src_copied);
    let merge_score = src_removed * DIFF_MAX_SCORE / src.len() as u64;
    if merge_score > break_score {
        return true;
    }
    let delta_size = src_removed.saturating_add(literal_added);
    if delta_size * DIFF_MAX_SCORE / (max_size as u64) < break_score {
        return false;
    }
    let s = src.len() as u64;
    if (s * break_score < src_removed * DIFF_MAX_SCORE)
        && (literal_added * 20 < src_removed)
        && (literal_added * 20 < src_copied)
    {
        return false;
    }
    true
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Flatten a tree object recursively into a sorted list of (path, mode, oid).
struct FlatEntry {
    path: String,
    mode: u32,
    oid: ObjectId,
}

fn flatten_tree(odb: &Odb, tree_oid: &ObjectId, prefix: &str) -> Result<Vec<FlatEntry>> {
    let entries = read_tree(odb, tree_oid)?;
    let mut result = Vec::new();

    for entry in entries {
        let name_str = String::from_utf8_lossy(&entry.name);
        let path = format_path(prefix, &name_str);
        if is_tree_mode(entry.mode) {
            let nested = flatten_tree(odb, &entry.oid, &path)?;
            result.extend(nested);
        } else {
            result.push(FlatEntry {
                path,
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }

    Ok(result)
}

/// Paths present in `HEAD`'s tree with mode and blob/commit OID (for status porcelain v2).
pub fn head_path_states(
    odb: &Odb,
    head_tree: Option<&ObjectId>,
) -> Result<std::collections::BTreeMap<String, (u32, ObjectId)>> {
    let mut m = std::collections::BTreeMap::new();
    let Some(t) = head_tree else {
        return Ok(m);
    };
    for fe in flatten_tree(odb, t, "")? {
        m.insert(fe.path, (fe.mode, fe.oid));
    }
    Ok(m)
}

/// Whether a mode represents a tree (directory).
fn is_tree_mode(mode: u32) -> bool {
    mode == 0o040000
}

/// Build a path with an optional prefix.
fn format_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Format a numeric mode as a zero-padded octal string.
pub fn format_mode(mode: u32) -> String {
    format!("{mode:06o}")
}

/// Read the HEAD commit OID from a submodule checkout directory.
///
/// Returns `None` if the path is missing, not a submodule checkout, or has no resolvable HEAD.
#[must_use]
pub fn read_submodule_head_for_checkout(sub_dir: &Path) -> Option<ObjectId> {
    read_submodule_head(sub_dir)
}

/// First line of a commit's message for `git diff --submodule=log` output.
///
/// Honors `encoding` in the commit object (Latin-1 vs UTF-8) using the same
/// rules as Git's submodule summary.
#[must_use]
pub fn submodule_commit_subject_line(c: &CommitData) -> String {
    let enc = c.encoding.as_deref().unwrap_or("UTF-8");
    let is_latin1 = enc.eq_ignore_ascii_case("ISO8859-1")
        || enc.eq_ignore_ascii_case("ISO-8859-1")
        || enc.eq_ignore_ascii_case("LATIN1")
        || enc.eq_ignore_ascii_case("ISO-8859-15");
    if let Some(raw) = c.raw_message.as_deref() {
        let line = raw.split(|b| *b == b'\n').next().unwrap_or(raw);
        if is_latin1 {
            return line
                .iter()
                .map(|&b| b as char)
                .collect::<String>()
                .trim()
                .to_owned();
        }
        return String::from_utf8_lossy(line).trim().to_string();
    }
    c.message.lines().next().unwrap_or("").trim().to_owned()
}

/// True when `sub_dir` is an empty directory (or missing), i.e. the placeholder left by
/// `git apply --index` before `git submodule update`.
fn submodule_worktree_is_unpopulated_placeholder(sub_dir: &Path) -> bool {
    match fs::read_dir(sub_dir) {
        Ok(mut it) => it.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn read_submodule_head(sub_dir: &Path) -> Option<ObjectId> {
    read_submodule_head_oid(sub_dir)
}

/// Resolve the embedded git directory for a submodule work tree (`sub_dir/.git`).
#[must_use]
pub fn submodule_embedded_git_dir(sub_dir: &Path) -> Option<PathBuf> {
    let gitfile = sub_dir.join(".git");
    if gitfile.is_file() {
        let content = fs::read_to_string(&gitfile).ok()?;
        let gitdir = content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir: "))?
            .trim();
        Some(if Path::new(gitdir).is_absolute() {
            PathBuf::from(gitdir)
        } else {
            sub_dir.join(gitdir)
        })
    } else if gitfile.is_dir() {
        Some(gitfile)
    } else {
        None
    }
}

/// Walk upward from `sub_dir` to find the nearest containing Git work tree.
fn find_superproject_git(sub_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut cur = sub_dir.parent()?;
    loop {
        let git_path = cur.join(".git");
        if git_path.exists() {
            let gd = if git_path.is_file() {
                let content = fs::read_to_string(&git_path).ok()?;
                let line = content
                    .lines()
                    .find_map(|l| l.strip_prefix("gitdir: "))?
                    .trim();
                if Path::new(line).is_absolute() {
                    PathBuf::from(line)
                } else {
                    cur.join(line)
                }
            } else {
                git_path
            };
            return Some((cur.to_path_buf(), gd));
        }
        cur = cur.parent()?;
    }
}

/// Read the HEAD commit OID from a submodule working tree directory.
///
/// Handles both embedded `.git` directories and `gitdir:` gitfiles pointing at
/// `.git/modules/...` (or other locations). Returns `None` if the path is not
/// a checkout or has no resolvable HEAD.
pub fn read_submodule_head_oid(sub_dir: &Path) -> Option<ObjectId> {
    // Submodule `.git` may be a gitfile pointing at `.git/modules/<name>` in another superproject
    // after `cp -R`. Prefer the current superproject's module dir when present.
    let mut git_dir = submodule_embedded_git_dir(sub_dir)?;
    if let Some((super_wt, super_git_dir)) = find_superproject_git(sub_dir) {
        let rel = sub_dir.strip_prefix(&super_wt).ok()?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let local_mod = super_git_dir
            .join("modules")
            .join(rel_str.trim_start_matches('/'));
        if local_mod.join("HEAD").exists() {
            let sg = super_git_dir.canonicalize().unwrap_or(super_git_dir);
            let cur = git_dir.canonicalize().unwrap_or_else(|_| git_dir.clone());
            if !cur.starts_with(&sg) {
                git_dir = local_mod;
            }
        }
    }
    let head_content = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head_trimmed = head_content.trim();
    if head_trimmed.starts_with("ref: ") {
        // Use the full ref resolver so packed-refs and worktrees match Git. If `HEAD` is a stale
        // symref (e.g. still `refs/heads/master` while only `main` exists), fall back like
        // `resolve_gitlink_ref` / `git add` on embedded repos (`t6437-submodule-merge`).
        match crate::refs::resolve_ref(&git_dir, "HEAD") {
            Ok(oid) => Some(oid),
            Err(_) => {
                let mut found = None;
                for branch in ["main", "master"] {
                    let p = git_dir.join("refs/heads").join(branch);
                    if let Ok(s) = fs::read_to_string(&p) {
                        if let Ok(o) = ObjectId::from_hex(s.trim()) {
                            found = Some(o);
                            break;
                        }
                    }
                }
                found
            }
        }
    } else {
        ObjectId::from_hex(head_trimmed).ok()
    }
}

/// True when a populated submodule checkout is *broken*: its `HEAD` resolves to a commit OID, but
/// that commit object cannot be read from the submodule's own object database.
///
/// This mirrors Git's [`is_submodule_modified`], which shells out to `git status --porcelain=2`
/// inside the submodule; when the submodule's object store is corrupt (e.g. `rm -r .git/objects`),
/// that inner status fails and Git aborts the surrounding `status`/`diff`/`fetch`. We detect the
/// same condition in-process so the superproject operation can return a fatal error rather than
/// silently treating the submodule as clean (t5526 "fetching submodule into a broken repository").
///
/// Returns `false` when the submodule is not checked out, has no embedded git dir, or has an
/// unresolvable HEAD (those are handled separately by the unpopulated/placeholder logic).
#[must_use]
pub fn submodule_head_object_broken(sub_dir: &Path) -> bool {
    let Some(sub_git_dir) = submodule_embedded_git_dir(sub_dir) else {
        return false;
    };
    let Some(head_oid) = read_submodule_head_oid(sub_dir) else {
        return false;
    };
    let odb = Odb::new(&sub_git_dir.join("objects"));
    match odb.read(&head_oid) {
        // HEAD object present but not a commit would be a different kind of corruption; only the
        // missing-object case is the broken-repo scenario Git guards against here.
        Ok(obj) => parse_commit(&obj.data).is_err(),
        Err(_) => true,
    }
}

/// True when a checked-out submodule at `rel_path` has modified or untracked content relative to
/// the gitlink `recorded_oid` stored in the superproject (used for `git diff <tree>` parity).
fn submodule_has_dirty_worktree_for_super_diff(
    super_worktree: &Path,
    rel_path: &str,
    recorded_oid: &ObjectId,
) -> bool {
    let flags = submodule_porcelain_flags(super_worktree, rel_path, *recorded_oid);
    flags.modified || flags.untracked
}

/// Submodule dirty bits aligned with Git's `DIRTY_SUBMODULE_*` / porcelain v2 `S???` token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubmodulePorcelainFlags {
    /// Submodule checkout HEAD differs from the gitlink OID recorded in the parent index.
    pub new_commits: bool,
    /// The submodule has its own staged or unstaged changes (`DIRTY_SUBMODULE_MODIFIED`).
    pub modified: bool,
    /// The submodule work tree contains paths not in its index (`DIRTY_SUBMODULE_UNTRACKED`).
    pub untracked: bool,
}

/// Inspect a checked-out submodule at `rel_path` (relative to `super_worktree`) and return
/// flags used for `git status --porcelain=v2` submodule tokens.
///
/// `recorded_oid` is the gitlink OID stored in the **parent** index (stage 0). When the
/// submodule is not checked out or cannot be opened, returns [`Default::default()`].
pub fn submodule_porcelain_flags(
    super_worktree: &Path,
    rel_path: &str,
    recorded_oid: ObjectId,
) -> SubmodulePorcelainFlags {
    let sub_dir = super_worktree.join(rel_path);
    let Some(sub_git_dir) = submodule_embedded_git_dir(&sub_dir) else {
        return SubmodulePorcelainFlags::default();
    };
    let Some(sub_head) = read_submodule_head_oid(&sub_dir) else {
        return SubmodulePorcelainFlags::default();
    };

    let new_commits = sub_head != recorded_oid;

    let index_path = sub_git_dir.join("index");
    let sub_index = match crate::index::Index::load(&index_path) {
        Ok(ix) => ix,
        Err(_) => {
            return SubmodulePorcelainFlags {
                new_commits,
                ..Default::default()
            }
        }
    };

    let tracked: std::collections::BTreeSet<String> = sub_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    let untracked = submodule_dir_has_untracked_inner(&sub_dir, &sub_dir, &tracked, &sub_index);

    let objects_dir = sub_git_dir.join("objects");
    let odb = Odb::new(&objects_dir);

    let sub_head_tree = (|| -> Option<ObjectId> {
        let h = fs::read_to_string(sub_git_dir.join("HEAD")).ok()?;
        let h_str = h.trim();
        let commit_oid = if let Some(r) = h_str.strip_prefix("ref: ") {
            let oid_hex = fs::read_to_string(sub_git_dir.join(r)).ok()?;
            ObjectId::from_hex(oid_hex.trim()).ok()?
        } else {
            ObjectId::from_hex(h_str).ok()?
        };
        let obj = odb.read(&commit_oid).ok()?;
        let commit = parse_commit(&obj.data).ok()?;
        Some(commit.tree)
    })();

    let staged_dirty = sub_head_tree
        .as_ref()
        .map(|t| diff_index_to_tree(&odb, &sub_index, Some(t), false).map(|v| !v.is_empty()))
        .unwrap_or(Ok(false));
    let staged_dirty = staged_dirty.unwrap_or(false);

    let unstaged_dirty = diff_index_to_worktree(&odb, &sub_index, &sub_dir, false, true)
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    let mut modified = staged_dirty || unstaged_dirty;

    // Nested submodule has its own index: OR `modified` from immediate gitlink children so a
    // dirty nested checkout (e.g. staged `file` under `sub1/sub2`) marks the parent gitlink as
    // modified in the superproject (t7506). Do **not** OR `untracked` — untracked-only inside a
    // nested submodule must stay `S..U` on the parent, not `S.U` / `S.M.`.
    for e in &sub_index.entries {
        if e.stage() != 0 || e.mode != 0o160000 {
            continue;
        }
        let child = String::from_utf8_lossy(&e.path).into_owned();
        let full_rel = if rel_path.is_empty() {
            child
        } else {
            format!("{rel_path}/{child}")
        };
        let nested = submodule_porcelain_flags(super_worktree, &full_rel, e.oid);
        modified |= nested.modified;
    }

    SubmodulePorcelainFlags {
        new_commits,
        modified,
        untracked,
    }
}

fn submodule_dir_has_untracked_inner(
    dir: &Path,
    root: &Path,
    tracked: &std::collections::BTreeSet<String>,
    owning_index: &Index,
) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| name.clone());

        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            let is_gitlink = owning_index
                .get(rel.as_bytes(), 0)
                .is_some_and(|e| e.mode == 0o160000);
            if is_gitlink {
                let Some(nested_git) = submodule_embedded_git_dir(&path) else {
                    continue;
                };
                let nested_index_path = nested_git.join("index");
                let Ok(nested_ix) = crate::index::Index::load(&nested_index_path) else {
                    continue;
                };
                let nested_tracked: std::collections::BTreeSet<String> = nested_ix
                    .entries
                    .iter()
                    .filter(|e| e.stage() == 0)
                    .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                    .collect();
                if submodule_dir_has_untracked_inner(&path, &path, &nested_tracked, &nested_ix) {
                    return true;
                }
            } else if submodule_dir_has_untracked_inner(&path, root, tracked, owning_index) {
                return true;
            }
        } else if !tracked.contains(&rel) {
            return true;
        }
    }
    false
}

/// Reorder diff entries by a `-O<orderfile>` (`diff.orderFile`): entries are sorted by the index of
/// the first orderfile glob pattern that matches their path; unmatched entries sort last (stable).
pub fn apply_orderfile_entries(
    entries: Vec<DiffEntry>,
    order_path: &str,
    cwd: &Path,
) -> Result<Vec<DiffEntry>> {
    apply_orderfile(entries, order_path, cwd)
}

fn apply_orderfile(
    mut entries: Vec<DiffEntry>,
    order_path: &str,
    cwd: &Path,
) -> Result<Vec<DiffEntry>> {
    let patterns = read_orderfile_patterns(order_path, cwd)?;
    let sort_key = |entry: &DiffEntry| -> usize {
        let path = entry
            .new_path
            .as_ref()
            .or(entry.old_path.as_ref())
            .cloned()
            .unwrap_or_default();
        for (i, pat) in patterns.iter().enumerate() {
            if orderfile_pattern_matches(pat, &path) {
                return i;
            }
        }
        patterns.len()
    };
    entries.sort_by_key(|e| sort_key(e));
    Ok(entries)
}

/// Read non-empty, non-comment glob patterns (one per line) from a `-O<orderfile>` file.
pub fn read_orderfile_patterns(order_path: &str, cwd: &Path) -> Result<Vec<String>> {
    let path = Path::new(order_path);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let _meta = std::fs::metadata(&resolved)
        .map_err(|e| Error::Message(format!("could not read orderfile {order_path}: {e}")))?;
    let mut f = std::fs::File::open(&resolved)
        .map_err(|e| Error::Message(format!("could not read orderfile {order_path}: {e}")))?;
    let mut content = String::new();
    std::io::Read::read_to_string(&mut f, &mut content)
        .map_err(|e| Error::Message(format!("could not read orderfile {order_path}: {e}")))?;
    Ok(content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect())
}

/// Reorder diff entries for `git diff` `--rotate-to` / `--skip-to` (changed paths only).
pub fn apply_rotate_skip_entries(
    mut entries: Vec<DiffEntry>,
    rotate_to: Option<&str>,
    skip_to: Option<&str>,
) -> Result<Vec<DiffEntry>> {
    let Some(needle) = rotate_to.or(skip_to) else {
        return Ok(entries);
    };
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(entries);
    }
    let idx = entries
        .iter()
        .position(|e| e.path() == needle)
        .ok_or_else(|| Error::Message(format!("fatal: No such path '{needle}' in the diff")))?;
    if rotate_to.is_some() {
        entries.rotate_left(idx);
    }
    if let Some(skip) = skip_to.filter(|s| !s.trim().is_empty()) {
        let pos = entries
            .iter()
            .position(|e| e.path() == skip)
            .ok_or_else(|| Error::Message(format!("fatal: No such path '{skip}' in the diff")))?;
        entries.drain(..pos);
    }
    Ok(entries)
}

/// `git log` rotate/skip: reorder using the **commit tree** path order (all blobs), then keep only
/// paths present in `entries` — matches Git's `diff --rotate-to` with history walks.
pub fn apply_rotate_skip_log_entries(
    odb: &Odb,
    commit_tree: &ObjectId,
    entries: Vec<DiffEntry>,
    rotate_to: Option<&str>,
    skip_to: Option<&str>,
) -> Result<Vec<DiffEntry>> {
    // Without --rotate-to/--skip-to this is a no-op (the ordered-paths helper
    // returns the entries untouched), so skip the full-tree walk that
    // `all_blob_paths_in_tree_order` performs — per displayed commit it read
    // every subtree of the commit's tree just to discard the result.
    fn trimmed(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|t| !t.is_empty())
    }
    if trimmed(rotate_to).is_none() && trimmed(skip_to).is_none() {
        return Ok(entries);
    }
    let tree_paths = crate::merge_diff::all_blob_paths_in_tree_order(odb, commit_tree);
    apply_rotate_skip_ordered_paths(&tree_paths, entries, rotate_to, skip_to)
}

fn apply_rotate_skip_ordered_paths(
    tree_paths: &[String],
    entries: Vec<DiffEntry>,
    rotate_to: Option<&str>,
    skip_to: Option<&str>,
) -> Result<Vec<DiffEntry>> {
    let rotate = rotate_to.and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    });
    let skip = skip_to.and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    });
    if rotate.is_none() && skip.is_none() {
        return Ok(entries);
    }

    use std::collections::HashMap;
    let mut by_path: HashMap<String, DiffEntry> = HashMap::new();
    for e in entries {
        by_path.insert(e.path().to_string(), e);
    }

    // `git log --skip-to`: only list changed paths from the skip point onward (unmodified paths
    // in the tree-order suffix are omitted). `--rotate-to` still lists every changed file in order.
    if rotate.is_none() {
        let Some(skip_path) = skip else {
            return Ok(by_path.into_values().collect());
        };
        let idx = tree_paths
            .iter()
            .position(|p| p == skip_path)
            .ok_or_else(|| {
                Error::Message(format!("fatal: No such path '{skip_path}' in the diff"))
            })?;
        let mut out = Vec::new();
        for p in tree_paths.iter().skip(idx) {
            if let Some(e) = by_path.remove(p) {
                out.push(e);
            }
        }
        return Ok(out);
    }

    let Some(needle) = rotate else {
        return Ok(by_path.into_values().collect());
    };
    let idx = tree_paths
        .iter()
        .position(|p| p == needle)
        .ok_or_else(|| Error::Message(format!("fatal: No such path '{needle}' in the diff")))?;
    let mut order: Vec<String> = tree_paths.to_vec();
    order.rotate_left(idx);
    if let Some(skip_path) = skip {
        let pos = order.iter().position(|p| p == skip_path).ok_or_else(|| {
            Error::Message(format!("fatal: No such path '{skip_path}' in the diff"))
        })?;
        order.drain(..pos);
    }
    let mut out = Vec::new();
    for p in order {
        if let Some(e) = by_path.remove(&p) {
            out.push(e);
        }
    }
    Ok(out)
}

/// Check if an orderfile pattern matches a path (matches the basename or the full path).
/// Supports basic glob patterns: `*` matches any sequence, `?` matches one char.
pub fn orderfile_pattern_matches(pattern: &str, path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    orderfile_glob_match(pattern, name) || orderfile_glob_match(pattern, path)
}

/// Basic glob matching (supports `*` and `?`).
fn orderfile_glob_match(pattern: &str, text: &str) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let pb = pattern.as_bytes();
    let tb = text.as_bytes();
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < tb.len() {
        if pi < pb.len() && (pb[pi] == b'?' || pb[pi] == tb[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pb.len() && pb[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pb.len() && pb[pi] == b'*' {
        pi += 1;
    }
    pi == pb.len()
}
