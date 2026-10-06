//! Three-way file merge — the engine behind `grit merge-file`.
//!
//! Performs a line-level three-way merge of *base*, *ours*, and *theirs*
//! using the Myers diff algorithm (via the `similar` crate).
//!
//! # Algorithm
//!
//! 1. Split each file into lines, preserving line endings.
//! 2. Annotate each base line with its diff status in `ours` and `theirs`.
//! 3. For each non-Unchanged position, expand the region to fully cover every
//!    overlapping Changed op from either side, then classify as
//!    OnlyOurs / OnlyTheirs / Conflict based on which sides are changed.
//! 4. Emit each hunk: unchanged → base; single-side → that side's content;
//!    two-side → conflict markers (or resolved via `favor`).
//!
//! Pure insertions (ops with empty old_range) are attached to the adjacent
//! base position and emitted as OnlyOurs / OnlyTheirs hunks.

use crate::error::Result;
use similar::{Algorithm, DiffOp, DiffTag};

/// How conflict regions should be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeFavor {
    /// Leave conflict markers in the output (default).
    #[default]
    None,
    /// For conflicts, keep our version.
    Ours,
    /// For conflicts, keep their version.
    Theirs,
    /// For conflicts, concatenate both versions.
    Union,
}

/// Conflict-marker output style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictStyle {
    /// Standard two-section markers (`<<<<<<<`, `=======`, `>>>>>>>`).
    #[default]
    Merge,
    /// Three-section markers including the base section.
    Diff3,
    /// Zealous diff3 (same as Diff3 for our purposes).
    ZealousDiff3,
}

/// Input and options for a three-way merge.
pub struct MergeInput<'a> {
    /// Base (ancestor) content.
    pub base: &'a [u8],
    /// Our version of the file.
    pub ours: &'a [u8],
    /// Their version of the file.
    pub theirs: &'a [u8],
    /// Label for the ours conflict marker line.
    pub label_ours: &'a str,
    /// Label for the base conflict marker line (diff3 only).
    pub label_base: &'a str,
    /// Label for the theirs conflict marker line.
    pub label_theirs: &'a str,
    /// Conflict resolution strategy.
    pub favor: MergeFavor,
    /// Conflict marker style.
    pub style: ConflictStyle,
    /// Width of conflict markers in characters (0 → use default of 7).
    pub marker_size: usize,
    /// Diff algorithm.
    pub diff_algorithm: Option<String>,
    /// Ignore all whitespace when comparing lines (`-w`).
    pub ignore_all_space: bool,
    /// Ignore changes in amount of whitespace (`-b`).
    pub ignore_space_change: bool,
    /// Ignore whitespace at end of line.
    pub ignore_space_at_eol: bool,
    /// Ignore CR at end of line.
    pub ignore_cr_at_eol: bool,
}

/// Result of a three-way merge.
pub struct MergeOutput {
    /// Merged file content.
    pub content: Vec<u8>,
    /// Number of conflict regions (0 = clean merge).
    pub conflicts: usize,
}

/// Perform a three-way line-level merge.
///
/// # Errors
///
/// Currently infallible but returns `Result` for future extension.
pub fn merge(input: &MergeInput<'_>) -> Result<MergeOutput> {
    let base_lines = split_lines(input.base);
    let ours_lines = split_lines(input.ours);
    let theirs_lines = split_lines(input.theirs);
    let ws_mode = WhitespaceMode {
        ignore_all_space: input.ignore_all_space,
        ignore_space_change: input.ignore_space_change,
        ignore_space_at_eol: input.ignore_space_at_eol,
        ignore_cr_at_eol: input.ignore_cr_at_eol,
    };
    // Ignore trailing `\n` / `\r\n` / `\r` when diffing (git `xdl_recmatch`), so clean merges
    // match `git merge-file` when only the final newline differs (t6403).
    let base_compare_lines = lines_for_merge_diff_compare(&base_lines, &ws_mode);
    let ours_compare_lines = lines_for_merge_diff_compare(&ours_lines, &ws_mode);
    let theirs_compare_lines = lines_for_merge_diff_compare(&theirs_lines, &ws_mode);

    let algo = input
        .diff_algorithm
        .as_deref()
        .map(|name| match name.to_lowercase().as_str() {
            "histogram" | "patience" => similar::Algorithm::Patience,
            _ => similar::Algorithm::Myers,
        })
        .unwrap_or(similar::Algorithm::Myers);
    let ours_ops = similar::capture_diff_slices(algo, &base_compare_lines, &ours_compare_lines);
    let theirs_ops = similar::capture_diff_slices(algo, &base_compare_lines, &theirs_compare_lines);

    let mut hunks = compute_hunks(
        &base_lines,
        &ours_lines,
        &theirs_lines,
        &ours_ops,
        &theirs_ops,
        &ws_mode,
    );
    // Zealous diff3 (`adjust_zealous_hunks`) assumes the raw `compute_hunks` sequence.
    if !matches!(input.style, ConflictStyle::ZealousDiff3) {
        hunks = merge_adjacent_replace_and_trailing_insert_conflicts(hunks);
        hunks = fold_adjacent_inserts_into_conflict(hunks);
        hunks = merge_adjacent_one_sided_line_changes_to_conflict(hunks);
    }
    // Match `git merge-file` (`XDL_MERGE_ZEALOUS_ALNUM`): refine then simplify conflicts.
    if !matches!(
        input.style,
        ConflictStyle::Diff3 | ConflictStyle::ZealousDiff3
    ) {
        hunks = refine_conflicts_by_subdiff(hunks, algo, &ws_mode);
        hunks = simplify_non_conflicts_between_conflicts(hunks, true);
    }
    // Git keeps adjacent conflict regions separate when identical lines appear
    // between them (e.g. t4200-rerere); do not merge Conflict+gap+Conflict.
    hunks = coalesce_nearby_conflicts(hunks, 3, false);
    if matches!(input.style, ConflictStyle::ZealousDiff3) {
        hunks = adjust_zealous_hunks(hunks);
    }

    let marker = if input.marker_size == 0 {
        7
    } else {
        input.marker_size
    };

    let mut content: Vec<u8> = Vec::new();
    let mut conflicts = 0usize;
    let mut ours_line_pos = 0usize;
    let mut theirs_line_pos = 0usize;

    for (idx, hunk) in hunks.iter().enumerate() {
        match hunk {
            Hunk::Unchanged(lines) => {
                append_lines(&mut content, lines);
                ours_line_pos += lines.len();
                theirs_line_pos += lines.len();
            }
            Hunk::OnlyOurs { ours, .. } => {
                append_lines(&mut content, ours);
                ours_line_pos += ours.len();
            }
            Hunk::OnlyTheirs { theirs, .. } => {
                append_lines(&mut content, theirs);
                theirs_line_pos += theirs.len();
            }
            Hunk::Conflict { base, ours, theirs } => {
                let conflict_ours_start = ours_line_pos;
                let conflict_theirs_start = theirs_line_pos;
                match input.favor {
                    MergeFavor::Ours => {
                        append_lines(&mut content, ours);
                        ours_line_pos += ours.len();
                    }
                    MergeFavor::Theirs => {
                        append_lines(&mut content, theirs);
                        theirs_line_pos += theirs.len();
                    }
                    MergeFavor::Union => {
                        append_lines(&mut content, ours);
                        // If the ours portion doesn't end with \n and theirs is
                        // non-empty, insert a newline so both sections appear as
                        // separate lines (matches git's missing-LF handling).
                        if !theirs.is_empty()
                            && !ours.is_empty()
                            && !ours.last().map(|l| l.ends_with(b"\n")).unwrap_or(false)
                        {
                            content.push(b'\n');
                        }
                        append_lines(&mut content, theirs);
                        ours_line_pos += ours.len();
                        theirs_line_pos += theirs.len();
                    }
                    MergeFavor::None => {
                        conflicts += 1;
                        if matches!(input.style, ConflictStyle::ZealousDiff3) {
                            let (mut prefix_len, mut suffix_len) =
                                common_prefix_suffix(ours, theirs);

                            if prefix_len > 0
                                && idx > 0
                                && hunk_output_lines(&hunks[idx - 1])
                                    .map(|lines| lines_end_with(lines, &ours[..prefix_len]))
                                    .unwrap_or(false)
                            {
                                prefix_len = 0;
                            }

                            if suffix_len > 0
                                && idx + 1 < hunks.len()
                                && hunk_output_lines(&hunks[idx + 1])
                                    .map(|lines| {
                                        lines_start_with(lines, &ours[ours.len() - suffix_len..])
                                    })
                                    .unwrap_or(false)
                            {
                                suffix_len = 0;
                            }

                            if prefix_len > 0 {
                                append_lines(&mut content, &ours[..prefix_len]);
                            }
                            let i1 = conflict_ours_start + prefix_len;
                            let i2 = conflict_theirs_start + prefix_len;
                            let needs_cr = conflict_markers_need_cr_at(
                                &ours_lines,
                                &theirs_lines,
                                &base_lines,
                                i1,
                                i2,
                            );
                            emit_conflict(
                                &mut content,
                                base,
                                &ours[prefix_len..ours.len() - suffix_len],
                                &theirs[prefix_len..theirs.len() - suffix_len],
                                input.label_ours,
                                input.label_base,
                                input.label_theirs,
                                input.style,
                                marker,
                                needs_cr,
                            );
                            if suffix_len > 0 {
                                append_lines(&mut content, &ours[ours.len() - suffix_len..]);
                            }
                        } else if matches!(input.style, ConflictStyle::Merge) {
                            let (prefix_len, suffix_len) =
                                common_prefix_suffix_merge_lines(ours, theirs);
                            let pre = &ours[..prefix_len];
                            let suf_start = ours.len().saturating_sub(suffix_len);
                            let o_mid = &ours[prefix_len..suf_start];
                            let t_mid =
                                &theirs[prefix_len..theirs.len().saturating_sub(suffix_len)];
                            let suf = &ours[suf_start..];
                            append_lines(&mut content, pre);
                            let i1 = conflict_ours_start + prefix_len;
                            let i2 = conflict_theirs_start + prefix_len;
                            let needs_cr = conflict_markers_need_cr_at(
                                &ours_lines,
                                &theirs_lines,
                                &base_lines,
                                i1,
                                i2,
                            );
                            emit_conflict(
                                &mut content,
                                base,
                                o_mid,
                                t_mid,
                                input.label_ours,
                                input.label_base,
                                input.label_theirs,
                                input.style,
                                marker,
                                needs_cr,
                            );
                            append_lines(&mut content, suf);
                        } else {
                            let needs_cr = conflict_markers_need_cr_at(
                                &ours_lines,
                                &theirs_lines,
                                &base_lines,
                                conflict_ours_start,
                                conflict_theirs_start,
                            );
                            emit_conflict(
                                &mut content,
                                base,
                                ours,
                                theirs,
                                input.label_ours,
                                input.label_base,
                                input.label_theirs,
                                input.style,
                                marker,
                                needs_cr,
                            );
                        }
                        ours_line_pos += ours.len();
                        theirs_line_pos += theirs.len();
                    }
                }
            }
        }
    }

    Ok(MergeOutput { content, conflicts })
}

fn append_lines(out: &mut Vec<u8>, lines: &[Vec<u8>]) {
    // If previous content ended without a newline and this hunk adds more
    // content, insert a newline separator to avoid merging lines together.
    if !out.is_empty() && !out.ends_with(b"\n") && !lines.is_empty() {
        out.push(b'\n');
    }
    for line in lines {
        out.extend_from_slice(line);
    }
}

fn common_prefix_suffix(ours: &[Vec<u8>], theirs: &[Vec<u8>]) -> (usize, usize) {
    let mut prefix = 0usize;
    while prefix < ours.len() && prefix < theirs.len() && ours[prefix] == theirs[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < ours.len().saturating_sub(prefix)
        && suffix < theirs.len().saturating_sub(prefix)
        && ours[ours.len() - 1 - suffix] == theirs[theirs.len() - 1 - suffix]
    {
        suffix += 1;
    }

    (prefix, suffix)
}

fn hunk_output_lines(hunk: &Hunk) -> Option<&[Vec<u8>]> {
    match hunk {
        Hunk::Unchanged(lines) => Some(lines),
        Hunk::OnlyOurs { ours, .. } => Some(ours),
        Hunk::OnlyTheirs { theirs, .. } => Some(theirs),
        Hunk::Conflict { .. } => None,
    }
}

fn lines_end_with(lines: &[Vec<u8>], suffix: &[Vec<u8>]) -> bool {
    if suffix.is_empty() || suffix.len() > lines.len() {
        return false;
    }
    lines[lines.len() - suffix.len()..] == *suffix
}

fn lines_start_with(lines: &[Vec<u8>], prefix: &[Vec<u8>]) -> bool {
    if prefix.is_empty() || prefix.len() > lines.len() {
        return false;
    }
    lines[..prefix.len()] == *prefix
}

/// Emit conflict markers into `out`.
#[allow(clippy::too_many_arguments)]
fn emit_conflict(
    out: &mut Vec<u8>,
    base: &[Vec<u8>],
    ours: &[Vec<u8>],
    theirs: &[Vec<u8>],
    label_ours: &str,
    label_base: &str,
    label_theirs: &str,
    style: ConflictStyle,
    marker: usize,
    needs_cr: bool,
) {
    let open = "<".repeat(marker);
    let eq = "=".repeat(marker);
    let close = ">".repeat(marker);
    let marker_eol: &[u8] = if needs_cr { b"\r\n" } else { b"\n" };

    // Ensure ours section starts on a new line if the previous content didn't
    // end with one.
    if !out.is_empty() && !out.ends_with(b"\n") && !out.ends_with(b"\r\n") {
        out.extend_from_slice(marker_eol);
    }

    write_conflict_marker_line(out, &open, Some(label_ours), marker_eol);
    for line in ours {
        out.extend_from_slice(line);
    }
    // Ensure separator starts on its own line.
    if out.last().copied() != Some(b'\n') {
        out.extend_from_slice(marker_eol);
    }
    match style {
        ConflictStyle::Diff3 | ConflictStyle::ZealousDiff3 => {
            let pipe = "|".repeat(marker);
            write_conflict_marker_line(out, &pipe, Some(label_base), marker_eol);
            for line in base {
                out.extend_from_slice(line);
            }
            if out.last().copied() != Some(b'\n') {
                out.extend_from_slice(marker_eol);
            }
            write_conflict_marker_line(out, &eq, None, marker_eol);
        }
        ConflictStyle::Merge => {
            write_conflict_marker_line(out, &eq, None, marker_eol);
        }
    }
    for line in theirs {
        out.extend_from_slice(line);
    }
    if out.last().copied() != Some(b'\n') {
        out.extend_from_slice(marker_eol);
    }
    write_conflict_marker_line(out, &close, Some(label_theirs), marker_eol);
}

fn write_conflict_marker_line(out: &mut Vec<u8>, marker: &str, label: Option<&str>, eol: &[u8]) {
    out.extend_from_slice(marker.as_bytes());
    // `git rerere` recognizes `<<<<<<< ` / `>>>>>>> ` only when there is a space after the marker
    // run, even if the branch label is empty (`ll_merge` with empty names — `try_replay_merge`).
    let open_or_close = marker.starts_with('<') || marker.starts_with('>');
    let has_label = label.is_some_and(|l| !l.is_empty());
    // Diff3 `|||||||` lines use a space before the base label so they match Git's conflict format
    // (and `print_sanitized_conflicted_diff` in t4108 can strip the label).
    if open_or_close || has_label {
        out.push(b' ');
    }
    if let Some(label) = label {
        if !label.is_empty() {
            out.extend_from_slice(label.as_bytes());
        }
    }
    out.extend_from_slice(eol);
}

/// Strip trailing `\n`, `\r\n`, or lone `\r` from a logical line (content kept for output).
fn strip_line_terminator(line: &[u8]) -> &[u8] {
    if line.ends_with(b"\r\n") {
        return &line[..line.len() - 2];
    }
    if line.ends_with(b"\n") {
        return &line[..line.len() - 1];
    }
    if line.ends_with(b"\r") {
        return &line[..line.len() - 1];
    }
    line
}

/// Compare two lines ignoring trailing newline / CR conventions (matches git `xdl_recmatch` for EOL).
fn merge_line_equal(a: &[u8], b: &[u8]) -> bool {
    strip_line_terminator(a) == strip_line_terminator(b)
}

/// Compare two line sequences line-by-line, ignoring trailing-newline conventions on each line.
/// Mirrors git `xdl_merge_cmp_lines`, used to auto-resolve a both-sides-changed region when the
/// two postimages are textually identical.
fn merge_lines_equal(a: &[Vec<u8>], b: &[Vec<u8>]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| merge_line_equal(x, y))
}

fn common_prefix_suffix_merge_lines(ours: &[Vec<u8>], theirs: &[Vec<u8>]) -> (usize, usize) {
    let mut prefix = 0usize;
    while prefix < ours.len()
        && prefix < theirs.len()
        && merge_line_equal(&ours[prefix], &theirs[prefix])
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < ours.len().saturating_sub(prefix)
        && suffix < theirs.len().saturating_sub(prefix)
        && merge_line_equal(
            &ours[ours.len() - 1 - suffix],
            &theirs[theirs.len() - 1 - suffix],
        )
    {
        suffix += 1;
    }

    (prefix, suffix)
}

/// Git `is_eol_crlf` for a single logical line in our representation.
fn is_eol_crlf_line(line: &[u8]) -> i8 {
    if line.ends_with(b"\r\n") {
        return 1;
    }
    if line.ends_with(b"\n") {
        return 0;
    }
    -1
}

/// Git `is_cr_needed` using indices into the full postimages / ancestor (see `xmerge.c`).
fn conflict_markers_need_cr_at(
    ours_lines: &[Vec<u8>],
    theirs_lines: &[Vec<u8>],
    base_lines: &[Vec<u8>],
    i1: usize,
    i2: usize,
) -> bool {
    let mut needs = if i1 > 0 {
        is_eol_crlf_line(ours_lines[i1 - 1].as_slice())
    } else {
        -1
    };
    if needs > 0 {
        let t0 = if i2 > 0 {
            is_eol_crlf_line(theirs_lines[i2 - 1].as_slice())
        } else {
            -1
        };
        if t0 >= 0 {
            needs = t0;
        }
    }
    if needs > 0 && !base_lines.is_empty() {
        needs = is_eol_crlf_line(base_lines[0].as_slice());
    }
    needs > 0
}

fn line_contains_alnum(line: &[u8]) -> bool {
    strip_line_terminator(line)
        .iter()
        .any(|b| b.is_ascii_alphanumeric())
}

/// `true` when git `xdl_simplify_non_conflicts` would **not** merge `Conflict + gap + Conflict`.
///
/// Git merges the outer conflicts when the gap is "small enough" unless `simplify_if_no_alnum`
/// (`XDL_MERGE_ZEALOUS < level`) requires keeping gaps that are long **and** alphanumeric.
fn gap_blocks_merge_between_conflicts(gap: &[Vec<u8>], simplify_if_no_alnum: bool) -> bool {
    if gap.len() <= 3 {
        return false;
    }
    !simplify_if_no_alnum || gap.iter().any(|l| line_contains_alnum(l.as_slice()))
}

fn merge_two_adjacent_conflicts(left: &Hunk, gap: &[Vec<u8>], right: &Hunk) -> Option<Hunk> {
    let Hunk::Conflict {
        base: b1,
        ours: o1,
        theirs: t1,
    } = left
    else {
        return None;
    };
    let Hunk::Conflict {
        base: b2,
        ours: o2,
        theirs: t2,
    } = right
    else {
        return None;
    };
    // Git's xdl_simplify_non_conflicts absorbs the common gap between the two conflicts back
    // into *both* sides of the merged conflict (the gap lines are identical on ours/theirs).
    let mut merged_base = b1.clone();
    merged_base.extend(gap.iter().cloned());
    merged_base.extend(b2.iter().cloned());
    let mut merged_ours = o1.clone();
    merged_ours.extend(gap.iter().cloned());
    merged_ours.extend(o2.iter().cloned());
    let mut merged_theirs = t1.clone();
    merged_theirs.extend(gap.iter().cloned());
    merged_theirs.extend(t2.iter().cloned());
    Some(Hunk::Conflict {
        base: merged_base,
        ours: merged_ours,
        theirs: merged_theirs,
    })
}

/// Git `xdl_simplify_non_conflicts` for our hunk list.
fn simplify_non_conflicts_between_conflicts(
    hunks: Vec<Hunk>,
    simplify_if_no_alnum: bool,
) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::with_capacity(hunks.len());
    let mut i = 0usize;
    while i < hunks.len() {
        let Hunk::Conflict { .. } = &hunks[i] else {
            out.push(hunks[i].clone());
            i += 1;
            continue;
        };
        let mut merged = hunks[i].clone();
        let mut j = i;
        while let (Some(Hunk::Unchanged(gap)), Some(next @ Hunk::Conflict { .. })) =
            (hunks.get(j + 1), hunks.get(j + 2))
        {
            if gap_blocks_merge_between_conflicts(gap, simplify_if_no_alnum) {
                break;
            }
            let Some(m) = merge_two_adjacent_conflicts(&merged, gap, next) else {
                break;
            };
            merged = m;
            j += 2;
        }
        out.push(merged);
        i = j + 1;
    }
    out
}

/// Split a conflict into sub-hunks by diffing the two sides (git `xdl_refine_conflicts`).
fn refine_conflicts_by_subdiff(
    hunks: Vec<Hunk>,
    algo: Algorithm,
    ws_mode: &WhitespaceMode,
) -> Vec<Hunk> {
    let mut out = Vec::with_capacity(hunks.len());
    for h in hunks {
        let Hunk::Conflict { base, ours, theirs } = h else {
            out.push(h);
            continue;
        };
        if ours.is_empty() || theirs.is_empty() {
            out.push(Hunk::Conflict { base, ours, theirs });
            continue;
        }
        let o_cmp = normalize_lines_for_compare(&ours, ws_mode);
        let t_cmp = normalize_lines_for_compare(&theirs, ws_mode);
        let sub_ops = similar::capture_diff_slices(algo, &o_cmp, &t_cmp);
        if sub_ops.len() == 1 && sub_ops[0].tag() == DiffTag::Equal {
            out.push(Hunk::Unchanged(ours));
            continue;
        }
        for op in &sub_ops {
            let old = op.old_range();
            let new_ = op.new_range();
            match op.tag() {
                DiffTag::Equal => {
                    if !old.is_empty() {
                        out.push(Hunk::Unchanged(ours[old.start..old.end].to_vec()));
                    }
                }
                DiffTag::Delete => {
                    // Lines present only on ours within an overlapping (both-changed) region
                    // stay a conflict with an empty theirs side (git's xdl_refine_conflicts
                    // re-emits these as conflict hunks, not clean resolutions).
                    if !old.is_empty() {
                        out.push(Hunk::Conflict {
                            base: Vec::new(),
                            ours: ours[old.start..old.end].to_vec(),
                            theirs: Vec::new(),
                        });
                    }
                }
                DiffTag::Insert => {
                    if !new_.is_empty() {
                        out.push(Hunk::Conflict {
                            base: Vec::new(),
                            ours: Vec::new(),
                            theirs: theirs[new_.start..new_.end].to_vec(),
                        });
                    }
                }
                DiffTag::Replace => {
                    out.push(Hunk::Conflict {
                        base: Vec::new(),
                        ours: ours[old.start..old.end].to_vec(),
                        theirs: theirs[new_.start..new_.end].to_vec(),
                    });
                }
            }
        }
    }
    out
}

/// A classified merge region (owns its lines).
#[derive(Debug, Clone)]
enum Hunk {
    /// Lines unchanged by both sides (base content).
    Unchanged(Vec<Vec<u8>>),
    /// Lines changed only by ours.
    OnlyOurs {
        /// Base lines for the changed region (empty for pure insertions).
        base: Vec<Vec<u8>>,
        /// Output lines from ours.
        ours: Vec<Vec<u8>>,
    },
    /// Lines changed only by theirs.
    OnlyTheirs {
        /// Base lines for the changed region (empty for pure insertions).
        base: Vec<Vec<u8>>,
        /// Output lines from theirs.
        theirs: Vec<Vec<u8>>,
    },
    /// Lines changed by both sides — a conflict.
    Conflict {
        base: Vec<Vec<u8>>,
        ours: Vec<Vec<u8>>,
        theirs: Vec<Vec<u8>>,
    },
}

/// Compute the merge hunks from two diff sequences against the same base.
///
/// Uses a position-by-position scan with op-based expansion to ensure that
/// changed regions spanning multiple lines are not split mid-op.
fn compute_hunks(
    base: &[Vec<u8>],
    ours: &[Vec<u8>],
    theirs: &[Vec<u8>],
    ours_ops: &[DiffOp],
    theirs_ops: &[DiffOp],
    ws_mode: &WhitespaceMode,
) -> Vec<Hunk> {
    let ours_changed = changed_mask(ours_ops, base.len());
    let theirs_changed = changed_mask(theirs_ops, base.len());

    let ours_inserts = collect_inserts(ours_ops, base.len());
    let theirs_inserts = collect_inserts(theirs_ops, base.len());

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut pos = 0usize;

    while pos <= base.len() {
        // Emit pure insertions before this base position.
        emit_inserts_at(
            pos,
            &ours_inserts,
            &theirs_inserts,
            ours,
            theirs,
            &mut hunks,
        );

        if pos == base.len() {
            break;
        }

        let o = ours_changed[pos];
        let t = theirs_changed[pos];

        if !o && !t {
            // Unchanged run. Stop before a position that has pending insertions
            // on either side so that insertions are emitted in-order at the
            // correct base position.
            let mut end = pos + 1;
            while end < base.len()
                && !ours_changed[end]
                && !theirs_changed[end]
                && ours_inserts[end].is_empty()
                && theirs_inserts[end].is_empty()
            {
                end += 1;
            }
            let terminator_only_diff = base.len() == ours.len()
                && (pos..end).all(|i| merge_line_equal(&base[i], &ours[i]))
                && (pos..end).any(|i| base[i] != ours[i]);
            let unchanged_lines = if ws_mode.ignore_all_space
                || ws_mode.ignore_space_change
                || ws_mode.ignore_space_at_eol
                || ws_mode.ignore_cr_at_eol
                || terminator_only_diff
            {
                &ours[pos..end]
            } else {
                &base[pos..end]
            };
            hunks.push(Hunk::Unchanged(unchanged_lines.to_vec()));
            pos = end;
            continue;
        }

        // Changed region: expand end until all overlapping Changed ops from
        // either side are fully consumed.  Repeat until stable.
        //
        // A region is also extended across *contiguous* changed base lines from EITHER side:
        // when ours changes one base line and theirs changes the very next base line (with no
        // unchanged base line between), Git's `xdl_merge` treats them as a single overlapping
        // region and reports a conflict. Without this, interleaved deletions (e.g. base `a b c d e`,
        // ours `a c e`, theirs `b d`) would be split into alternating one-sided hunks that each
        // "resolve" cleanly, producing an empty merge where Git conflicts (t7201 8).
        let mut end = pos + 1;
        loop {
            let mut new_end = furthest_changed_op_end(ours_ops, pos, end)
                .max(furthest_changed_op_end(theirs_ops, pos, end));
            while new_end < base.len() && (ours_changed[new_end] || theirs_changed[new_end]) {
                new_end += 1;
            }
            if new_end <= end {
                break;
            }
            end = new_end;
        }

        // Classify the full range [pos..end).
        let any_ours = (pos..end).any(|p| ours_changed[p]);
        let any_theirs = (pos..end).any(|p| theirs_changed[p]);

        match (any_ours, any_theirs) {
            (true, false) => {
                let c = collect_new_lines(ours_ops, ours, pos, end);
                hunks.push(Hunk::OnlyOurs {
                    base: base[pos..end].to_vec(),
                    ours: c,
                });
            }
            (false, true) => {
                let c = collect_new_lines(theirs_ops, theirs, pos, end);
                hunks.push(Hunk::OnlyTheirs {
                    base: base[pos..end].to_vec(),
                    theirs: c,
                });
            }
            (true, true) => {
                let o = collect_new_lines(ours_ops, ours, pos, end);
                let t = collect_new_lines(theirs_ops, theirs, pos, end);
                // Git's `xdl_merge` auto-resolves a both-sides-changed region when both sides
                // produce textually identical content (`xdl_merge_cmp_lines` over the two
                // postimages): the change is taken once instead of conflicting. Only a genuine
                // textual difference becomes a conflict (see t3424 `--empty=keep`, where a commit
                // whose change is already present in upstream merges cleanly and becomes empty).
                if merge_lines_equal(&o, &t) {
                    hunks.push(Hunk::OnlyOurs {
                        base: base[pos..end].to_vec(),
                        ours: o,
                    });
                } else {
                    hunks.push(Hunk::Conflict {
                        base: base[pos..end].to_vec(),
                        ours: o,
                        theirs: t,
                    });
                }
            }
            (false, false) => {
                // Should not happen, but treat as unchanged.
                hunks.push(Hunk::Unchanged(base[pos..end].to_vec()));
            }
        }

        pos = end;
    }

    hunks
}

/// Build a boolean mask: `mask[i]` is `true` if base line `i` is covered by
/// a non-Equal op.
fn changed_mask(ops: &[DiffOp], base_len: usize) -> Vec<bool> {
    let mut mask = vec![false; base_len];
    for op in ops {
        if op.tag() == DiffTag::Equal {
            continue;
        }
        for p in op.old_range() {
            if p < base_len {
                mask[p] = true;
            }
        }
    }
    mask
}

/// Collect pure insertions (ops with empty old_range) at each base position.
///
/// Returns a `Vec` of length `base_len + 1`; entry `i` holds all
/// `(new_start, new_end)` ranges inserted before base line `i`.
fn collect_inserts(ops: &[DiffOp], base_len: usize) -> Vec<Vec<(usize, usize)>> {
    let mut result: Vec<Vec<(usize, usize)>> = vec![Vec::new(); base_len + 1];
    for op in ops {
        let old = op.old_range();
        let new_ = op.new_range();
        if old.is_empty() && !new_.is_empty() {
            let pos = old.start.min(base_len);
            result[pos].push((new_.start, new_.end));
        }
    }
    result
}

/// Emit hunks for pure insertions at base position `pos`.
fn emit_inserts_at(
    pos: usize,
    ours_inserts: &[Vec<(usize, usize)>],
    theirs_inserts: &[Vec<(usize, usize)>],
    ours: &[Vec<u8>],
    theirs: &[Vec<u8>],
    hunks: &mut Vec<Hunk>,
) {
    let o_ins = &ours_inserts[pos];
    let t_ins = &theirs_inserts[pos];

    let has_ours = !o_ins.is_empty();
    let has_theirs = !t_ins.is_empty();

    if has_ours && has_theirs {
        // Both sides insert at the same base position (empty base region). Git's
        // `xdl_merge` treats this as a single overlapping change region and only
        // auto-resolves when the two insertions are textually identical; any
        // difference is a conflict. It still factors out a shared leading and
        // trailing run as unchanged context (emitted outside the conflict
        // markers), so we extract a common prefix *and* suffix and conflict on
        // whatever remains -- even when one side's remainder is empty (a
        // "superset" insertion still conflicts; see t4108/t4038).
        let o_lines: Vec<Vec<u8>> = o_ins
            .iter()
            .flat_map(|&(s, e)| ours[s..e].to_vec())
            .collect();
        let t_lines: Vec<Vec<u8>> = t_ins
            .iter()
            .flat_map(|&(s, e)| theirs[s..e].to_vec())
            .collect();

        let mut common_prefix = 0usize;
        while common_prefix < o_lines.len()
            && common_prefix < t_lines.len()
            && o_lines[common_prefix] == t_lines[common_prefix]
        {
            common_prefix += 1;
        }

        let mut common_suffix = 0usize;
        while common_suffix < o_lines.len() - common_prefix
            && common_suffix < t_lines.len() - common_prefix
            && o_lines[o_lines.len() - 1 - common_suffix]
                == t_lines[t_lines.len() - 1 - common_suffix]
        {
            common_suffix += 1;
        }

        if common_prefix > 0 {
            hunks.push(Hunk::Unchanged(o_lines[..common_prefix].to_vec()));
        }

        let ours_tail = o_lines[common_prefix..o_lines.len() - common_suffix].to_vec();
        let theirs_tail = t_lines[common_prefix..t_lines.len() - common_suffix].to_vec();

        if !ours_tail.is_empty() || !theirs_tail.is_empty() {
            // Identical insertions on both sides auto-resolve to a single copy (git
            // `xdl_merge`); only a genuine textual difference conflicts.
            if merge_lines_equal(&ours_tail, &theirs_tail) {
                hunks.push(Hunk::OnlyOurs {
                    base: Vec::new(),
                    ours: ours_tail,
                });
            } else {
                hunks.push(Hunk::Conflict {
                    base: Vec::new(),
                    ours: ours_tail,
                    theirs: theirs_tail,
                });
            }
        }

        if common_suffix > 0 {
            hunks.push(Hunk::Unchanged(
                o_lines[o_lines.len() - common_suffix..].to_vec(),
            ));
        }
    } else if has_ours {
        for &(ns, ne) in o_ins {
            let lines: Vec<Vec<u8>> = ours[ns..ne].to_vec();
            if !lines.is_empty() {
                hunks.push(Hunk::OnlyOurs {
                    base: Vec::new(),
                    ours: lines,
                });
            }
        }
    } else if has_theirs {
        for &(ns, ne) in t_ins {
            let lines: Vec<Vec<u8>> = theirs[ns..ne].to_vec();
            if !lines.is_empty() {
                hunks.push(Hunk::OnlyTheirs {
                    base: Vec::new(),
                    theirs: lines,
                });
            }
        }
    }
}

fn adjust_zealous_hunks(hunks: Vec<Hunk>) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let mut i = 0usize;

    while i < hunks.len() {
        let mut consumed = 1usize;
        let mut transformed: Option<Vec<Hunk>> = None;

        let (pre_insert, mid_idx) = match &hunks[i] {
            Hunk::OnlyTheirs { base, theirs } if base.is_empty() => {
                (Some(theirs.as_slice()), i + 1)
            }
            _ => (None, i),
        };

        if let Some(Hunk::OnlyOurs { base, ours }) = hunks.get(mid_idx) {
            if !base.is_empty() {
                let post_insert = match hunks.get(mid_idx + 1) {
                    Some(Hunk::OnlyTheirs { base, theirs }) if base.is_empty() => {
                        Some(theirs.as_slice())
                    }
                    _ => None,
                };

                let mut prefix_len = 0usize;
                if let Some(pre) = pre_insert {
                    if !pre.is_empty() && ours.starts_with(pre) {
                        prefix_len = pre.len();
                    }
                }

                let mut suffix_len = 0usize;
                if let Some(post) = post_insert {
                    if !post.is_empty() && ours[prefix_len..].ends_with(post) {
                        suffix_len = post.len();
                    }
                }

                if prefix_len > 0 || suffix_len > 0 {
                    consumed = if pre_insert.is_some() {
                        if post_insert.is_some() {
                            3
                        } else {
                            2
                        }
                    } else if post_insert.is_some() {
                        2
                    } else {
                        1
                    };

                    let mut replacement: Vec<Hunk> = Vec::new();
                    if prefix_len > 0 {
                        replacement.push(Hunk::Unchanged(ours[..prefix_len].to_vec()));
                    }
                    replacement.push(Hunk::Conflict {
                        base: base.clone(),
                        ours: ours[prefix_len..ours.len() - suffix_len].to_vec(),
                        theirs: base.clone(),
                    });
                    if suffix_len > 0 {
                        replacement.push(Hunk::Unchanged(ours[ours.len() - suffix_len..].to_vec()));
                    }
                    transformed = Some(replacement);
                }
            }
        }

        if let Some(replacement) = transformed {
            for h in replacement {
                push_hunk_with_unchanged_merge(&mut out, h);
            }
            i += consumed;
            continue;
        }

        push_hunk_with_unchanged_merge(&mut out, hunks[i].clone());
        i += 1;
    }

    out
}

/// If one side replaces base lines and the other appends an insertion immediately after
/// that base span, `compute_hunks` emits two hunks (`Only*` + trailing insert). Git treats
/// the combined region as one conflict (e.g. base `hello\n`, ours adds `hello\n`, theirs
/// replaces with `remove-conflict\n`).
/// When both sides edit **adjacent** base lines but each line is changed on only one side, Git's
/// merge reports a single conflict spanning both lines (`t4108-apply-threeway`).
fn merge_adjacent_one_sided_line_changes_to_conflict(hunks: Vec<Hunk>) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::with_capacity(hunks.len());
    let mut i = 0usize;
    while i < hunks.len() {
        let merged = match (&hunks[i], hunks.get(i + 1)) {
            (
                Hunk::OnlyTheirs {
                    base: b1,
                    theirs: t1,
                },
                Some(Hunk::OnlyOurs { base: b2, ours: o2 }),
            ) if b1.len() == 1 && b2.len() == 1 && t1.len() == 1 && o2.len() == 1 => {
                Some(Hunk::Conflict {
                    base: vec![b1[0].clone(), b2[0].clone()],
                    ours: vec![b1[0].clone(), o2[0].clone()],
                    theirs: vec![t1[0].clone(), b2[0].clone()],
                })
            }
            (
                Hunk::OnlyOurs { base: b1, ours: o1 },
                Some(Hunk::OnlyTheirs {
                    base: b2,
                    theirs: t2,
                }),
            ) if b1.len() == 1 && b2.len() == 1 && o1.len() == 1 && t2.len() == 1 => {
                Some(Hunk::Conflict {
                    base: vec![b1[0].clone(), b2[0].clone()],
                    ours: vec![o1[0].clone(), b2[0].clone()],
                    theirs: vec![b1[0].clone(), t2[0].clone()],
                })
            }
            _ => None,
        };
        if let Some(h) = merged {
            out.push(h);
            i += 2;
        } else {
            out.push(hunks[i].clone());
            i += 1;
        }
    }
    out
}

fn merge_adjacent_replace_and_trailing_insert_conflicts(hunks: Vec<Hunk>) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::with_capacity(hunks.len());
    let mut i = 0usize;
    while i < hunks.len() {
        let merged = match (&hunks[i], hunks.get(i + 1)) {
            (Hunk::OnlyTheirs { base, theirs }, Some(Hunk::OnlyOurs { base: bo, ours: o }))
                if !base.is_empty() && bo.is_empty() && !o.is_empty() && !theirs.is_empty() =>
            {
                Some(Hunk::Conflict {
                    base: base.clone(),
                    ours: o.clone(),
                    theirs: theirs.clone(),
                })
            }
            (
                Hunk::OnlyOurs { base, ours },
                Some(Hunk::OnlyTheirs {
                    base: bt,
                    theirs: t,
                }),
            ) if !base.is_empty() && bt.is_empty() && !t.is_empty() && !ours.is_empty() => {
                Some(Hunk::Conflict {
                    base: base.clone(),
                    ours: ours.clone(),
                    theirs: t.clone(),
                })
            }
            // A pure insertion by one side immediately *before* a base line the other side
            // modifies. `compute_hunks` emits the leading insert (`Only*` with empty base) then
            // the modify (`Only*` with non-empty base) as two separate hunks, but git's
            // `xdl_merge` attaches the insertion to the adjacent change region and reports a
            // single conflict (e.g. base `d`, ours `c\nd`, theirs `e`). Mirror that here.
            (Hunk::OnlyOurs { base: bo, ours }, Some(Hunk::OnlyTheirs { base: bt, theirs }))
                if bo.is_empty() && !bt.is_empty() && !ours.is_empty() && !theirs.is_empty() =>
            {
                let mut conflict_ours = ours.clone();
                conflict_ours.extend(bt.iter().cloned());
                Some(Hunk::Conflict {
                    base: bt.clone(),
                    ours: conflict_ours,
                    theirs: theirs.clone(),
                })
            }
            (Hunk::OnlyTheirs { base: bt, theirs }, Some(Hunk::OnlyOurs { base: bo, ours }))
                if bt.is_empty() && !bo.is_empty() && !theirs.is_empty() && !ours.is_empty() =>
            {
                let mut conflict_theirs = theirs.clone();
                conflict_theirs.extend(bo.iter().cloned());
                Some(Hunk::Conflict {
                    base: bo.clone(),
                    ours: ours.clone(),
                    theirs: conflict_theirs,
                })
            }
            _ => None,
        };
        if let Some(h) = merged {
            out.push(h);
            i += 2;
        } else {
            out.push(hunks[i].clone());
            i += 1;
        }
    }
    out
}

/// Fold a one-sided pure insertion (`Only* { base: [] }`) that sits *immediately
/// adjacent* to a `Conflict` into that conflict's matching side.
///
/// Git's `xdl_merge` attaches an insertion to a neighbouring change region when
/// they touch (`xdl_append_merge`: `i1 <= m->i1 + m->chg1`). After
/// `merge_adjacent_replace_and_trailing_insert_conflicts` has turned a
/// leading-insert + modify into a `Conflict`, a *trailing* one-sided insert that
/// directly follows the conflict (empty base ⇒ no intervening base line) is part
/// of the same region and must be absorbed. This is what makes a previously
/// committed conflict (whose `>>>>>>>` marker line is inserted right after the
/// contested base line) re-conflict correctly — e.g. t4200 "rerere with inner
/// conflict markers", where base `bar`, ours `<<<\nfoo\n===\nbar\n>>>`,
/// theirs `baz` must yield a single conflict containing all of `ours` vs `baz`.
fn fold_adjacent_inserts_into_conflict(hunks: Vec<Hunk>) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        match hunk {
            // A pure insertion directly following a conflict joins that conflict's
            // matching side.
            Hunk::OnlyOurs { ref base, ref ours } if base.is_empty() => {
                if let Some(Hunk::Conflict { ours: c_ours, .. }) = out.last_mut() {
                    c_ours.extend(ours.iter().cloned());
                    continue;
                }
                out.push(hunk);
            }
            Hunk::OnlyTheirs {
                ref base,
                ref theirs,
            } if base.is_empty() => {
                if let Some(Hunk::Conflict {
                    theirs: c_theirs, ..
                }) = out.last_mut()
                {
                    c_theirs.extend(theirs.iter().cloned());
                    continue;
                }
                out.push(hunk);
            }
            other => out.push(other),
        }
    }
    out
}

fn coalesce_nearby_conflicts(hunks: Vec<Hunk>, max_gap_lines: usize, enable: bool) -> Vec<Hunk> {
    if !enable {
        return hunks;
    }
    let mut out: Vec<Hunk> = Vec::new();
    let mut i = 0usize;

    while i < hunks.len() {
        let Some(Hunk::Conflict { base, ours, theirs }) = hunks.get(i) else {
            out.push(hunks[i].clone());
            i += 1;
            continue;
        };

        let mut merged_base = base.clone();
        let mut merged_ours = ours.clone();
        let mut merged_theirs = theirs.clone();
        let mut j = i;

        while let (
            Some(Hunk::Unchanged(gap)),
            Some(Hunk::Conflict {
                base: next_base,
                ours: next_ours,
                theirs: next_theirs,
            }),
        ) = (hunks.get(j + 1), hunks.get(j + 2))
        {
            if gap.len() > max_gap_lines {
                break;
            }

            merged_base.extend(gap.iter().cloned());
            merged_base.extend(next_base.iter().cloned());
            merged_ours.extend(gap.iter().cloned());
            merged_ours.extend(next_ours.iter().cloned());
            merged_theirs.extend(gap.iter().cloned());
            merged_theirs.extend(next_theirs.iter().cloned());
            j += 2;
        }

        out.push(Hunk::Conflict {
            base: merged_base,
            ours: merged_ours,
            theirs: merged_theirs,
        });
        i = j + 1;
    }

    out
}

fn push_hunk_with_unchanged_merge(out: &mut Vec<Hunk>, hunk: Hunk) {
    match hunk {
        Hunk::Unchanged(mut lines) => {
            if let Some(Hunk::Unchanged(prev)) = out.last_mut() {
                prev.append(&mut lines);
            } else if !lines.is_empty() {
                out.push(Hunk::Unchanged(lines));
            }
        }
        other => out.push(other),
    }
}

/// Return the maximum `old_range().end` among all Changed ops that overlap
/// with `[run_start..current_end)`.  Returns `current_end` if nothing
/// extends further.
fn furthest_changed_op_end(ops: &[DiffOp], run_start: usize, current_end: usize) -> usize {
    let mut max = current_end;
    for op in ops {
        if op.tag() == DiffTag::Equal {
            continue;
        }
        let old = op.old_range();
        if old.start < current_end && old.end > run_start && old.end > max {
            max = old.end;
        }
    }
    max
}

/// Collect new (output) lines for base range `[base_start..base_end)`.
///
/// For each op whose `old_range` overlaps the range:
/// - Equal ops contribute their corresponding new-side lines.
/// - Changed ops contribute their full replacement.
fn collect_new_lines(
    ops: &[DiffOp],
    new: &[Vec<u8>],
    base_start: usize,
    base_end: usize,
) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    for op in ops {
        let old = op.old_range();
        let new_ = op.new_range();
        if old.is_empty() {
            continue; // pure insertion, handled separately
        }
        if old.end <= base_start || old.start >= base_end {
            continue; // no overlap
        }
        if op.tag() == DiffTag::Equal {
            let overlap_start = base_start.max(old.start);
            let overlap_end = base_end.min(old.end);
            let offset = overlap_start - old.start;
            let len = overlap_end - overlap_start;
            for i in offset..offset + len {
                if new_.start + i < new_.end {
                    lines.push(new[new_.start + i].clone());
                }
            }
        } else {
            for i in new_.clone() {
                lines.push(new[i].clone());
            }
        }
    }
    lines
}

/// Run Myers diff on two line slices.
fn _diff_ops(old: &[Vec<u8>], new: &[Vec<u8>]) -> Vec<DiffOp> {
    similar::capture_diff_slices(Algorithm::Myers, old, new)
}

/// Split a byte slice into lines, each including its trailing `\n` if present.
fn split_lines(data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for i in 0..data.len() {
        if data[i] == b'\n' {
            lines.push(data[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < data.len() {
        lines.push(data[start..].to_vec());
    }
    lines
}

#[derive(Clone, Copy, Debug, Default)]
struct WhitespaceMode {
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
}

fn normalize_lines_for_compare(lines: &[Vec<u8>], mode: &WhitespaceMode) -> Vec<Vec<u8>> {
    lines
        .iter()
        .map(|line| normalize_line_for_compare(line, mode))
        .collect()
}

fn lines_for_merge_diff_compare(lines: &[Vec<u8>], mode: &WhitespaceMode) -> Vec<Vec<u8>> {
    lines
        .iter()
        .map(|line| normalize_line_for_compare(strip_line_terminator(line), mode))
        .collect()
}

fn normalize_line_for_compare(line: &[u8], mode: &WhitespaceMode) -> Vec<u8> {
    let mut bytes = line.to_vec();

    if mode.ignore_cr_at_eol && bytes.ends_with(b"\r\n") {
        let len = bytes.len();
        bytes.remove(len - 2);
    }

    if mode.ignore_all_space {
        return bytes
            .into_iter()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
    }

    if mode.ignore_space_change {
        let mut out = Vec::with_capacity(bytes.len());
        let mut in_ws = false;
        for ch in bytes {
            if ch.is_ascii_whitespace() {
                if !in_ws {
                    out.push(b' ');
                    in_ws = true;
                }
            } else {
                out.push(ch);
                in_ws = false;
            }
        }
        while out.last().is_some_and(|b| b.is_ascii_whitespace()) {
            out.pop();
        }
        return out;
    }

    if mode.ignore_space_at_eol {
        if bytes.last().copied() == Some(b'\n') {
            let mut body = bytes[..bytes.len() - 1].to_vec();
            while body.last().is_some_and(|b| b.is_ascii_whitespace()) {
                body.pop();
            }
            body.push(b'\n');
            bytes = body;
        } else {
            while bytes.last().is_some_and(|b| b.is_ascii_whitespace()) {
                bytes.pop();
            }
        }
    }

    bytes
}

/// Returns `true` if the byte slice appears to be binary content.
///
/// Uses the same heuristic as git: any NUL byte means binary.
#[must_use]
pub fn is_binary(data: &[u8]) -> bool {
    data.contains(&0u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn do_merge(base: &str, ours: &str, theirs: &str) -> (String, usize) {
        let input = MergeInput {
            base: base.as_bytes(),
            ours: ours.as_bytes(),
            theirs: theirs.as_bytes(),
            label_ours: "ours",
            label_base: "base",
            label_theirs: "theirs",
            favor: MergeFavor::None,
            style: ConflictStyle::Merge,
            marker_size: 7,
            diff_algorithm: None,
            ignore_all_space: false,
            ignore_space_change: false,
            ignore_space_at_eol: false,
            ignore_cr_at_eol: false,
        };
        let out = merge(&input).unwrap();
        (String::from_utf8(out.content).unwrap(), out.conflicts)
    }

    #[test]
    fn no_changes() {
        let t = "line1\nline2\nline3\n";
        let (r, c) = do_merge(t, t, t);
        assert_eq!(r, t);
        assert_eq!(c, 0);
    }

    #[test]
    fn only_ours() {
        let (r, c) = do_merge("a\nb\nc\n", "a\nB\nc\n", "a\nb\nc\n");
        assert_eq!(r, "a\nB\nc\n");
        assert_eq!(c, 0);
    }

    #[test]
    fn only_theirs() {
        let (r, c) = do_merge("a\nb\nc\n", "a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(r, "a\nB\nc\n");
        assert_eq!(c, 0);
    }

    #[test]
    fn conflict() {
        let (r, c) = do_merge("a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n");
        assert_eq!(c, 1);
        assert!(r.contains("<<<<<<< ours\nX\n=======\nY\n>>>>>>> theirs"));
    }

    #[test]
    fn t4108_style_overlapping_edits_conflict() {
        let base = "1\n2\n3\n4\n5\n6\n7\n";
        let ours = "1\ntwo\n3\n4\n5\nsix\n7\n";
        let theirs = "1\n2\n3\n4\nfive\n6\n7\n";
        let (_r, c) = do_merge(base, ours, theirs);
        assert!(
            c >= 1,
            "expected at least one conflict region (Git merge reports CONFLICT for this shape)"
        );
    }

    #[test]
    fn conflict_delete_vs_change() {
        // ours deletes lines 1-2, theirs changes line 1 → conflict
        let base = "a\nb\nc\n";
        let ours = "c\n"; // deleted a and b
        let theirs = "A\nb\nc\n"; // changed a → A
        let (r, c) = do_merge(base, ours, theirs);
        assert_eq!(c, 1, "expected conflict, got: {r:?}");
    }

    #[test]
    fn conflict_replace_vs_insert_after_same_line() {
        // Base one line; ours duplicates it; theirs replaces it — Git reports content conflict.
        let base = "hello\n";
        let ours = "hello\nhello\n";
        let theirs = "remove-conflict\n";
        let (r, c) = do_merge(base, ours, theirs);
        assert_eq!(c, 1, "expected conflict, got: {r:?}");
    }

    #[test]
    fn conflict_insert_before_modified_line() {
        // Base one line; ours inserts a new line *before* it (keeping the base line); theirs
        // modifies that base line. Git's `xdl_merge` attaches the leading insertion to the
        // adjacent change region and reports a content conflict (t3407 `--abort after
        // --continue`: base `d`, ours `c\nd`, theirs `e`).
        let base = "d\n";
        let ours = "c\nd\n";
        let theirs = "e\n";
        let (r, c) = do_merge(base, ours, theirs);
        assert_eq!(c, 1, "expected conflict, got: {r:?}");
        // The ours side of the conflict carries both the inserted and the kept base line.
        assert!(r.contains("c\nd\n"), "ours side should keep c\\nd: {r}");
        assert!(r.contains("e\n"), "theirs side should be e: {r}");
    }

    #[test]
    fn conflict_insert_before_modified_line_symmetric() {
        // Symmetric case: theirs inserts before the base line, ours modifies it.
        let base = "d\n";
        let ours = "e\n";
        let theirs = "c\nd\n";
        let (r, c) = do_merge(base, ours, theirs);
        assert_eq!(c, 1, "expected conflict, got: {r:?}");
    }

    #[test]
    fn clean_insert_separated_from_modified_line() {
        // An insertion separated by unchanged context from the other side's modification must
        // still merge cleanly (git only conflicts when the insertion is *adjacent* to the change).
        let base = "a\nb\nc\n";
        let ours = "x\na\nb\nc\n"; // insert x at the top
        let theirs = "a\nb\nC\n"; // modify the last line, two lines away
        let (r, c) = do_merge(base, ours, theirs);
        assert_eq!(c, 0, "expected clean merge, got: {r:?}");
    }

    #[test]
    fn favor_ours() {
        let input = MergeInput {
            base: b"a\nb\nc\n",
            ours: b"a\nX\nc\n",
            theirs: b"a\nY\nc\n",
            label_ours: "o",
            label_base: "b",
            label_theirs: "t",
            favor: MergeFavor::Ours,
            style: ConflictStyle::Merge,
            marker_size: 7,
            diff_algorithm: None,
            ignore_all_space: false,
            ignore_space_change: false,
            ignore_space_at_eol: false,
            ignore_cr_at_eol: false,
        };
        let out = merge(&input).unwrap();
        assert_eq!(out.content, b"a\nX\nc\n");
        assert_eq!(out.conflicts, 0);
    }

    #[test]
    fn union_missing_lf() {
        let input = MergeInput {
            base: b"line1\nline2\nline3",
            ours: b"line1\nline2\nline3x",
            theirs: b"line1\nline2\nline3y",
            label_ours: "o",
            label_base: "b",
            label_theirs: "t",
            favor: MergeFavor::Union,
            style: ConflictStyle::Merge,
            marker_size: 7,
            diff_algorithm: None,
            ignore_all_space: false,
            ignore_space_change: false,
            ignore_space_at_eol: false,
            ignore_cr_at_eol: false,
        };
        let out = merge(&input).unwrap();
        // union: line3x\nline3y (newline inserted between no-LF lines)
        assert_eq!(out.content, b"line1\nline2\nline3x\nline3y");
    }

    #[test]
    fn zdiff3_interesting_conflict_shape() {
        let input = MergeInput {
            base: b"1\n2\n3\n4\n5\n6\n7\n8\n9\n",
            ours: b"1\n2\n3\n4\nA\nB\nC\nD\nE\nF\nG\nH\nI\nJ\n7\n8\n9\n",
            theirs: b"1\n2\n3\n4\nA\nB\nC\n5\n6\nG\nH\nI\nJ\n7\n8\n9\n",
            label_ours: "HEAD",
            label_base: "base",
            label_theirs: "right^0",
            favor: MergeFavor::None,
            style: ConflictStyle::ZealousDiff3,
            marker_size: 7,
            diff_algorithm: None,
            ignore_all_space: false,
            ignore_space_change: false,
            ignore_space_at_eol: false,
            ignore_cr_at_eol: false,
        };
        let out = merge(&input).unwrap();
        let rendered = String::from_utf8(out.content).unwrap();
        assert_eq!(out.conflicts, 1, "{rendered}");
        assert!(rendered.contains("<<<<<<< HEAD\nD\nE\nF\n"), "{rendered}");
    }

    #[test]
    fn preserves_shared_and_superset_insertions() {
        let input = MergeInput {
            base: b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
            ours: b"1\n2\n3\n4\n5\n5.5\n6\n7\n8\n9\n10\n",
            theirs: b"1\n2\n3\n4\n5\n5.5\n6\n7\n8\n9\n10\n10.5\n",
            label_ours: "ours",
            label_base: "base",
            label_theirs: "theirs",
            favor: MergeFavor::None,
            style: ConflictStyle::Merge,
            marker_size: 7,
            diff_algorithm: None,
            ignore_all_space: false,
            ignore_space_change: false,
            ignore_space_at_eol: false,
            ignore_cr_at_eol: false,
        };
        let base_lines = split_lines(input.base);
        let ours_lines = split_lines(input.ours);
        let theirs_lines = split_lines(input.theirs);
        let ws_mode = WhitespaceMode::default();
        let base_compare_lines = normalize_lines_for_compare(&base_lines, &ws_mode);
        let ours_compare_lines = normalize_lines_for_compare(&ours_lines, &ws_mode);
        let theirs_compare_lines = normalize_lines_for_compare(&theirs_lines, &ws_mode);
        let ours_ops = similar::capture_diff_slices(
            Algorithm::Myers,
            &base_compare_lines,
            &ours_compare_lines,
        );
        let theirs_ops = similar::capture_diff_slices(
            Algorithm::Myers,
            &base_compare_lines,
            &theirs_compare_lines,
        );
        let hunks = compute_hunks(
            &base_lines,
            &ours_lines,
            &theirs_lines,
            &ours_ops,
            &theirs_ops,
            &ws_mode,
        );
        assert_eq!(
            hunks.len(),
            4,
            "expected unchanged shared insertion and theirs-only tail insertion"
        );
        let out = merge(&input).unwrap();
        let rendered = String::from_utf8(out.content).unwrap();
        assert_eq!(out.conflicts, 0, "{rendered}");
        assert_eq!(
            rendered, "1\n2\n3\n4\n5\n5.5\n6\n7\n8\n9\n10\n10.5\n",
            "{rendered}"
        );
    }
}
