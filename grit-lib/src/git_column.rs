//! Git-compatible column layout for long-format status (untracked / ignored lists).
//!
//! Mirrors the behaviour of upstream `column.c` / `print_columns` used by `wt-status.c`.

use std::io::{self, Write};

use unicode_width::UnicodeWidthStr;

/// Layout mode (lower 4 bits of [`ColOpts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnLayout {
    /// Fill columns before rows (`COL_COLUMN`).
    Column,
    /// Fill rows before columns (`COL_ROW`).
    Row,
    /// One path per line (`COL_PLAIN`).
    Plain,
}

/// Bit flags matching Git's `column.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColOpts(u32);

const LAYOUT_MASK: u32 = 0x000F;
const ENABLE_MASK: u32 = 0x0030;
const PARSEOPT: u32 = 0x0040;
const DENSE: u32 = 0x0080;

const DISABLED: u32 = 0x0000;
const ENABLED: u32 = 0x0010;
const AUTO: u32 = 0x0020;

const LAYOUT_COLUMN: u32 = 0;
const LAYOUT_ROW: u32 = 1;
const LAYOUT_PLAIN: u32 = 15;

impl ColOpts {
    /// Create an empty column option set.
    #[must_use]
    pub const fn new() -> Self {
        Self(0)
    }

    fn layout_bits(self) -> u32 {
        self.0 & LAYOUT_MASK
    }

    /// True when column layout is active (`COL_ENABLED`).
    #[must_use]
    pub fn is_active(self) -> bool {
        self.0 & ENABLE_MASK == ENABLED
    }

    fn dense(self) -> bool {
        self.0 & DENSE != 0
    }

    fn layout_mode(self) -> ColumnLayout {
        match self.layout_bits() {
            LAYOUT_ROW => ColumnLayout::Row,
            LAYOUT_PLAIN => ColumnLayout::Plain,
            _ => ColumnLayout::Column,
        }
    }
}

impl Default for ColOpts {
    fn default() -> Self {
        Self::new()
    }
}

/// Options passed to [`print_columns`], matching `struct column_options`.
#[derive(Debug, Clone)]
pub struct ColumnOptions {
    /// Total width for layout. CLI callers should resolve terminal width before calling.
    pub width: Option<usize>,
    /// Padding between columns.
    pub padding: usize,
    /// Prefix to print before each output row.
    pub indent: String,
    /// Newline string appended at row boundaries.
    pub nl: String,
}

impl Default for ColumnOptions {
    fn default() -> Self {
        Self {
            width: None,
            padding: 1,
            indent: String::new(),
            nl: "\n".to_owned(),
        }
    }
}

fn div_round_up(a: usize, b: usize) -> usize {
    if b == 0 {
        return a;
    }
    a.div_ceil(b)
}

fn item_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn xy_to_linear(layout: ColumnLayout, cols: usize, rows: usize, x: usize, y: usize) -> usize {
    match layout {
        ColumnLayout::Column => x * rows + y,
        ColumnLayout::Row => y * cols + x,
        ColumnLayout::Plain => y,
    }
}

/// Parse space- or comma-separated column tokens (Git `parse_config` / `parse_option`).
pub fn parse_column_tokens_into(value: &str, colopts: &mut ColOpts) -> Result<(), String> {
    let mut group_set: u8 = 0;
    for raw in value.split([' ', ',']) {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        parse_one_token(token, colopts, &mut group_set)?;
    }
    // Setting layout without enable/disable implies `always` (Git `parse_config`).
    if group_set & 1 != 0 && group_set & 2 == 0 {
        colopts.0 = (colopts.0 & !ENABLE_MASK) | ENABLED;
    }
    Ok(())
}

fn parse_one_token(token: &str, colopts: &mut ColOpts, group_set: &mut u8) -> Result<(), String> {
    const LAYOUT_SET: u8 = 1;
    const ENABLE_SET: u8 = 2;

    let (neg_dense, name) = token
        .strip_prefix("no")
        .filter(|rest| rest.len() > 2)
        .map(|rest| (true, rest))
        .unwrap_or((false, token));

    match name {
        "always" => {
            *group_set |= ENABLE_SET;
            colopts.0 = (colopts.0 & !ENABLE_MASK) | ENABLED;
        }
        "never" => {
            *group_set |= ENABLE_SET;
            colopts.0 = (colopts.0 & !ENABLE_MASK) | DISABLED;
        }
        "auto" => {
            *group_set |= ENABLE_SET;
            colopts.0 = (colopts.0 & !ENABLE_MASK) | AUTO;
        }
        "plain" => {
            *group_set |= LAYOUT_SET;
            colopts.0 = (colopts.0 & !LAYOUT_MASK) | LAYOUT_PLAIN;
        }
        "column" => {
            *group_set |= LAYOUT_SET;
            colopts.0 = (colopts.0 & !LAYOUT_MASK) | LAYOUT_COLUMN;
        }
        "row" => {
            *group_set |= LAYOUT_SET;
            colopts.0 = (colopts.0 & !LAYOUT_MASK) | LAYOUT_ROW;
        }
        "dense" => {
            if neg_dense {
                colopts.0 &= !DENSE;
            } else {
                colopts.0 |= DENSE;
            }
        }
        _ => return Err(format!("unsupported column option '{token}'")),
    }
    Ok(())
}

/// Apply `finalize_colopts` semantics: resolve `auto` using the supplied TTY state.
pub fn finalize_colopts(colopts: &mut ColOpts, stdout_is_tty: bool) {
    if colopts.0 & ENABLE_MASK != AUTO {
        return;
    }
    colopts.0 &= !ENABLE_MASK;
    if stdout_is_tty {
        colopts.0 |= ENABLED;
    }
}

fn compute_column_width(
    layout: ColumnLayout,
    list_len: usize,
    len: &[usize],
    cols: usize,
    rows: usize,
    width_idx: &mut [usize],
) {
    let n = list_len;
    for x in 0..cols {
        width_idx[x] = xy_to_linear(layout, cols, rows, x, 0);
        for y in 0..rows {
            let i = xy_to_linear(layout, cols, rows, x, y);
            if i < n && len[width_idx[x]] < len[i] {
                width_idx[x] = i;
            }
        }
    }
}

/// Print `list` using Git column layout; when inactive, prints one `indent` + item + `nl` per row.
pub fn print_columns(
    out: &mut impl Write,
    list: &[String],
    colopts: ColOpts,
    opts: &ColumnOptions,
) -> io::Result<()> {
    if list.is_empty() {
        return Ok(());
    }
    if !colopts.is_active() {
        for s in list {
            write!(out, "{}{}{}", opts.indent, s, opts.nl)?;
        }
        return Ok(());
    }

    let layout = colopts.layout_mode();
    if layout == ColumnLayout::Plain {
        for s in list {
            write!(out, "{}{}{}", opts.indent, s, opts.nl)?;
        }
        return Ok(());
    }

    let n = list.len();
    let len: Vec<usize> = list.iter().map(|s| item_width(s)).collect();

    // Git's fallback terminal width is 80, and `print_columns` uses one less.
    let width_budget = opts.width.unwrap_or(79);
    let indent_len = item_width(&opts.indent);

    let mut cell_w = 0usize;
    for &l in &len {
        cell_w = cell_w.max(l);
    }
    cell_w += opts.padding;

    let mut cols = (width_budget.saturating_sub(indent_len)) / cell_w;
    if cols == 0 {
        cols = 1;
    }
    let mut rows = div_round_up(n, cols);

    // Git only allocates `data->width` (per-column widest index) inside `shrink_columns`, i.e. the
    // dense path. In non-dense mode `display_cell`'s `data->width` is NULL, so every cell is padded
    // to the uniform `initial_width`. Mirror that: `width_idx` is `Some` only when dense.
    let mut width_idx: Option<Vec<usize>> = None;

    if colopts.dense() {
        let mut wi: Vec<usize> = vec![0; cols];
        compute_column_width(layout, n, &len, cols, rows, &mut wi);
        while rows > 1 {
            let prev_rows = rows;
            let prev_cols = cols;
            rows -= 1;
            cols = div_round_up(n, rows);
            if cols != prev_cols {
                wi.resize(cols, 0);
            }
            compute_column_width(layout, n, &len, cols, rows, &mut wi);

            let mut total = indent_len;
            for x in 0..cols {
                total += len[wi[x]];
                total += opts.padding;
            }
            if total > width_budget {
                rows = prev_rows;
                cols = prev_cols;
                wi.resize(cols, 0);
                compute_column_width(layout, n, &len, cols, rows, &mut wi);
                break;
            }
        }
        width_idx = Some(wi);
    }

    let initial_width = len.iter().copied().max().unwrap_or(0) + opts.padding;
    let spaces = vec![b' '; initial_width];

    for y in 0..rows {
        for x in 0..cols {
            let i = xy_to_linear(layout, cols, rows, x, y);
            if i >= n {
                continue;
            }

            let cell_len = len[i];
            let mut pad_len = cell_len;
            if let Some(ref wi) = width_idx {
                if len[wi[x]] < initial_width {
                    pad_len += initial_width - len[wi[x]];
                    pad_len = pad_len.saturating_sub(opts.padding);
                }
            }

            let newline = match layout {
                ColumnLayout::Column => i + rows >= n,
                ColumnLayout::Row => x == cols - 1 || i == n - 1,
                ColumnLayout::Plain => true,
            };

            if x == 0 {
                write!(out, "{}", opts.indent)?;
            }
            write!(out, "{}", list[i])?;
            if newline {
                write!(out, "{}", opts.nl)?;
            } else {
                let run = initial_width.saturating_sub(pad_len);
                let run = run.min(spaces.len());
                out.write_all(&spaces[..run])?;
            }
        }
    }

    Ok(())
}

/// Mark options as originating from the command line (`COL_PARSEOPT` + `COL_ENABLED`), then parse `arg`.
pub fn apply_column_cli_arg(colopts: &mut ColOpts, arg: Option<&str>) -> Result<(), String> {
    colopts.0 |= PARSEOPT;
    colopts.0 &= !ENABLE_MASK;
    colopts.0 |= ENABLED;
    if let Some(a) = arg {
        parse_column_tokens_into(a, colopts)?;
    }
    Ok(())
}

/// Read `column.status` and `column.ui` from config (Git `git_column_config` order).
pub fn merge_column_config(
    config: &crate::config::ConfigSet,
    colopts: &mut ColOpts,
) -> Result<(), String> {
    if let Some(v) = config.get("column.status") {
        parse_column_tokens_into(&v, colopts)?;
    }
    if let Some(v) = config.get("column.ui") {
        parse_column_tokens_into(&v, colopts)?;
    }
    Ok(())
}
