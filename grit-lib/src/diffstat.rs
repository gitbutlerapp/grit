//! Git-compatible `--stat` / diffstat layout (width, name truncation, bar scaling).
//!
//! Matches the width algorithm in Git's `show_stats()` (`diff.c`).

use std::io::{Result as IoResult, Write};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Visible terminal width of `s`, skipping ANSI CSI sequences (like Git `utf8_strnwidth(..., 1)`).
#[must_use]
pub fn display_width_minus_ansi(s: &str) -> usize {
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        w = w.saturating_add(UnicodeWidthChar::width(ch).unwrap_or(0));
    }
    w
}

/// `term_columns()` approximation: `COLUMNS` env, then `stty size`, then 80.
#[must_use]
pub fn terminal_columns() -> usize {
    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(w) = cols.parse::<usize>() {
            if w > 0 {
                return w;
            }
        }
    }
    // The terminal size is constant for the life of the process (matching
    // C git, which caches `term_columns()` after the first call); spawning
    // `stty` once per --stat commit dominated history walks. The `COLUMNS`
    // check above stays uncached so per-call env overrides keep working.
    // hygiene: immutable terminal width cache (process tty geometry, set once)
    static STTY_COLS: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    if let Some(w) = *STTY_COLS.get_or_init(|| {
        let output = std::process::Command::new("stty")
            .arg("size")
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() == 2 {
            if let Ok(w) = parts[1].parse::<usize>() {
                if w > 0 {
                    return Some(w);
                }
            }
        }
        None
    }) {
        return w;
    }
    80
}

/// Default total width for `format-patch` diffstat (`MAIL_DEFAULT_WRAP` in Git).
pub const FORMAT_PATCH_STAT_WIDTH: usize = 72;

#[derive(Debug, Clone)]
pub struct FileStatInput {
    pub path_display: String,
    pub insertions: usize,
    pub deletions: usize,
    pub is_binary: bool,
    /// Unmerged (conflicted) path: rendered as ` name | Unmerged` and excluded
    /// from the "N files changed" count (git `diffstat_file.is_unmerged`).
    pub is_unmerged: bool,
}

/// Options for laying out diffstat lines (Git `diff_options` stat fields).
#[derive(Debug, Clone)]
pub struct DiffstatOptions<'a> {
    /// Total display width for the stat block (after subtracting `line_prefix` when using terminal width).
    pub total_width: usize,
    /// Prefix printed before each stat line (graph + color); only affects width budget when
    /// `subtract_prefix_from_terminal` is true and `width_prefix` is empty.
    pub line_prefix: &'a str,
    /// Prefix whose display width is subtracted from the terminal columns for the width budget,
    /// but which is *not* itself printed (the caller emits it separately, e.g. `log --graph`'s
    /// per-line rail). When empty, `line_prefix` is used for the subtraction instead. Matches
    /// Git's `width = term_columns() - utf8_strnwidth(line_prefix)` where the graph's vertical
    /// rail is the `output_prefix`.
    pub width_prefix: &'a str,
    /// When true, width budget is [`Self::terminal_width`] minus
    /// `display_width_minus_ansi(<prefix>)` (callers set [`Self::terminal_width`] from the
    /// terminal size or `COLUMNS`, typically in `grit-cli`).
    pub subtract_prefix_from_terminal: bool,
    /// Terminal width in columns when [`Self::subtract_prefix_from_terminal`] is true.
    pub terminal_width: usize,
    /// Cap filename area (`diff.statNameWidth` / `--stat-name-width`).
    pub stat_name_width: Option<usize>,
    /// Cap graph (+/-) area (`diff.statGraphWidth` / `--stat-graph-width`).
    pub stat_graph_width: Option<usize>,
    /// Max files to show; extra files omitted with a `...` line.
    pub stat_count: Option<usize>,
    /// ANSI SGR before `+` run (empty = no color).
    pub color_add: &'a str,
    /// ANSI SGR before `-` run (empty = no color).
    pub color_del: &'a str,
    /// ANSI reset after colored bar segments (typically `\x1b[m`).
    pub color_reset: &'a str,
    /// Extra columns allocated to the +/- bar (Git `log --graph --stat` uses one more than plain diffstat).
    pub graph_bar_slack: usize,
    /// When subtracting `line_prefix` from `COLUMNS`, add this many columns back (colored graph `|`).
    pub graph_prefix_budget_slack: usize,
}

fn scale_linear(it: usize, width: usize, max_change: usize) -> usize {
    if it == 0 || max_change == 0 {
        return 0;
    }
    if width <= 1 {
        return if it > 0 { 1 } else { 0 };
    }
    1 + (it * (width - 1) / max_change)
}

fn decimal_width(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        format!("{n}").len()
    }
}

/// Truncate a path to fit `area_width` display columns (Git `show_stats` name scaling).
/// Pad `s` with trailing ASCII spaces so its display width is at least `min_cols`.
///
/// Git's diffstat uses display-column width for the name field (`utf8_strnwidth`-style), not
/// Rust's `{:<n$}` padding which counts Unicode scalar values.
fn pad_name_to_display_width(s: &str, min_cols: usize) -> String {
    let w = s.width();
    if w >= min_cols {
        return s.to_string();
    }
    let pad = min_cols - w;
    let mut out = String::with_capacity(s.len() + pad);
    out.push_str(s);
    out.push_str(&" ".repeat(pad));
    out
}

fn truncate_path_for_name_area(path: &str, area_width: usize) -> (String, usize) {
    let full_w = path.width();
    if full_w <= area_width {
        return (path.to_string(), full_w);
    }
    let mut len = area_width;
    len = len.saturating_sub(3);
    let mut byte_start = 0usize;
    let mut name_w = full_w;
    while name_w > len {
        let ch = path[byte_start..].chars().next().unwrap_or('\u{fffd}');
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        name_w = name_w.saturating_sub(cw);
        byte_start += ch.len_utf8();
    }
    let rest = &path[byte_start..];
    if let Some(slash_idx) = rest.find('/') {
        let after = &rest[slash_idx..];
        let after_w = after.width();
        if after_w <= area_width {
            return (format!("...{}", after), after_w);
        }
    }
    let s = format!("...{}", rest);
    (s.clone(), s.width())
}

/// Write diffstat lines and summary, matching Git's layout.
pub fn write_diffstat_block(
    out: &mut impl Write,
    files: &[FileStatInput],
    opts: &DiffstatOptions<'_>,
) -> IoResult<()> {
    if files.is_empty() {
        return Ok(());
    }

    let limit = opts.stat_count.unwrap_or(files.len()).min(files.len());
    let shown = &files[..limit];

    let mut max_len = 0usize;
    let mut max_change = 0usize;
    let mut number_width = 0usize;
    let mut bin_width = 0usize;

    for f in shown {
        let w = f.path_display.width();
        if max_len < w {
            max_len = w;
        }
        if f.is_unmerged {
            // "Unmerged" is 8 characters (git show_stats()).
            if bin_width < 8 {
                bin_width = 8;
            }
            continue;
        }
        if f.is_binary {
            let w = if f.insertions == 0 && f.deletions == 0 {
                3
            } else {
                14 + decimal_width(f.insertions) + decimal_width(f.deletions)
            };
            if bin_width < w {
                bin_width = w;
            }
            number_width = number_width.max(3);
            continue;
        }
        let ch = f.insertions + f.deletions;
        if max_change < ch {
            max_change = ch;
        }
    }

    let width_prefix = if opts.width_prefix.is_empty() {
        opts.line_prefix
    } else {
        opts.width_prefix
    };
    let mut width = if opts.subtract_prefix_from_terminal {
        opts.terminal_width
            .saturating_sub(display_width_minus_ansi(width_prefix))
            .saturating_add(opts.graph_prefix_budget_slack)
    } else {
        opts.total_width
    };

    number_width = number_width.max(decimal_width(max_change));

    if width < 16 + 6 + number_width {
        width = 16 + 6 + number_width;
    }

    let mut graph_width = if max_change + 4 > bin_width {
        max_change
    } else {
        bin_width.saturating_sub(4)
    };
    if let Some(cap) = opts.stat_graph_width {
        if cap > 0 && cap < graph_width {
            graph_width = cap;
        }
    }

    let mut name_width = match opts.stat_name_width {
        Some(nw) if nw > 0 && nw < max_len => nw,
        _ => max_len,
    };

    if name_width + number_width + 6 + graph_width > width {
        let mut gw = graph_width;
        let target_gw = width * 3 / 8;
        if gw > target_gw.saturating_sub(number_width).saturating_sub(6) {
            gw = target_gw.saturating_sub(number_width).saturating_sub(6);
            if gw < 6 {
                gw = 6;
            }
        }
        graph_width = gw;
        if let Some(cap) = opts.stat_graph_width {
            if graph_width > cap {
                graph_width = cap;
            }
        }
        if name_width
            > width
                .saturating_sub(number_width)
                .saturating_sub(6)
                .saturating_sub(graph_width)
        {
            name_width = width
                .saturating_sub(number_width)
                .saturating_sub(6)
                .saturating_sub(graph_width);
        } else {
            graph_width = width
                .saturating_sub(number_width)
                .saturating_sub(6)
                .saturating_sub(name_width);
        }
    }

    graph_width = graph_width.saturating_add(opts.graph_bar_slack);

    let mut total_ins = 0usize;
    let mut total_del = 0usize;

    for f in shown {
        let prefix = opts.line_prefix;
        if f.is_unmerged {
            let (display_name, _) = truncate_path_for_name_area(&f.path_display, name_width);
            let name_col = pad_name_to_display_width(&display_name, name_width);
            // git: ` %s%s%*s | %*sUnmerged` — number_width is usually < len("Unmerged"),
            // so the word is printed verbatim with no extra left padding.
            if prefix.is_empty() {
                writeln!(
                    out,
                    " {} | {:>nw$}",
                    name_col,
                    "Unmerged",
                    nw = number_width
                )?;
            } else {
                writeln!(
                    out,
                    "{prefix}{} | {:>nw$}",
                    name_col,
                    "Unmerged",
                    nw = number_width
                )?;
            }
            continue;
        }
        if f.is_binary {
            let (display_name, _) = truncate_path_for_name_area(&f.path_display, name_width);
            let name_col = pad_name_to_display_width(&display_name, name_width);
            if f.insertions == 0 && f.deletions == 0 {
                if prefix.is_empty() {
                    writeln!(out, " {} | {:>nw$}", name_col, "Bin", nw = number_width)?;
                } else {
                    writeln!(
                        out,
                        "{prefix}{} | {:>nw$}",
                        name_col,
                        "Bin",
                        nw = number_width
                    )?;
                }
            } else if prefix.is_empty() {
                writeln!(
                    out,
                    " {} | {:>nw$} {} -> {} bytes",
                    name_col,
                    "Bin",
                    f.deletions,
                    f.insertions,
                    nw = number_width
                )?;
            } else {
                writeln!(
                    out,
                    "{prefix}{} | {:>nw$} {} -> {} bytes",
                    name_col,
                    "Bin",
                    f.deletions,
                    f.insertions,
                    nw = number_width
                )?;
            }
            continue;
        }

        let added = f.insertions;
        let deleted = f.deletions;
        let (display_name, _) = truncate_path_for_name_area(&f.path_display, name_width);
        let name_col = pad_name_to_display_width(&display_name, name_width);

        let mut add = added;
        let mut del = deleted;
        if graph_width <= max_change && max_change > 0 {
            let total_scaled = scale_linear(added + del, graph_width, max_change);
            let mut total = total_scaled;
            if total < 2 && add > 0 && del > 0 {
                total = 2;
            }
            if add < del {
                add = scale_linear(add, graph_width, max_change);
                del = total.saturating_sub(add);
            } else {
                del = scale_linear(del, graph_width, max_change);
                add = total.saturating_sub(del);
            }
        }

        total_ins = total_ins.saturating_add(added);
        total_del = total_del.saturating_add(deleted);

        let total = added + del;
        if prefix.is_empty() {
            write!(out, " {} | {:>nw$}", name_col, total, nw = number_width)?;
        } else {
            write!(
                out,
                "{prefix}{} | {:>nw$}",
                name_col,
                total,
                nw = number_width
            )?;
        }
        if total > 0 {
            write!(out, " ")?;
        }
        if add > 0 {
            if !opts.color_add.is_empty() {
                write!(out, "{}", opts.color_add)?;
            }
            write!(out, "{}", "+".repeat(add))?;
            if !opts.color_add.is_empty() && !opts.color_reset.is_empty() {
                write!(out, "{}", opts.color_reset)?;
            }
        }
        if del > 0 {
            if !opts.color_del.is_empty() {
                write!(out, "{}", opts.color_del)?;
            }
            write!(out, "{}", "-".repeat(del))?;
            if !opts.color_del.is_empty() && !opts.color_reset.is_empty() {
                write!(out, "{}", opts.color_reset)?;
            }
        }
        writeln!(out)?;
    }

    if files.len() > limit {
        if opts.line_prefix.is_empty() {
            writeln!(out, " ...")?;
        } else {
            writeln!(out, "{}...", opts.line_prefix)?;
        }
    }

    // `--stat-count` only truncates the per-file lines; the summary still
    // covers every entry (t4049).
    for f in &files[limit..] {
        if f.is_binary {
            continue;
        }
        total_ins = total_ins.saturating_add(f.insertions);
        total_del = total_del.saturating_add(f.deletions);
    }

    // Unmerged paths are listed but not counted as "changed" (git show_stats()).
    let files_changed = files.iter().filter(|f| !f.is_unmerged).count();
    let mut summary = if opts.line_prefix.is_empty() {
        format!(
            " {} file{} changed",
            files_changed,
            if files_changed == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "{}{} file{} changed",
            opts.line_prefix,
            files_changed,
            if files_changed == 1 { "" } else { "s" }
        )
    };
    // git: when no files changed (e.g. only unmerged paths), the summary is just
    // " 0 files changed" with no insertions/deletions suffix.
    if files_changed > 0 {
        if total_ins > 0 {
            summary.push_str(&format!(
                ", {} insertion{}(+)",
                total_ins,
                if total_ins == 1 { "" } else { "s" }
            ));
        }
        if total_del > 0 {
            summary.push_str(&format!(
                ", {} deletion{}(-)",
                total_del,
                if total_del == 1 { "" } else { "s" }
            ));
        }
        if total_ins == 0 && total_del == 0 {
            summary.push_str(", 0 insertions(+), 0 deletions(-)");
        }
    }
    writeln!(out, "{summary}")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_name_matches_git_display_columns_for_wide_chars() {
        // Truncated path from t4073: display width 9; Git pads to name_width 10 with one space.
        let truncated = ".../f再见";
        assert_eq!(truncated.width(), 9);
        let padded = pad_name_to_display_width(truncated, 10);
        assert_eq!(padded.width(), 10);
        assert_eq!(padded, ".../f再见 ");
    }

    #[test]
    fn diffstat_name_width_10_matches_git_padding() {
        let files = vec![FileStatInput {
            path_display: "d你好/f再见".to_string(),
            insertions: 0,
            deletions: 0,
            is_binary: false,
            is_unmerged: false,
        }];
        let opts = DiffstatOptions {
            total_width: 80,
            line_prefix: "",
            width_prefix: "",
            subtract_prefix_from_terminal: false,
            terminal_width: 80,
            stat_name_width: Some(10),
            stat_graph_width: None,
            stat_count: None,
            color_add: "",
            color_del: "",
            color_reset: "",
            graph_bar_slack: 0,
            graph_prefix_budget_slack: 0,
        };
        let mut buf = Vec::new();
        write_diffstat_block(&mut buf, &files, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let line = s.lines().next().unwrap();
        assert!(
            line.contains(".../f再见  |"),
            "expected two spaces before pipe like git, got {line:?}"
        );
    }
}
