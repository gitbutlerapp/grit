//! Interactive `git add -p` — stage selected hunks from the index↔worktree diff.
//!
//! Uses the same Myers line-diff and hunk-splitting approach as [`crate::commands::stash`] patch
//! mode, then writes blended blob content and updated modes into the index.

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::crlf::{self, ConvertToGitOpts};
use grit_lib::diff::{diff_index_to_worktree, mode_from_metadata, DiffStatus};
use grit_lib::index::{Index, IndexEntry, MODE_TREE};
use grit_lib::merge_file::is_binary;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use similar::{Algorithm, TextDiff};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::commands::add::{resolved_env_index_path, AddConfig};
use crate::commands::checkout::{patch_path_filter_matches, resolve_pathspec};
use crate::commands::stash::{partial_unified_for_op_range, split_hunk_at_first_gap};
use grit_lib::index::entry_from_metadata;

const COLOR_RESET: &str = "\x1b[m";

/// Resolve a color slot: `cfg.get(key)` parsed via `parse_color`, else the provided default ANSI
/// escape. Empty config or parse failure falls back to the default.
fn color_slot(cfg: &ConfigSet, key: &str, default_esc: &str) -> String {
    match cfg.get(key) {
        Some(v) if !v.trim().is_empty() => {
            grit_lib::config::parse_color(&v).unwrap_or_else(|_| default_esc.to_owned())
        }
        _ => default_esc.to_owned(),
    }
}

/// Whether a Git colorbool config value means "always on", "off", or "auto".
/// `None` for unset/auto.
fn colorbool_explicit(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "always" => Some(true),
        "never" | "false" | "off" | "0" => Some(false),
        "true" | "on" | "1" => Some(true),
        _ => None, // "auto" or unrecognized: let auto-detection decide
    }
}

/// Auto color decision: colors when stdout is a tty OR `GIT_PAGER_IN_USE` is set, and the
/// terminal is not dumb. `force_color` in t3701 exports `GIT_PAGER_IN_USE=true TERM=vt100`.
fn color_auto() -> bool {
    if crate::editor::is_terminal_dumb() {
        return false;
    }
    (std::io::stdout().is_terminal() && grit_lib::terminal::ansi_supported())
        || std::env::var_os("GIT_PAGER_IN_USE").is_some()
}

/// Resolve whether a given color domain (`color.interactive` / `color.diff`) is active, mirroring
/// git's `check_color_config` + fallback to `color.ui`. An explicit value on the specific key
/// wins; otherwise fall back to `color.ui`; otherwise auto.
fn want_color_domain(cfg: &ConfigSet, key: &str) -> bool {
    if let Some(v) = cfg.get(key) {
        if let Some(explicit) = colorbool_explicit(&v) {
            return explicit;
        }
        // "auto" on the specific key: fall through to auto detection.
        return color_auto();
    }
    if let Some(v) = cfg.get("color.ui") {
        if let Some(explicit) = colorbool_explicit(&v) {
            return explicit;
        }
    }
    color_auto()
}

/// Diff/interactive color slots resolved once per `add -p`/`add -i` invocation.
pub(crate) struct ColorCtx {
    use_interactive: bool,
    use_diff: bool,
    /// `interactive.diffFilter` shell command, if any (applied to the colored diff text).
    diff_filter: Option<String>,
    // diff colors
    meta: String,
    frag: String,
    context: String,
    old: String,
    new: String,
    // interactive colors
    pub(crate) prompt: String,
    pub(crate) header: String,
    pub(crate) help: String,
    pub(crate) error: String,
    pub(crate) reset: String,
}

impl ColorCtx {
    pub(crate) fn from_config(cfg: &ConfigSet) -> Self {
        let use_interactive = want_color_domain(cfg, "color.interactive");
        let use_diff = want_color_domain(cfg, "color.diff");
        let diff_filter = cfg
            .get("interactive.diffFilter")
            .filter(|s| !s.trim().is_empty());
        let on = use_diff;
        let ion = use_interactive;
        ColorCtx {
            use_interactive,
            use_diff,
            diff_filter,
            meta: if on {
                color_slot(cfg, "color.diff.meta", "\x1b[1m")
            } else {
                String::new()
            },
            frag: if on {
                color_slot(cfg, "color.diff.frag", "\x1b[36m")
            } else {
                String::new()
            },
            context: if on {
                color_slot(cfg, "color.diff.context", "")
            } else {
                String::new()
            },
            old: if on {
                color_slot(cfg, "color.diff.old", "\x1b[31m")
            } else {
                String::new()
            },
            new: if on {
                color_slot(cfg, "color.diff.new", "\x1b[32m")
            } else {
                String::new()
            },
            prompt: if ion {
                color_slot(cfg, "color.interactive.prompt", "\x1b[1;34m")
            } else {
                String::new()
            },
            header: if ion {
                color_slot(cfg, "color.interactive.header", "\x1b[1m")
            } else {
                String::new()
            },
            help: if ion {
                color_slot(cfg, "color.interactive.help", "\x1b[1;31m")
            } else {
                String::new()
            },
            error: if ion {
                color_slot(cfg, "color.interactive.error", "\x1b[1;31m")
            } else {
                String::new()
            },
            reset: if on || ion {
                COLOR_RESET.to_owned()
            } else {
                String::new()
            },
        }
    }

    fn any(&self) -> bool {
        self.use_diff || self.use_interactive
    }

    /// Whether interactive (menu/prompt/help) coloring is active.
    pub(crate) fn use_interactive(&self) -> bool {
        self.use_interactive
    }

    /// Wrap `text` in `color` + reset if coloring is active and `color` is non-empty.
    pub(crate) fn wrap(&self, color: &str, text: &str) -> String {
        if color.is_empty() {
            text.to_owned()
        } else {
            format!("{color}{text}{}", self.reset)
        }
    }

    /// Colorize a multi-line diff body (file header lines, `@@` frag lines, and `+`/`-`/` ` body
    /// lines) the way `git diff --color` would, then run it through `interactive.diffFilter` if set.
    /// `text` must end with a newline per line.
    fn colorize_diff(&self, text: &str, ws_highlight_all: bool) -> String {
        let colored = if self.use_diff {
            let mut out = String::new();
            for line in text.split_inclusive('\n') {
                let (body, nl) = match line.strip_suffix('\n') {
                    Some(b) => (b, "\n"),
                    None => (line, ""),
                };
                let colored_line = self.colorize_diff_line(body, ws_highlight_all);
                out.push_str(&colored_line);
                out.push_str(nl);
            }
            out
        } else {
            text.to_owned()
        };
        match &self.diff_filter {
            Some(cmd) => run_diff_filter(cmd, &colored).unwrap_or(colored),
            None => colored,
        }
    }

    fn colorize_diff_line(&self, body: &str, ws_highlight_all: bool) -> String {
        if body.is_empty() {
            return String::new();
        }
        let first = body.as_bytes()[0];
        let (color, is_add) = match first {
            b'@' if body.starts_with("@@") => (&self.frag, false),
            b'+' => (&self.new, true),
            b'-' => (&self.old, false),
            b' ' => (&self.context, false),
            _ if body.starts_with("diff --git")
                || body.starts_with("index ")
                || body.starts_with("--- ")
                || body.starts_with("+++ ")
                || body.starts_with("old mode")
                || body.starts_with("new mode")
                || body.starts_with("new file")
                || body.starts_with("deleted file")
                || body.starts_with("rename ")
                || body.starts_with("copy ")
                || body.starts_with("similarity")
                || body.starts_with("dissimilarity") =>
            {
                (&self.meta, false)
            }
            _ => (&self.context, false),
        };
        // Whitespace-error highlight: trailing whitespace on `+` lines (or all sides when
        // wsErrorHighlight=all) is shown with GIT_COLOR_BG_RED on top of the line color.
        let want_ws = ws_highlight_all && (is_add || first == b'-' || first == b' ');
        if want_ws {
            if let Some(ws_start) = body.rfind(|c: char| !c.is_whitespace()) {
                let split = ws_start + body[ws_start..].chars().next().map_or(1, |c| c.len_utf8());
                if split < body.len() {
                    let head = &body[..split];
                    let tail = &body[split..];
                    let mut s = String::new();
                    if !color.is_empty() {
                        s.push_str(color);
                    }
                    s.push_str(head);
                    s.push_str("\x1b[41m");
                    s.push_str(tail);
                    s.push_str(&self.reset);
                    return s;
                }
            }
        }
        if color.is_empty() {
            body.to_owned()
        } else {
            format!("{color}{body}{}", self.reset)
        }
    }
}

/// Run the `interactive.diffFilter` shell command, piping `input` on stdin and capturing stdout.
fn run_diff_filter(cmd: &str, input: &str) -> Result<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning interactive.diffFilter")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).ok();
    }
    let out = child.wait_with_output().context("running diffFilter")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

use std::io::IsTerminal as _;

/// Blend index and worktree bytes for **staging** (`git add -p`).
///
/// [`checkout::blend_line_diff_by_hunk_ranges`] uses `accepted` with **revert/checkout** semantics
/// (accepted ⇒ keep the index/source side). For `add -p`, user `y` means take the **worktree**
/// side, so we invert the boolean vector.
fn blend_for_stage_hunks(
    index_bytes: &[u8],
    work_bytes: &[u8],
    ranges: &[(usize, usize)],
    stage_yes: &[bool],
) -> String {
    let revert_accepted: Vec<bool> = stage_yes.iter().map(|a| !*a).collect();
    crate::commands::checkout::blend_line_diff_by_hunk_ranges(
        index_bytes,
        work_bytes,
        ranges,
        &revert_accepted,
    )
}

/// Which prompt verb to use for the current file, mirroring Git's `prompt_mode_type`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HunkKind {
    ModeChange,
    Deletion,
    Addition,
    Hunk,
}

/// Build the bracketed permitted-letter suffix used in the interactive prompt, matching
/// `add-patch.c`: navigation letters appear with multiple hunks, `,s` when the hunk can split,
/// `,e` unless the file is a deletion, and `,p,P` always.
fn prompt_suffix(n_hunks: usize, splittable: bool, is_deletion: bool) -> String {
    let mut s = String::new();
    if n_hunks > 1 {
        // ,k / ,K (previous), ,j / ,J (next), ,g,/ for goto/search.
        s.push_str(",k,K,j,J,g,/");
    }
    if splittable {
        s.push_str(",s");
    }
    if !is_deletion {
        s.push_str(",e");
    }
    s.push_str(",p,P");
    s
}

/// Per-hunk decision state mirroring Git's `UNDECIDED_HUNK`/`USE_HUNK`/`SKIP_HUNK`
/// (`add-patch.c`). The interactive hunk loop tracks one of these per hunk so navigation
/// (`j`/`k`/`J`/`K`/`g`/`/`) can find undecided hunks and `(was: y)`/`(was: n)` can annotate
/// already-decided ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Undecided,
    Use,
    Skip,
}

/// `dec_mod` from `add-patch.c`: `(value + max - 1) % max`.
fn dec_mod(value: usize, max: usize) -> usize {
    (value + max - 1) % max
}

/// `summarize_hunk` (`add-patch.c`): one-line summary used by the `g` goto-list, e.g.
/// `_-1,2_+1,3__________+15` (padded to width 20, then the first non-context line of the hunk,
/// truncated to width 80). `hunk_text` is the rendered `@@ ... @@`-headed hunk body.
fn summarize_hunk(hunk_text: &str) -> String {
    const SUMMARY_HEADER_WIDTH: usize = 20;
    const SUMMARY_LINE_WIDTH: usize = 80;
    let mut lines = hunk_text.lines();
    let header = lines.next().unwrap_or("");
    // Parse `@@ -o,c +o,c @@` into the ` -o,c +o,c ` summary prefix git emits.
    let mut out = String::new();
    if let Some(rest) = header.strip_prefix("@@ ") {
        if let Some(idx) = rest.find(" @@") {
            let ranges = &rest[..idx]; // e.g. "-1,2 +1,3"
            let mut parts = ranges.split_whitespace();
            let old = parts.next().unwrap_or("-0,0").trim_start_matches('-');
            let new = parts.next().unwrap_or("+0,0").trim_start_matches('+');
            let (oo, oc) = split_range(old);
            let (no, nc) = split_range(new);
            out = format!(" -{oo},{oc} +{no},{nc} ");
        }
    }
    if out.len() < SUMMARY_HEADER_WIDTH {
        out.push_str(&" ".repeat(SUMMARY_HEADER_WIDTH - out.len()));
    }
    // First line that is not a context line (does not begin with a space).
    for line in lines {
        if !line.starts_with(' ') {
            out.push_str(line);
            break;
        }
    }
    if out.len() > SUMMARY_LINE_WIDTH {
        out.truncate(SUMMARY_LINE_WIDTH);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Parse a unified-diff range `o,c` (or bare `o`, meaning count 1) into `(offset, count)` strings.
fn split_range(s: &str) -> (String, String) {
    match s.split_once(',') {
        Some((o, c)) => (o.to_string(), c.to_string()),
        None => (s.to_string(), "1".to_string()),
    }
}

const DISPLAY_HUNKS_LINES: usize = 20;

/// Sentinel `hunk_ranges` entry for the body-less mode-change pseudo-hunk (git's
/// `file_diff->mode_change`); it maps to no `ops` range.
const MODE_HUNK: (usize, usize) = (usize::MAX, usize::MAX);

/// Render a single hunk (`@@ -o,c +o,c @@` header + body) for the op range `[start, end)`, using
/// **absolute** line offsets from `old_lines`/`new_lines`. Leading/trailing equal runs in the range
/// are capped to `context` lines of surrounding context, matching `git diff`'s hunk headers — which
/// the simpler [`partial_unified_for_op_range`] cannot because it rebuilds offsets from 1.
#[allow(clippy::too_many_arguments)]
fn render_hunk_with_offsets(
    old_lines: &[&str],
    new_lines: &[&str],
    ops: &[similar::DiffOp],
    start: usize,
    end: usize,
    context: usize,
    old_no_nl: bool,
    new_no_nl: bool,
) -> String {
    use similar::DiffOp;
    // Track whether a body entry is the final line of its old/new file so we can emit
    // `\ No newline at end of file` right after it (mirroring git's diff output).
    let old_last = old_lines.len().checked_sub(1);
    let new_last = new_lines.len().checked_sub(1);
    // Collect body lines as (marker, text, no_nl) and track absolute old/new starts and counts.
    let mut body: Vec<(char, String, bool)> = Vec::new();
    let mut old_start: Option<usize> = None; // 0-based
    let mut new_start: Option<usize> = None;
    let mut old_count = 0usize;
    let mut new_count = 0usize;

    let range = &ops[start..end];
    let last = range.len();
    for (k, op) in range.iter().enumerate() {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                // Determine how many context lines to keep: up to `context` at the leading edge
                // (k == 0) and trailing edge (k == last-1); interior equal runs are kept whole.
                let lead = k == 0;
                let trail = k == last - 1;
                let (skip_front, take) = if lead && trail {
                    // Whole-range equal (shouldn't happen for a change hunk): keep all.
                    (0, len)
                } else if lead {
                    let keep = context.min(len);
                    (len - keep, keep)
                } else if trail {
                    (0, context.min(len))
                } else {
                    (0, len)
                };
                for j in skip_front..(skip_front + take) {
                    if old_start.is_none() {
                        old_start = Some(old_index + j);
                        new_start = Some(new_index + j);
                    }
                    // A context line is the file's last line only when it is last on both sides.
                    let no_nl = (old_last == Some(old_index + j) && old_no_nl)
                        && (new_last == Some(new_index + j) && new_no_nl);
                    body.push((' ', old_lines[old_index + j].to_string(), no_nl));
                    old_count += 1;
                    new_count += 1;
                }
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                for j in 0..old_len {
                    if old_start.is_none() {
                        old_start = Some(old_index + j);
                        new_start = Some(new_index);
                    }
                    let no_nl = old_last == Some(old_index + j) && old_no_nl;
                    body.push(('-', old_lines[old_index + j].to_string(), no_nl));
                    old_count += 1;
                }
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                for j in 0..new_len {
                    if old_start.is_none() {
                        old_start = Some(old_index);
                        new_start = Some(new_index + j);
                    }
                    let no_nl = new_last == Some(new_index + j) && new_no_nl;
                    body.push(('+', new_lines[new_index + j].to_string(), no_nl));
                    new_count += 1;
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                for j in 0..old_len {
                    if old_start.is_none() {
                        old_start = Some(old_index + j);
                        new_start = Some(new_index);
                    }
                    let no_nl = old_last == Some(old_index + j) && old_no_nl;
                    body.push(('-', old_lines[old_index + j].to_string(), no_nl));
                    old_count += 1;
                }
                for j in 0..new_len {
                    if old_start.is_none() {
                        old_start = Some(old_index);
                        new_start = Some(new_index + j);
                    }
                    let no_nl = new_last == Some(new_index + j) && new_no_nl;
                    body.push(('+', new_lines[new_index + j].to_string(), no_nl));
                    new_count += 1;
                }
            }
        }
    }

    // Git: a zero-count side uses the bare 0-based position (`-0,0` / `+N,0`); a non-empty side
    // uses the 1-based first line.
    let o_off = match old_start {
        Some(s) if old_count > 0 => s + 1,
        Some(s) => s,
        None => 0,
    };
    let n_off = match new_start {
        Some(s) if new_count > 0 => s + 1,
        Some(s) => s,
        None => 0,
    };
    let o_hdr = if old_count == 1 {
        format!("{o_off}")
    } else {
        format!("{o_off},{old_count}")
    };
    let n_hdr = if new_count == 1 {
        format!("{n_off}")
    } else {
        format!("{n_off},{new_count}")
    };
    let mut s = format!("@@ -{o_hdr} +{n_hdr} @@\n");
    for (m, line, no_nl) in body {
        s.push(m);
        s.push_str(&line);
        s.push('\n');
        if no_nl {
            s.push_str("\\ No newline at end of file\n");
        }
    }
    s
}

/// Compute the natural hunk ranges (`[start, end)` op-index spans) that
/// `git diff -U<context> --inter-hunk-context=<inter>` would emit: two changes separated by an
/// equal run are kept in the same hunk when the run is `<= 2*context + inter` lines (their context
/// regions plus the inter-hunk gap would touch); a longer run starts a new hunk. Each returned
/// range still includes the full separating equal runs (the renderer caps the shown context to
/// `context`), with overlap on the boundary equal run so each hunk renders its surrounding context,
/// mirroring [`split_hunk_into_all`].
pub(crate) fn natural_hunk_ranges(
    ops: &[similar::DiffOp],
    context: usize,
    inter_hunk_context: usize,
) -> Vec<(usize, usize)> {
    let is_eq = |i: usize| matches!(ops.get(i), Some(similar::DiffOp::Equal { .. }));
    let eq_len = |i: usize| match ops.get(i) {
        Some(similar::DiffOp::Equal { len, .. }) => *len,
        _ => 0,
    };
    let split_threshold = 2 * context + inter_hunk_context;
    let n = ops.len();
    let change_idxs: Vec<usize> = (0..n).filter(|&i| !is_eq(i)).collect();
    if change_idxs.is_empty() {
        return vec![(0, n)];
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // `group_first_change`/`prev_change` track the first/last change op of the current hunk.
    let mut group_first_change = change_idxs[0];
    let mut prev_change = change_idxs[0];
    for &c in &change_idxs[1..] {
        // Between two consecutive change ops there is at most one equal op; a big gap (> 2*context)
        // closes the current hunk and starts a new one. The separating equal op is shared so both
        // hunks render context around it.
        let big_gap = (prev_change + 1..c).any(|mid| eq_len(mid) > split_threshold);
        if big_gap {
            // Range start: equal op before the first change (leading context), else the change.
            let start = if group_first_change > 0 && is_eq(group_first_change - 1) {
                group_first_change - 1
            } else {
                group_first_change
            };
            // Range end: include the trailing equal op after the last change.
            let end = if is_eq(prev_change + 1) {
                prev_change + 2
            } else {
                prev_change + 1
            };
            ranges.push((start, end));
            group_first_change = c;
        }
        prev_change = c;
    }
    // Close the final hunk.
    let start = if group_first_change > 0 && is_eq(group_first_change - 1) {
        group_first_change - 1
    } else {
        group_first_change
    };
    let end = if is_eq(prev_change + 1) {
        prev_change + 2
    } else {
        prev_change + 1
    };
    ranges.push((start, end));
    ranges
}

/// Read a full answer line from stdin, returning `None` on EOF and `Some(trimmed)` otherwise
/// (only the trailing newline is stripped; interior/leading spaces are preserved so that
/// `g 1` and `/ pattern` round-trip). Matches `read_single_character`/`strbuf_getline`.
fn read_answer(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
    Ok(Some(trimmed))
}

/// Convert per-hunk decisions to the `accepted: Vec<bool>` the blender expects (USE ⇒ true).
fn decisions_to_accepted(decisions: &[Decision]) -> Vec<bool> {
    decisions.iter().map(|d| *d == Decision::Use).collect()
}

/// Index of the first still-undecided hunk, if any (git's `get_first_undecided`).
fn first_undecided(decisions: &[Decision]) -> Option<usize> {
    decisions.iter().position(|d| *d == Decision::Undecided)
}

/// Fully split a hunk into all of its `splittable_into` sub-hunks (git's `s` splits all the way,
/// not just the first gap). Returns true if at least one split happened.
///
/// Unlike [`split_hunk_at_first_gap`] (a hard op-range partition), the boundary equal run is
/// **shared** by the two adjacent sub-hunks so each one renders with the surrounding context git
/// shows (the `@@ ... @@` line and the trailing/leading context lines). Equal ops never consult
/// `accepted` during blending, so the overlap is harmless to staging.
fn split_hunk_into_all(
    ranges: &mut Vec<(usize, usize)>,
    hunk_index: usize,
    ops: &[similar::DiffOp],
) -> bool {
    if hunk_index >= ranges.len() {
        return false;
    }
    let (start, end) = ranges[hunk_index];
    let is_eq = |i: usize| matches!(ops.get(i), Some(similar::DiffOp::Equal { .. }));

    // Find the boundaries of each maximal change-run, then split at the middle of the equal runs
    // that separate them. `boundaries[k]` is `(eq_run_start, eq_run_end)` for the k-th internal gap.
    let mut sub: Vec<(usize, usize)> = Vec::new();
    let mut i = start;
    // Leading context.
    while i < end && is_eq(i) {
        i += 1;
    }
    let mut seg_start = start;
    while i < end {
        // Consume a change run.
        while i < end && !is_eq(i) {
            i += 1;
        }
        // Consume the following equal run.
        let eq_start = i;
        while i < end && is_eq(i) {
            i += 1;
        }
        if eq_start < i && i < end {
            // Internal gap: end this sub-hunk after the equal run, start the next at its start so
            // the equal run is shared as trailing/leading context.
            sub.push((seg_start, i));
            seg_start = eq_start;
        }
    }
    sub.push((seg_start, end));

    if sub.len() < 2 {
        return false;
    }
    ranges.splice(hunk_index..=hunk_index, sub);
    true
}

/// Render the `g`-command hunk list (git's `display_hunks`): up to `DISPLAY_HUNKS_LINES` lines
/// starting at `start`, each `%c%2d: <summary>`. Returns the index one past the last shown.
fn display_hunk_list(
    out: &mut impl Write,
    ranges: &[(usize, usize)],
    decisions: &[Decision],
    work: &[u8],
    start: usize,
    render_hunk: &dyn Fn(usize, &[(usize, usize)], &[u8]) -> String,
) -> usize {
    let end = (start + DISPLAY_HUNKS_LINES).min(ranges.len());
    for (i, dec) in decisions.iter().enumerate().take(end).skip(start) {
        let mark = match dec {
            Decision::Use => '+',
            Decision::Skip => '-',
            Decision::Undecided => ' ',
        };
        let text = render_hunk(i, ranges, work);
        let summary = summarize_hunk(&text);
        write!(out, "{mark}{:>2}: {summary}", i + 1).ok();
    }
    end
}

/// `?` help during the hunk loop. Git prints the always-available lines, then only the remainder
/// lines whose command character is present in the current `nav` suffix. When every hunk in the
/// file has been decided (only possible under `--no-auto-advance`), it also appends the
/// `HUNKS SUMMARY` line with `Some((total, use, skip))`.
fn write_patch_help(out: &mut impl Write, nav: &str, summary: Option<(usize, usize, usize)>) {
    let base = "y - stage this hunk\n\
                n - do not stage this hunk\n\
                q - quit; do not stage this hunk or any of the remaining ones\n\
                a - stage this hunk and all later hunks in the file\n\
                d - do not stage this hunk or any of the later hunks in the file\n";
    write!(out, "{base}").ok();
    // Remainder lines, each gated on its command character appearing in `nav`.
    let remainder: &[(char, &str)] = &[
        (
            'k',
            "k - leave this hunk undecided, see previous undecided hunk",
        ),
        ('K', "K - leave this hunk undecided, see previous hunk"),
        (
            'j',
            "j - leave this hunk undecided, see next undecided hunk",
        ),
        ('J', "J - leave this hunk undecided, see next hunk"),
        ('g', "g - select a hunk to go to"),
        ('/', "/ - search for a hunk matching the given regex"),
        ('s', "s - split the current hunk into smaller hunks"),
        ('e', "e - manually edit the current hunk"),
        ('p', "p - print the current hunk"),
    ];
    for (ch, line) in remainder {
        if nav.contains(*ch) {
            writeln!(out, "{line}").ok();
        }
    }
    writeln!(out, "? - print help").ok();
    if let Some((total, used, skipped)) = summary {
        writeln!(
            out,
            "HUNKS SUMMARY - Hunks: {total}, USE: {used}, SKIP: {skipped}"
        )
        .ok();
    }
}

/// 7-character abbreviated blob OID for `data` (Git's default short hash in patch headers).
fn short_oid_of(odb: &Odb, data: &[u8]) -> String {
    let _ = odb;
    let oid = Odb::hash_object_data(ObjectKind::Blob, data);
    oid.to_hex().chars().take(7).collect()
}

/// Number of sub-hunks the op range `start..end` would split into (gap-based, matching
/// [`split_hunk_at_first_gap`]): one more than the count of internal equal-runs flanked by changes.
fn splittable_into(ops: &[similar::DiffOp], start: usize, end: usize) -> usize {
    let is_eq = |i: usize| matches!(ops.get(i), Some(similar::DiffOp::Equal { .. }));
    let mut count = 1usize;
    let mut i = start;
    // Skip leading context.
    while i < end && is_eq(i) {
        i += 1;
    }
    while i < end {
        // Consume a run of changes.
        while i < end && !is_eq(i) {
            i += 1;
        }
        // Consume the following equal run; if more changes follow, this is a split point.
        let eq_start = i;
        while i < end && is_eq(i) {
            i += 1;
        }
        if eq_start < i && i < end {
            count += 1;
        }
    }
    count
}

/// Tunables for `git add -p` that come from `-U`/`--inter-hunk-context`/`--no-auto-advance`
/// (or the corresponding `diff.*` config). Resolved in [`crate::commands::add`].
pub(crate) struct PatchOptions {
    /// Number of context lines around each hunk (default 3).
    pub context: usize,
    /// Context lines kept between otherwise-adjacent hunks (default 0).
    pub inter_hunk_context: usize,
    /// Whether to auto-advance to the next hunk after a decision (default true).
    pub auto_advance: bool,
}

impl Default for PatchOptions {
    fn default() -> Self {
        Self {
            context: 3,
            inter_hunk_context: 0,
            auto_advance: true,
        }
    }
}

/// Run `git add -p` / `git add --patch`.
pub(crate) fn run_add_patch(
    repo: &Repository,
    pathspecs: &[String],
    add_cfg: &AddConfig,
    opts: &PatchOptions,
) -> Result<()> {
    run_add_patch_with_reader(repo, pathspecs, add_cfg, opts, None)
}

/// Like [`run_add_patch`] but lets a caller (e.g. `add -i`'s patch sub-command) thread its own
/// already-buffered stdin reader through, so input is not lost between the two BufReaders.
///
/// # Errors
/// Propagates I/O, ODB, and index errors.
pub(crate) fn run_add_patch_with_reader(
    repo: &Repository,
    pathspecs: &[String],
    add_cfg: &AddConfig,
    opts: &PatchOptions,
    external_reader: Option<&mut dyn BufRead>,
) -> Result<()> {
    let inter_hunk_context = opts.inter_hunk_context;
    let auto_advance = opts.auto_advance;
    let context = opts.context;

    // `git add -p` shells out to `git diff-files`, which validates `diff.algorithm`. A bogus value
    // (e.g. `-c diff.algorithm=bogus`) aborts before any prompt (t3701 "diff.algorithm is passed").
    if let Some(algo) = add_cfg.config.get("diff.algorithm") {
        let a = algo.trim();
        if !a.is_empty()
            && !matches!(
                a.to_ascii_lowercase().as_str(),
                "myers" | "default" | "minimal" | "patience" | "histogram"
            )
        {
            bail!(
                "option diff-algorithm accepts \"myers\", \"minimal\", \"patience\" and \"histogram\""
            );
        }
    }

    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let cwd = std::env::current_dir().context("resolving cwd")?;
    let filter_paths: Vec<String> = pathspecs
        .iter()
        .map(|p| resolve_pathspec(p, work_tree, &cwd))
        .collect();

    let index_path = resolved_env_index_path(repo);
    let raw_index = Index::load(&index_path).unwrap_or_else(|_| Index::new());
    let mut index = repo.load_index_at(&index_path).context("loading index")?;

    let mut entries = diff_index_to_worktree(&repo.odb, &index, work_tree, false, false)?;
    entries.retain(|e| {
        if e.status == DiffStatus::Unmerged {
            return false;
        }
        patch_path_filter_matches(&e.path().to_string_lossy(), &filter_paths)
    });
    entries.sort_by(|a, b| a.path().cmp(b.path()));

    if entries
        .iter()
        .any(|entry| path_under_sparse_index_dir(&raw_index, &entry.path().to_string_lossy()))
    {
        emit_index_trace_region("ensure_full_index");
    }

    if entries.is_empty() {
        println!("No changes.");
        return Ok(());
    }

    let stdin = io::stdin();
    let mut owned_reader;
    let mut reader: &mut dyn BufRead = match external_reader {
        Some(r) => r,
        None => {
            owned_reader = stdin.lock();
            &mut owned_reader
        }
    };
    let mut out = io::stdout();

    let odb = &repo.odb;
    let conv = &add_cfg.conv;
    let attrs = &add_cfg.attrs;

    // Color/diffFilter context (color.diff / color.interactive / interactive.diffFilter), resolved
    // once. In t3701 `force_color` exports GIT_PAGER_IN_USE=true TERM=vt100 to force `auto` on.
    let cctx = ColorCtx::from_config(&add_cfg.config);
    // diff.wsErrorHighlight: whether to highlight whitespace errors in the colored diff.
    let ws_highlight_all = add_cfg
        .config
        .get("diff.wsErrorHighlight")
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.split(',').any(|t| t.trim() == "all")
        })
        .unwrap_or(false);

    // Track how many candidate files turned out to be binary; if every one did, Git prints
    // "Only binary files changed." (add-patch.c) instead of silently doing nothing.
    let total_entries = entries.len();
    let mut binary_count = 0usize;

    for entry in entries {
        // TODO(byte-paths): lossy worktree path until add-patch FS I/O migrates (Phase 2).
        let path_str = entry.path().to_string_lossy().into_owned();
        let path_bytes = path_str.as_bytes();

        let Some(ie) = index.get(path_bytes, 0).cloned() else {
            continue;
        };

        if ie.mode == 0o160000 {
            continue;
        }

        let abs_path = work_tree.join(&path_str);
        let meta = match fs::symlink_metadata(&abs_path) {
            Ok(m) => m,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(20) /* ENOTDIR */ =>
            {
                if entry.status != DiffStatus::Deleted {
                    continue;
                }
                handle_deleted_file(
                    repo,
                    &mut index,
                    index_path.as_path(),
                    &path_str,
                    &ie,
                    &mut reader,
                    &mut out,
                    odb,
                )?;
                continue;
            }
            Err(_) => continue,
        };

        let file_attrs = crlf::get_file_attrs(attrs, &path_str, false, &add_cfg.config);

        let index_blob = if ie.oid == ObjectId::zero() {
            Vec::new()
        } else {
            let obj = match odb.read(&ie.oid) {
                Ok(o) if o.kind == ObjectKind::Blob => o.data,
                _ => continue,
            };
            obj
        };

        let work_blob = if meta.file_type().is_symlink() {
            let target = fs::read_link(&abs_path)?;
            target.to_string_lossy().into_owned().into_bytes()
        } else {
            let raw = fs::read(&abs_path).unwrap_or_default();
            let prior_blob = if ie.oid != ObjectId::zero() {
                Some(index_blob.clone())
            } else {
                None
            };
            let opts = ConvertToGitOpts {
                index_blob: prior_blob.as_deref(),
                renormalize: false,
                check_safecrlf: true,
            };
            match crlf::convert_to_git_with_opts(&raw, &path_str, conv, &file_attrs, opts) {
                Ok(c) => c,
                Err(msg) => {
                    eprintln!("{msg}");
                    continue;
                }
            }
        };

        if is_binary(&index_blob) || is_binary(&work_blob) {
            binary_count += 1;
            continue;
        }

        // An intent-to-add path (or a `DiffStatus::Added` entry) is rendered as a *new file*: the
        // index side is empty and the prompt verb is "Stage addition", with no mode-change prompt.
        let is_addition = entry.status == DiffStatus::Added || ie.intent_to_add();
        // A whole-file deletion (the worktree path is gone): Git renders this as `Stage deletion`
        // with no split (`s`) or edit (`e`) option, and the file header shows `deleted file mode`.
        let is_deletion = entry.status == DiffStatus::Deleted;
        let mode_differs =
            !is_addition && parse_mode_u32(&entry.old_mode) != parse_mode_u32(&entry.new_mode);
        let content_differs = index_blob != work_blob;

        let mut effective_mode = ie.mode;
        let index_side_bytes = index_blob.clone();

        // A mode-only change (no content diff) is presented as a standalone `(1/1) Stage mode
        // change` prompt here; when content ALSO differs, the mode change is folded into the hunk
        // loop below as hunk 0 (git's `file_diff->mode_change`).
        if mode_differs && !content_differs {
            write!(out, "(1/1) Stage mode change [y,n,q,a,d,p,P,?]? ").ok();
            out.flush().ok();
            match read_one_command(&mut reader, &mut out)? {
                ReadCmd::Eof => {
                    writeln!(out).ok();
                    repo.write_index_at(&index_path, &mut index)?;
                    return Ok(());
                }
                ReadCmd::Invalid => {}
                ReadCmd::Char { lower, .. } => match lower {
                    'y' => effective_mode = mode_from_metadata(&meta),
                    'q' => {
                        writeln!(out).ok();
                        repo.write_index_at(&index_path, &mut index)?;
                        return Ok(());
                    }
                    _ => {}
                },
            }
        }

        if !content_differs {
            if mode_differs && effective_mode != ie.mode {
                write_index_blob_and_mode(
                    odb,
                    &mut index,
                    &path_str,
                    &abs_path,
                    &index_side_bytes,
                    effective_mode,
                )?;
            }
            continue;
        }

        let mut cur_work = work_blob;

        // A labeled block (not a loop): control flow only ever `break 'rediff`s
        // out of it, so the body runs at most once.
        'rediff: {
            let index_str = String::from_utf8_lossy(&index_side_bytes);
            let work_str = String::from_utf8_lossy(&cur_work);
            let text_diff = TextDiff::configure()
                .algorithm(Algorithm::Myers)
                .diff_lines(index_str.as_ref(), work_str.as_ref());
            let ops: Vec<_> = text_diff.ops().to_vec();
            let has_change = ops
                .iter()
                .any(|o| !matches!(o, similar::DiffOp::Equal { .. }));
            if !has_change {
                if mode_differs && effective_mode != ie.mode {
                    write_index_blob_and_mode(
                        odb,
                        &mut index,
                        &path_str,
                        &abs_path,
                        &index_side_bytes,
                        effective_mode,
                    )?;
                }
                break 'rediff;
            }

            let n_ops = ops.len();
            // Additions/deletions are a single whole-file hunk; otherwise split into the natural
            // hunks `git diff -U<context>` would produce so navigation (j/k/J/K/g//) works without
            // requiring `s` first.
            let mut hunk_ranges: Vec<(usize, usize)> = if is_addition || is_deletion {
                vec![(0, n_ops)]
            } else {
                natural_hunk_ranges(&ops, context, inter_hunk_context)
            };
            // A mode change accompanying content is hunk 0 (git's `file_diff->mode_change`): a
            // body-less pseudo-hunk marked by the `MODE_HUNK` sentinel range. It has no `s`/`e`.
            let mode_hunk = mode_differs;
            if mode_hunk {
                hunk_ranges.insert(0, MODE_HUNK);
            }
            let mut decisions = vec![Decision::Undecided; hunk_ranges.len()];
            let mut hunk_index = 0usize;
            // -1 means "nothing rendered yet"; the hunk body is only re-printed when the cursor
            // lands on a different hunk (matching `rendered_hunk_index` in `add-patch.c`). Split
            // resets this to force a re-render of the now-current hunk.
            let mut rendered_hunk_index: isize = -1;

            // Render the file diff header once per file (git: `render_diff_header`).
            let mut header = String::new();
            header.push_str(&format!("diff --git a/{path_str} b/{path_str}\n"));
            if is_addition {
                let short = short_oid_of(odb, &cur_work);
                let new_mode = mode_from_metadata(&meta);
                header.push_str(&format!("new file mode {new_mode:06o}\n"));
                header.push_str(&format!("index 0000000..{short}\n"));
                header.push_str(&format!("--- /dev/null\n+++ b/{path_str}\n"));
            } else if is_deletion {
                let short = short_oid_of(odb, &index_side_bytes);
                header.push_str(&format!("deleted file mode {:06o}\n", ie.mode));
                header.push_str(&format!("index {short}..0000000\n"));
                header.push_str(&format!("--- a/{path_str}\n+++ /dev/null\n"));
            } else {
                header.push_str(&format!("--- a/{path_str}\n+++ b/{path_str}\n"));
            }
            // Colorize the header (and run it through interactive.diffFilter) when color is active.
            // The diffFilter is applied per-block; the test cases (65/67) match on individual lines.
            if cctx.any() {
                write!(out, "{}", cctx.colorize_diff(&header, ws_highlight_all)).ok();
            } else {
                write!(out, "{header}").ok();
            }

            // Render the hunk body text for hunk `i` (header `@@ ... @@` + body lines) with the
            // absolute line offsets `git diff` shows. `work` may change after an `e` edit, which
            // re-diffs, so the ops/lines for that path are recomputed by the caller on `rediff`.
            let index_side_str = String::from_utf8_lossy(&index_side_bytes).into_owned();
            let old_lines: Vec<&str> = index_side_str.lines().collect();
            let old_no_nl = !index_side_bytes.is_empty() && !index_side_bytes.ends_with(b"\n");
            let render_hunk = |i: usize, ranges: &[(usize, usize)], work: &[u8]| -> String {
                let (s, e) = ranges[i];
                if (s, e) == MODE_HUNK {
                    // The mode-change pseudo-hunk has no diff body.
                    return String::new();
                }
                let work_str = String::from_utf8_lossy(work).into_owned();
                let new_lines: Vec<&str> = work_str.lines().collect();
                let new_no_nl = !work.is_empty() && !work.ends_with(b"\n");
                let plain = render_hunk_with_offsets(
                    &old_lines, &new_lines, &ops, s, e, context, old_no_nl, new_no_nl,
                );
                if cctx.any() {
                    cctx.colorize_diff(&plain, ws_highlight_all)
                } else {
                    plain
                }
            };

            // `interactive.diffFilter` mismatch check (git's parse_diff): the filter must preserve
            // the diff's line structure. Build the full plain diff (header + every hunk body),
            // colorize it, run the filter, and if the filtered output has fewer lines than the
            // plain diff, abort with "mismatched output" (t3701 "detect bogus diffFilter output").
            if let Some(filter) = &cctx.diff_filter {
                let mut full_plain = header.clone();
                for i in 0..hunk_ranges.len() {
                    full_plain.push_str(&{
                        let (s, e) = hunk_ranges[i];
                        if (s, e) == MODE_HUNK {
                            String::new()
                        } else {
                            let work_str = String::from_utf8_lossy(&cur_work).into_owned();
                            let new_lines: Vec<&str> = work_str.lines().collect();
                            let new_no_nl = !cur_work.is_empty() && !cur_work.ends_with(b"\n");
                            render_hunk_with_offsets(
                                &old_lines, &new_lines, &ops, s, e, context, old_no_nl, new_no_nl,
                            )
                        }
                    });
                }
                let colored_full = {
                    let mut out = String::new();
                    for line in full_plain.split_inclusive('\n') {
                        let (body, nl) = match line.strip_suffix('\n') {
                            Some(b) => (b, "\n"),
                            None => (line, ""),
                        };
                        out.push_str(&cctx.colorize_diff_line(body, ws_highlight_all));
                        out.push_str(nl);
                    }
                    out
                };
                if let Ok(filtered) = run_diff_filter(filter, &colored_full) {
                    let plain_lines = full_plain.lines().count();
                    let filtered_lines = filtered.lines().count();
                    if filtered_lines < plain_lines {
                        bail!("mismatched output from interactive.diffFilter\nPlease make sure that the filter correctly indicates all lines, and that no lines are added or removed.");
                    }
                }
            }

            'hunk_loop: loop {
                let n_hunks = hunk_ranges.len();
                if hunk_index >= n_hunks {
                    hunk_index = 0;
                }

                // Find the nearest undecided hunk before/after the cursor (cyclic), git's
                // `undecided_previous`/`undecided_next`.
                let mut undecided_previous: Option<usize> = None;
                let mut undecided_next: Option<usize> = None;
                if n_hunks > 0 {
                    let mut i = dec_mod(hunk_index, n_hunks);
                    while i != hunk_index {
                        if decisions[i] == Decision::Undecided {
                            undecided_previous = Some(i);
                            break;
                        }
                        i = dec_mod(i, n_hunks);
                    }
                    let mut i = (hunk_index + 1) % n_hunks;
                    while i != hunk_index {
                        if decisions[i] == Decision::Undecided {
                            undecided_next = Some(i);
                            break;
                        }
                        i = (i + 1) % n_hunks;
                    }
                }

                // Everything decided? Without auto-advance we keep showing the (last) hunk and the
                // `?` help gains the HUNKS SUMMARY line; with auto-advance we move past the file.
                let all_decided = undecided_previous.is_none()
                    && undecided_next.is_none()
                    && decisions[hunk_index] != Decision::Undecided;
                if all_decided && auto_advance {
                    break 'hunk_loop;
                }

                let is_mode_hunk = hunk_ranges[hunk_index] == MODE_HUNK;
                if rendered_hunk_index != hunk_index as isize {
                    // The mode-change pseudo-hunk has no diff body to print.
                    if !is_mode_hunk {
                        write!(out, "{}", render_hunk(hunk_index, &hunk_ranges, &cur_work)).ok();
                    }
                    rendered_hunk_index = hunk_index as isize;
                }

                let (s, e) = hunk_ranges[hunk_index];
                let display_idx = hunk_index + 1;

                // Build the navigation suffix exactly as git does (order-sensitive).
                let mut nav = String::new();
                let allow_prev_undecided = undecided_previous.is_some();
                let allow_prev = n_hunks > 1;
                let allow_next_undecided = undecided_next.is_some();
                let allow_next = n_hunks > 1;
                let allow_goto = n_hunks > 1;
                if allow_prev_undecided {
                    nav.push_str(",k");
                }
                if allow_prev {
                    nav.push_str(",K");
                }
                if allow_next_undecided {
                    nav.push_str(",j");
                }
                if allow_next {
                    nav.push_str(",J");
                }
                if allow_goto {
                    nav.push_str(",g,/");
                }
                let splittable = !is_deletion && !is_mode_hunk && splittable_into(&ops, s, e) > 1;
                if splittable {
                    nav.push_str(",s");
                }
                let allow_edit = !is_deletion && !is_mode_hunk;
                if allow_edit {
                    nav.push_str(",e");
                }
                nav.push_str(",p,P");

                let kind = if is_deletion {
                    HunkKind::Deletion
                } else if is_addition {
                    HunkKind::Addition
                } else if is_mode_hunk {
                    HunkKind::ModeChange
                } else {
                    HunkKind::Hunk
                };
                let verb = match kind {
                    HunkKind::ModeChange => "Stage mode change",
                    HunkKind::Deletion => "Stage deletion",
                    HunkKind::Addition => "Stage addition",
                    HunkKind::Hunk => "Stage this hunk",
                };
                let was = match decisions[hunk_index] {
                    Decision::Use => " (was: y)",
                    Decision::Skip => " (was: n)",
                    Decision::Undecided => "",
                };
                let prompt_text =
                    format!("({display_idx}/{n_hunks}) {verb}{was} [y,n,q,a,d{nav},?]? ");
                if cctx.use_interactive && !cctx.prompt.is_empty() {
                    write!(out, "{}", cctx.wrap(&cctx.prompt, &prompt_text)).ok();
                } else {
                    write!(out, "{prompt_text}").ok();
                }
                out.flush().ok();

                // `soft_increment`: after y/n/e move to the next undecided hunk, or off the end.
                let soft_increment = |dec_next: Option<usize>| dec_next.unwrap_or(n_hunks);

                let answer = match read_answer(&mut reader)? {
                    None => {
                        // EOF: git prints a trailing newline and applies decided hunks so far.
                        writeln!(out).ok();
                        if mode_hunk && decisions[0] == Decision::Use {
                            effective_mode = mode_from_metadata(&meta);
                        }
                        let accepted = decisions_to_accepted(&decisions);
                        let blended = blend_for_stage_hunks(
                            &index_side_bytes,
                            &cur_work,
                            &hunk_ranges,
                            &accepted,
                        );
                        write_index_blob_and_mode(
                            odb,
                            &mut index,
                            &path_str,
                            &abs_path,
                            blended.as_bytes(),
                            effective_mode,
                        )?;
                        repo.write_index_at(&index_path, &mut index)?;
                        return Ok(());
                    }
                    Some(a) => a,
                };

                let Some(first) = answer.chars().next() else {
                    continue 'hunk_loop;
                };
                let lower = first.to_ascii_lowercase();

                // 'g' takes a hunk number and '/' takes a regexp, so they may be multi-char.
                if answer.chars().count() != 1 && lower != 'g' && first != '/' {
                    writeln!(out, "Only one letter is expected, got '{answer}'").ok();
                    continue 'hunk_loop;
                }

                match lower {
                    'y' => {
                        decisions[hunk_index] = Decision::Use;
                        hunk_index = soft_increment(undecided_next);
                    }
                    'n' => {
                        decisions[hunk_index] = Decision::Skip;
                        hunk_index = soft_increment(undecided_next);
                    }
                    'a' => {
                        for d in decisions.iter_mut().skip(hunk_index) {
                            if *d == Decision::Undecided {
                                *d = Decision::Use;
                            }
                        }
                        hunk_index = first_undecided(&decisions).unwrap_or(0);
                    }
                    'd' => {
                        for d in decisions.iter_mut().skip(hunk_index) {
                            if *d == Decision::Undecided {
                                *d = Decision::Skip;
                            }
                        }
                        hunk_index = first_undecided(&decisions).unwrap_or(0);
                    }
                    'q' => {
                        // Git: `q` sets `patch_update_resp = file_diff_nr` and breaks, then
                        // `putchar('\n')` and applies the decided hunks for this file before
                        // stopping all further files.
                        writeln!(out).ok();
                        if mode_hunk && decisions[0] == Decision::Use {
                            effective_mode = mode_from_metadata(&meta);
                        }
                        let accepted = decisions_to_accepted(&decisions);
                        let blended = blend_for_stage_hunks(
                            &index_side_bytes,
                            &cur_work,
                            &hunk_ranges,
                            &accepted,
                        );
                        write_index_blob_and_mode(
                            odb,
                            &mut index,
                            &path_str,
                            &abs_path,
                            blended.as_bytes(),
                            effective_mode,
                        )?;
                        repo.write_index_at(&index_path, &mut index)?;
                        return Ok(());
                    }
                    _ if first == 'K' => {
                        if allow_prev {
                            hunk_index = dec_mod(hunk_index, n_hunks);
                        } else {
                            writeln!(out, "No other hunk").ok();
                        }
                    }
                    _ if first == 'J' => {
                        if allow_next {
                            hunk_index += 1;
                        } else {
                            writeln!(out, "No other hunk").ok();
                        }
                    }
                    _ if first == 'k' => {
                        if let Some(p) = undecided_previous {
                            hunk_index = p;
                        } else {
                            writeln!(out, "No other undecided hunk").ok();
                        }
                    }
                    _ if first == 'j' => {
                        if let Some(n) = undecided_next {
                            hunk_index = n;
                        } else {
                            writeln!(out, "No other undecided hunk").ok();
                        }
                    }
                    'g' => {
                        if !allow_goto {
                            writeln!(out, "No other hunks to goto").ok();
                            continue 'hunk_loop;
                        }
                        // Strip the leading 'g' and trim.
                        let mut arg: String = answer
                            .chars()
                            .skip(1)
                            .collect::<String>()
                            .trim()
                            .to_string();
                        // Show the hunk list until the user provides a target.
                        let mut start = hunk_index as isize - (DISPLAY_HUNKS_LINES as isize) / 2;
                        if start < 0 {
                            start = 0;
                        }
                        let mut start = start as usize;
                        while arg.is_empty() {
                            let end = display_hunk_list(
                                &mut out,
                                &hunk_ranges,
                                &decisions,
                                &cur_work,
                                start,
                                &render_hunk,
                            );
                            if end < n_hunks {
                                write!(out, "go to which hunk (<ret> to see more)? ").ok();
                            } else {
                                write!(out, "go to which hunk? ").ok();
                            }
                            out.flush().ok();
                            start = end;
                            match read_answer(&mut reader)? {
                                None => {
                                    writeln!(out).ok();
                                    let accepted = decisions_to_accepted(&decisions);
                                    let blended = blend_for_stage_hunks(
                                        &index_side_bytes,
                                        &cur_work,
                                        &hunk_ranges,
                                        &accepted,
                                    );
                                    write_index_blob_and_mode(
                                        odb,
                                        &mut index,
                                        &path_str,
                                        &abs_path,
                                        blended.as_bytes(),
                                        effective_mode,
                                    )?;
                                    repo.write_index_at(&index_path, &mut index)?;
                                    return Ok(());
                                }
                                Some(a) => arg = a.trim().to_string(),
                            }
                        }
                        match arg.parse::<usize>() {
                            Ok(n) if n >= 1 && n <= n_hunks => hunk_index = n - 1,
                            Ok(_) => {
                                if n_hunks == 1 {
                                    writeln!(out, "Sorry, only 1 hunk available.").ok();
                                } else {
                                    writeln!(out, "Sorry, only {n_hunks} hunks available.").ok();
                                }
                            }
                            Err(_) => {
                                writeln!(out, "Invalid number: '{arg}'").ok();
                            }
                        }
                    }
                    _ if first == '/' => {
                        if !allow_goto {
                            writeln!(out, "No other hunks to search").ok();
                            continue 'hunk_loop;
                        }
                        let mut pat: String = answer.chars().skip(1).collect::<String>();
                        pat = pat.trim_end_matches(['\n', '\r']).to_string();
                        if pat.is_empty() {
                            write!(out, "search for regex? ").ok();
                            out.flush().ok();
                            match read_answer(&mut reader)? {
                                None => {
                                    writeln!(out).ok();
                                    let accepted = decisions_to_accepted(&decisions);
                                    let blended = blend_for_stage_hunks(
                                        &index_side_bytes,
                                        &cur_work,
                                        &hunk_ranges,
                                        &accepted,
                                    );
                                    write_index_blob_and_mode(
                                        odb,
                                        &mut index,
                                        &path_str,
                                        &abs_path,
                                        blended.as_bytes(),
                                        effective_mode,
                                    )?;
                                    repo.write_index_at(&index_path, &mut index)?;
                                    return Ok(());
                                }
                                Some(a) => pat = a.trim_end_matches(['\n', '\r']).to_string(),
                            }
                            if pat.is_empty() {
                                continue 'hunk_loop;
                            }
                        }
                        match regex::Regex::new(&pat) {
                            Ok(re) => {
                                let mut i = hunk_index;
                                let mut found = false;
                                loop {
                                    let text = render_hunk(i, &hunk_ranges, &cur_work);
                                    if re.is_match(&text) {
                                        found = true;
                                        break;
                                    }
                                    i = (i + 1) % n_hunks;
                                    if i == hunk_index {
                                        break;
                                    }
                                }
                                if found {
                                    hunk_index = i;
                                } else {
                                    writeln!(out, "No hunk matches the given pattern").ok();
                                }
                            }
                            Err(_) => {
                                writeln!(out, "Malformed search regexp {pat}").ok();
                            }
                        }
                    }
                    's' => {
                        if !splittable {
                            writeln!(out, "Sorry, cannot split this hunk").ok();
                        } else {
                            let before = hunk_ranges.len();
                            if split_hunk_into_all(&mut hunk_ranges, hunk_index, &ops) {
                                let added = hunk_ranges.len() - before;
                                // The new sub-hunks (and the original slot) are all undecided.
                                for _ in 0..added {
                                    decisions.insert(hunk_index + 1, Decision::Undecided);
                                }
                                decisions[hunk_index] = Decision::Undecided;
                                writeln!(out, "Split into {} hunks.", added + 1).ok();
                                rendered_hunk_index = -1;
                            }
                        }
                    }
                    'e' => {
                        if !allow_edit {
                            writeln!(out, "Sorry, cannot edit this hunk").ok();
                        } else {
                            match edit_hunk_and_apply(
                                &repo.git_dir,
                                repo.work_tree.as_deref(),
                                &add_cfg.config,
                                &mut out,
                                path_str.as_str(),
                                &index_side_bytes,
                                &cur_work,
                                &ops[s..e],
                                context,
                            ) {
                                Ok(EditResult::Unchanged) => {
                                    // No-op edit: stage the hunk as-is (git: `hunk->use = USE_HUNK`).
                                    decisions[hunk_index] = Decision::Use;
                                    hunk_index = soft_increment(undecided_next);
                                }
                                Ok(EditResult::Edited(new_work)) => {
                                    cur_work = new_work;
                                    decisions[hunk_index] = Decision::Use;
                                    hunk_index = soft_increment(undecided_next);
                                }
                                Ok(EditResult::Aborted) | Err(_) => {}
                            }
                        }
                    }
                    'p' => {
                        // p/P just re-render the current hunk.
                        rendered_hunk_index = -1;
                    }
                    '?' => {
                        let summary = if all_decided {
                            let used = decisions.iter().filter(|d| **d == Decision::Use).count();
                            let skipped =
                                decisions.iter().filter(|d| **d == Decision::Skip).count();
                            Some((decisions.len(), used, skipped))
                        } else {
                            None
                        };
                        write_patch_help(&mut out, &nav, summary);
                    }
                    ' ' => {}
                    _ => {
                        writeln!(out, "Unknown command '{answer}' (use '?' for help)").ok();
                    }
                }
            }

            // Git prints a trailing newline when leaving `patch_update_file`.
            writeln!(out).ok();

            // Stage the mode change iff the mode pseudo-hunk (index 0) was accepted.
            if mode_hunk && decisions[0] == Decision::Use {
                effective_mode = mode_from_metadata(&meta);
            }
            let accepted = decisions_to_accepted(&decisions);
            let blended =
                blend_for_stage_hunks(&index_side_bytes, &cur_work, &hunk_ranges, &accepted);

            if accepted.iter().any(|&a| a) || (mode_differs && effective_mode != ie.mode) {
                write_index_blob_and_mode(
                    odb,
                    &mut index,
                    &path_str,
                    &abs_path,
                    blended.as_bytes(),
                    effective_mode,
                )?;
            }
            break 'rediff;
        }
    }

    // Mirror Git: if every candidate file was binary (and thus skipped), say so.
    if total_entries > 0 && binary_count == total_entries {
        println!("Only binary files changed.");
    }

    repo.write_index_at(&index_path, &mut index)
        .context("writing index")?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadCmd {
    Eof,
    Invalid,
    /// A single-character command. `lower` is folded for matching; `raw` keeps the original case
    /// for the "Unknown command '<x>'" diagnostic.
    Char {
        lower: char,
        raw: char,
    },
}

impl ReadCmd {
    fn ch(lower: char, raw: char) -> Self {
        ReadCmd::Char { lower, raw }
    }
}

fn read_one_command(reader: &mut impl BufRead, out: &mut impl Write) -> Result<ReadCmd> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(ReadCmd::Eof);
    }
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let t = trimmed.trim();
    if t.is_empty() {
        return Ok(ReadCmd::ch(' ', ' '));
    }
    if t.chars().count() > 1 {
        // Git: `err(s, _("Only one letter is expected, got '%s'"), ...)`.
        writeln!(out, "Only one letter is expected, got '{t}'")?;
        return Ok(ReadCmd::Invalid);
    }
    let c = t.chars().next().unwrap_or(' ');
    Ok(ReadCmd::ch(c.to_ascii_lowercase(), c))
}

fn parse_mode_u32(m: &str) -> u32 {
    u32::from_str_radix(m, 8).unwrap_or(0)
}

fn handle_deleted_file(
    repo: &Repository,
    index: &mut Index,
    index_path: &Path,
    path_str: &str,
    ie: &IndexEntry,
    reader: &mut impl BufRead,
    out: &mut impl Write,
    odb: &Odb,
) -> Result<()> {
    let index_blob = if ie.oid == ObjectId::zero() {
        Vec::new()
    } else {
        let obj = odb.read(&ie.oid)?;
        if obj.kind != ObjectKind::Blob {
            return Ok(());
        }
        obj.data
    };
    if is_binary(&index_blob) {
        return Ok(());
    }

    let work_blob = Vec::<u8>::new();
    let index_str = String::from_utf8_lossy(&index_blob);
    let work_str = String::from_utf8_lossy(&work_blob);
    let text_diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_lines(index_str.as_ref(), work_str.as_ref());
    let ops: Vec<_> = text_diff.ops().to_vec();
    let n_ops = ops.len();
    let mut hunk_ranges = vec![(0, n_ops)];
    let mut accepted = vec![false; 1];
    let mut hunk_cursor = 0usize;

    loop {
        if hunk_cursor >= hunk_ranges.len() {
            break;
        }
        let display_idx = hunk_cursor + 1;
        let n_hunks = hunk_ranges.len();
        let (s, e) = hunk_ranges[hunk_cursor];
        let hunk_only =
            partial_unified_for_op_range(path_str, &index_blob, &work_blob, &ops[s..e], 3, true);
        // Deletion file header: `deleted file mode`, `index <old>..0000000`, `+++ /dev/null`.
        let short = short_oid_of(odb, &index_blob);
        writeln!(out, "diff --git a/{path_str} b/{path_str}").ok();
        writeln!(out, "deleted file mode {:06o}", ie.mode).ok();
        writeln!(out, "index {short}..0000000").ok();
        write!(out, "--- a/{path_str}\n+++ /dev/null\n").ok();
        write!(out, "{hunk_only}").ok();
        // Deletions never offer edit (`e`); split (`s`) only when the hunk is actually splittable.
        let splittable = splittable_into(&ops, s, e) > 1;
        let suffix = prompt_suffix(n_hunks, splittable, true);
        write!(
            out,
            "({display_idx}/{n_hunks}) Stage deletion [y,n,q,a,d{suffix},?]? "
        )
        .ok();
        out.flush().ok();

        match read_one_command(reader, out)? {
            ReadCmd::Eof => {
                repo.write_index_at(index_path, index)?;
                return Ok(());
            }
            ReadCmd::Invalid => continue,
            ReadCmd::Char { lower, .. } => match lower {
                'y' => {
                    accepted[hunk_cursor] = true;
                    hunk_cursor += 1;
                }
                'n' => {
                    hunk_cursor += 1;
                }
                'a' => {
                    for j in hunk_cursor..n_hunks {
                        accepted[j] = true;
                    }
                    break;
                }
                'd' => break,
                'q' => {
                    repo.write_index_at(index_path, index)?;
                    return Ok(());
                }
                's' => {
                    if !split_hunk_at_first_gap(&mut hunk_ranges, hunk_cursor, &ops) {
                        writeln!(out, "Sorry, cannot split this hunk").ok();
                        continue;
                    }
                    let n = hunk_ranges.len();
                    accepted.resize(n, false);
                }
                '?' => {
                    writeln!(
                        out,
                        "y - stage this hunk for deletion\n\
                         n - do not stage this hunk\n\
                         q - quit\n\
                         a - stage this and all later hunks\n\
                         d - skip remaining hunks in this file\n\
                         s - split hunk\n"
                    )
                    .ok();
                }
                _ => {}
            },
        }
    }

    if accepted.iter().any(|&a| a) {
        let blended = blend_for_stage_hunks(&index_blob, &work_blob, &hunk_ranges, &accepted);
        if blended.is_empty() {
            index.remove(path_str.as_bytes());
        } else {
            let oid = odb.write(ObjectKind::Blob, blended.as_bytes())?;
            if let Some(ent) = index.get_mut(path_str.as_bytes(), 0) {
                ent.oid = oid;
                ent.size = blended.len() as u32;
            }
        }
    }
    Ok(())
}

fn write_index_blob_and_mode(
    odb: &Odb,
    index: &mut Index,
    path_str: &str,
    abs_path: &Path,
    blob_data: &[u8],
    mode: u32,
) -> Result<()> {
    let oid = odb.write(ObjectKind::Blob, blob_data)?;
    let meta = fs::symlink_metadata(abs_path).ok();
    // Whether the staged blob equals the current worktree bytes. When a partial hunk (or an edited
    // hunk) stages content that differs from the worktree, the index entry's stat must NOT claim to
    // match the worktree — otherwise `git diff` (diff-files) takes the stat fast-path and reports
    // the path clean even though the staged blob differs (t3701 "real edit works").
    let worktree_bytes = fs::read(abs_path).ok();
    let blob_matches_worktree = worktree_bytes.as_deref() == Some(blob_data);
    let mut new_ent = if let Some(m) = meta.as_ref() {
        let mut e = entry_from_metadata(m, path_str.as_bytes(), oid, mode);
        e.mode = mode;
        if !blob_matches_worktree {
            // Record the blob's true size and drop the worktree mtime so diff-files re-hashes and
            // sees the difference (Git leaves such entries stat-dirty).
            e.size = blob_data.len() as u32;
            e.mtime_sec = 0;
            e.mtime_nsec = 0;
            e.ctime_sec = 0;
            e.ctime_nsec = 0;
        }
        e
    } else {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: blob_data.len() as u32,
            oid,
            flags: path_str.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path_str.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    };
    new_ent.set_intent_to_add(false);
    new_ent.set_assume_unchanged(false);
    new_ent.set_skip_worktree(false);
    index.stage_file(new_ent);
    Ok(())
}

fn path_under_sparse_index_dir(index: &Index, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    index
        .entries
        .iter()
        .filter(|entry| entry.stage() == 0 && entry.mode == MODE_TREE)
        .filter_map(|entry| std::str::from_utf8(&entry.path).ok())
        .map(|prefix| prefix.trim_end_matches('/'))
        .any(|prefix| {
            let prefix_slash = format!("{prefix}/");
            path == prefix || path.starts_with(&prefix_slash)
        })
}

fn emit_index_trace_region(label: &str) {
    if let Ok(trace2_event) = std::env::var("GIT_TRACE2_EVENT") {
        if !trace2_event.trim().is_empty() {
            let _ = crate::trace2_region_json(&trace2_event, "index", label);
        }
    }
}

/// Open `content` in the user's editor, returning the edited bytes.
///
/// Editor resolution goes through [`crate::editor::resolve_commit_launch_editor`], matching git's
/// `git_editor()` (`GIT_EDITOR` → `core.editor` → `VISUAL` (only when not a dumb terminal) →
/// `EDITOR`). Crucially this treats the harness placeholders `VISUAL=:` / a bare `EDITOR=:` the way
/// git does, so `test_set_editor`'s `EDITOR='"$FAKE_EDITOR"'` wins (t3701 edit tests).
///
/// The edit file lives at `$GIT_DIR/addp-hunk-edit.diff` (matching git's
/// `strbuf_edit_interactively(..., "addp-hunk-edit.diff", ...)`). Putting it inside the repository
/// — rather than `$TMPDIR` — is load-bearing: some editors (and t3701's fake editor, which does a
/// relative `mv -f patch "$1"`) assume the edit file shares the working directory's filesystem.
fn run_editor_on_text(
    git_dir: &Path,
    work_tree: Option<&Path>,
    config: &ConfigSet,
    content: &[u8],
) -> Result<Vec<u8>> {
    use std::io::Write;
    let path = git_dir.join("addp-hunk-edit.diff");
    {
        let mut f = fs::File::create(&path).context("creating add -p edit file")?;
        f.write_all(content)?;
        f.flush()?;
    }
    let mut editor = crate::editor::resolve_commit_launch_editor(config)
        .ok_or_else(|| anyhow::anyhow!("Terminal is dumb, but EDITOR unset"))?;
    // Mirror `launch_commit_editor`: an explicit `EDITOR=...` still wins over a `:` placeholder.
    if editor.trim() == ":" {
        if let Ok(env_editor) = std::env::var("EDITOR") {
            if !env_editor.trim().is_empty() && env_editor.trim() != ":" {
                editor = env_editor;
            }
        }
    }
    // Git treats `:` as a no-op editor (`launch_specified_editor`): leave the file untouched.
    if editor.trim() == ":" {
        let edited = fs::read(&path).context("reading edited file")?;
        let _ = fs::remove_file(&path);
        return Ok(edited);
    }
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path);
    // Run the editor from the work tree so scripts using relative paths (t3701's fake editor does a
    // relative `mv -f patch "$1"`) see the same cwd as `git add -p`.
    if let Some(wt) = work_tree {
        cmd.current_dir(wt);
    } else {
        cmd.current_dir(git_dir);
    }
    let status = cmd.status().context("running editor")?;
    if !status.success() {
        let _ = fs::remove_file(&path);
        bail!("editor failed");
    }
    let edited = fs::read(&path).context("reading edited file")?;
    let _ = fs::remove_file(&path);
    Ok(edited)
}

/// Compute the inclusive index(old)-side line span `[old_start, old_end)` covered by `op_slice`.
fn index_span(op_slice: &[similar::DiffOp]) -> (usize, usize) {
    let mut start = usize::MAX;
    let mut end = 0usize;
    for op in op_slice {
        let (s, e) = match *op {
            similar::DiffOp::Equal { old_index, len, .. } => (old_index, old_index + len),
            similar::DiffOp::Delete {
                old_index, old_len, ..
            } => (old_index, old_index + old_len),
            similar::DiffOp::Insert { old_index, .. } => (old_index, old_index),
            similar::DiffOp::Replace {
                old_index, old_len, ..
            } => (old_index, old_index + old_len),
        };
        start = start.min(s);
        end = end.max(e);
    }
    if start == usize::MAX {
        (0, 0)
    } else {
        (start, end)
    }
}

/// Manually edit the current hunk (the `e` command), mirroring `edit_hunk_manually` +
/// `recount_edited_hunk` + apply-check in `add-patch.c`.
///
/// Renders the hunk body with a commented quick-guide, runs the editor, strips comment lines,
/// then applies the edited hunk to the index-side content at this hunk's location to produce the
/// new full worktree content. If the edited hunk's context/removed lines do not match the index
/// content, prints `error: patch failed` / `hunk does not apply` (matching `git apply`) and
/// returns `Ok(None)`.
///
/// # Returns
/// - `Ok(Some(new_work))` — the new worktree-side content after applying the edited hunk.
/// - `Ok(None)` — the edit was abandoned/empty or did not apply (hunk left unchanged).
///
/// # Errors
/// Propagates editor/IO failures.
/// Outcome of a manual hunk edit (`e`).
enum EditResult {
    /// The editor left the hunk unchanged: stage it as-is (`hunk->use = USE_HUNK`, no rediff).
    Unchanged,
    /// The hunk was edited: `cur_work` is replaced with the new full-file content.
    Edited(Vec<u8>),
    /// The edit was abandoned (all lines removed, or the patch did not apply): leave undecided.
    Aborted,
}

fn edit_hunk_and_apply(
    git_dir: &Path,
    work_tree: Option<&Path>,
    config: &ConfigSet,
    out: &mut impl Write,
    path: &str,
    index_bytes: &[u8],
    work_bytes: &[u8],
    op_slice: &[similar::DiffOp],
    context: usize,
) -> Result<EditResult> {
    // The body to present is the hunk text (header + ` `/`+`/`-` lines), as displayed.
    let hunk_text =
        partial_unified_for_op_range(path, index_bytes, work_bytes, op_slice, context, true);

    // Comment guide, matching add-patch.c. Comment char defaults to '#'.
    let mut buf = String::new();
    buf.push_str("# Manual hunk edit mode -- see bottom for a quick guide.\n");
    buf.push_str(&hunk_text);
    buf.push_str("# ---\n");
    buf.push_str("# To remove '-' lines, make them ' ' lines (context).\n");
    buf.push_str("# To remove '+' lines, delete them.\n");
    buf.push_str("# Lines starting with # will be removed.\n");
    buf.push_str(
        "# If it does not apply cleanly, you will be given an opportunity to\n\
         # edit again.  If all lines of the hunk are removed, then the edit is\n\
         # aborted and the hunk is left unchanged.\n",
    );

    let edited = run_editor_on_text(git_dir, work_tree, config, buf.as_bytes())?;
    let edited = String::from_utf8_lossy(&edited).into_owned();

    // If the editor left the presented buffer unchanged (e.g. EDITOR=: / touch), git stages the
    // hunk as-is without re-diffing the file.
    if edited == buf {
        return Ok(EditResult::Unchanged);
    }

    // Strip comment lines.
    let body: Vec<&str> = edited.lines().filter(|l| !l.starts_with('#')).collect();

    // Mirror git's `recount_edited_hunk` + reassemble: if a `@@` header is present git re-parses it
    // (and skips it) and anchors the body there; otherwise (e.g. a totally garbled patch) it keeps
    // the original header and still treats every body line. Lines starting with ` `/`+`/`-` are
    // context/del/add; a `\` line is the no-newline marker; any other line is treated as a context
    // line whose whole text must match the index — so a garbage patch with no real context (t3701
    // "garbage edit rejected") fails to locate and prints "patch does not apply".
    let has_header = body.iter().any(|l| l.starts_with("@@"));
    let mut old_lines: Vec<String> = Vec::new();
    let mut new_lines: Vec<String> = Vec::new();
    let mut saw_body = !has_header;
    let mut any_body = false;
    for line in &body {
        if line.starts_with("@@") {
            saw_body = true;
            continue;
        }
        if !saw_body {
            // Lines before the header are ignored (git anchors the body at the header).
            continue;
        }
        any_body = true;
        if line.starts_with('\\') {
            continue; // "\ No newline at end of file"
        }
        let (marker, rest) = match line.chars().next() {
            Some(c @ (' ' | '+' | '-')) => (c, &line[1..]),
            // A line with no recognized marker is treated as a context line (git strips one leading
            // space when present; a bare line keeps its full text).
            _ => (' ', *line),
        };
        match marker {
            ' ' => {
                old_lines.push(rest.to_string());
                new_lines.push(rest.to_string());
            }
            '-' => old_lines.push(rest.to_string()),
            '+' => new_lines.push(rest.to_string()),
            _ => {}
        }
    }

    if !any_body {
        // All lines removed (or nothing but a header): abandon the edit, leave the hunk unchanged.
        return Ok(EditResult::Aborted);
    }

    // Apply positionally, like `git apply`: locate where the edited hunk's old side
    // (context + removed lines) matches a contiguous run of the index content, preferring the
    // original hunk position, then splice the new side (context + added) in its place.
    let (orig_old_start, _orig_old_end) = index_span(op_slice);
    let index_str = String::from_utf8_lossy(index_bytes);
    let index_lines: Vec<&str> = index_str.lines().collect();

    // An old side that does not match the index content makes the reassembled patch fail
    // `git apply --check`. Git then prints the apply error and offers to edit again. We emit the
    // same diagnostics; the caller's `e n d` path consumes the `n` answer.
    let match_at = locate_hunk(&index_lines, &old_lines, orig_old_start);
    let Some(pos) = match_at else {
        writeln!(out, "error: patch failed: {path}:{}", orig_old_start + 1).ok();
        writeln!(out, "error: {path}: patch does not apply").ok();
        writeln!(
            out,
            "Your edited hunk does not apply. Edit again (saying \"no\" discards!) [y/n]? "
        )
        .ok();
        return Ok(EditResult::Aborted);
    };

    let trailing_newline = work_bytes.ends_with(b"\n") || index_bytes.ends_with(b"\n");
    let mut result_lines: Vec<String> = Vec::new();
    result_lines.extend(index_lines[..pos].iter().map(|s| s.to_string()));
    result_lines.extend(new_lines.iter().cloned());
    result_lines.extend(
        index_lines[(pos + old_lines.len()).min(index_lines.len())..]
            .iter()
            .map(|s| s.to_string()),
    );

    let mut new_content = result_lines.join("\n");
    if trailing_newline && !new_content.is_empty() {
        new_content.push('\n');
    }
    Ok(EditResult::Edited(new_content.into_bytes()))
}

/// Find the line index in `haystack` where `needle` matches contiguously, preferring `hint` then
/// scanning outward (the position-then-fuzz search `git apply` performs). Returns `None` if no
/// match exists. An empty `needle` (pure insertion) matches at `hint` (clamped).
fn locate_hunk(haystack: &[&str], needle: &[String], hint: usize) -> Option<usize> {
    let n = needle.len();
    if n == 0 {
        return Some(hint.min(haystack.len()));
    }
    if n > haystack.len() {
        return None;
    }
    let matches_at = |p: usize| {
        haystack[p..p + n]
            .iter()
            .zip(needle)
            .all(|(a, b)| *a == b.as_str())
    };
    let last = haystack.len() - n;
    let start = hint.min(last);
    // Search forward then backward from the hint.
    for p in start..=last {
        if matches_at(p) {
            return Some(p);
        }
    }
    for p in (0..start).rev() {
        if matches_at(p) {
            return Some(p);
        }
    }
    None
}
