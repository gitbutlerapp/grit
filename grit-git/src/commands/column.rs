//! `grit column` — display data in columns.
//!
//! Reads lines from stdin and formats them into columns, similar to
//! `git column`. Useful for displaying lists (branches, tags, etc.)
//! in a compact columnar layout.
//!
//! Usage:
//!   echo -e "a\nb\nc\nd\ne" | grit column --mode=column --width=40

use anyhow::{bail, Result};
use clap::Args as ClapArgs;
use std::io::{self, BufRead, IsTerminal, Write};

/// Arguments for `grit column`.
#[derive(Debug, ClapArgs)]
#[command(
    about = "Display data in columns",
    override_usage = "grit column [--mode=<mode>] [--width=<n>] [--padding=<n>] [--nl=<string>]"
)]
pub struct Args {
    /// Column layout mode (e.g. column, row, plain, never, always, auto).
    /// Can include modifiers separated by comma: column,dense or row,dense.
    #[arg(long = "mode", default_value = "column")]
    pub mode: String,

    /// Total display width (defaults to `$COLUMNS` or 80).
    #[arg(long = "width")]
    pub width: Option<usize>,

    /// Padding between columns (defaults to 1).
    #[arg(long = "padding", default_value_t = 1, allow_hyphen_values = true)]
    pub padding: i32,

    /// Indentation prefix for each output line.
    #[arg(long = "indent", default_value = "")]
    pub indent: String,

    /// String to append to each output line instead of the default newline.
    #[arg(long = "nl")]
    pub nl: Option<String>,
}

/// Parsed column display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnMode {
    Always,
    Column,
    Row,
    Plain,
    Never,
    Auto,
}

/// Run the `column` command.
pub fn run(args: Args) -> Result<()> {
    if args.padding < 0 {
        eprintln!("fatal: --padding must be non-negative");
        std::process::exit(128);
    }
    let padding = args.padding as usize;
    let width = resolve_width(args.width);

    let (mode, dense) = parse_mode(&args.mode)?;

    let stdin = io::stdin();
    let items: Vec<String> = stdin.lock().lines().collect::<io::Result<Vec<_>>>()?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mode = match mode {
        ColumnMode::Auto => {
            if io::stdout().is_terminal() {
                ColumnMode::Column
            } else {
                ColumnMode::Plain
            }
        }
        other => other,
    };

    match mode {
        ColumnMode::Never => {
            // Match git behavior: "--mode=never" bypasses column formatting
            // options and prints raw input.
            for item in &items {
                writeln!(out, "{item}")?;
            }
        }
        ColumnMode::Plain => {
            if let Some(ref nl) = args.nl {
                for item in &items {
                    write!(out, "{}{item}{nl}", args.indent)?;
                }
            } else {
                for item in &items {
                    writeln!(out, "{}{item}", args.indent)?;
                }
            }
        }
        ColumnMode::Row => {
            if dense {
                format_rows_dense(&items, width, padding, &args.indent, &mut out)?;
            } else {
                format_rows(&items, width, padding, &args.indent, &mut out)?;
            }
        }
        ColumnMode::Column | ColumnMode::Always | ColumnMode::Auto => {
            if dense {
                format_columns_dense(&items, width, padding, &args.indent, &mut out)?;
            } else {
                format_columns(&items, width, padding, &args.indent, &mut out)?;
            }
        }
    }

    Ok(())
}

fn resolve_width(explicit_width: Option<usize>) -> usize {
    explicit_width
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
        })
        .unwrap_or(80)
}

/// Parse a mode string like "column", "column,dense", "row,nodense" etc.
fn parse_mode(s: &str) -> Result<(ColumnMode, bool)> {
    let parts: Vec<&str> = s.split(',').collect();
    let base = match parts[0] {
        "always" => ColumnMode::Always,
        "column" => ColumnMode::Column,
        "row" => ColumnMode::Row,
        "plain" => ColumnMode::Plain,
        "never" => ColumnMode::Never,
        "auto" => ColumnMode::Auto,
        other => bail!("unknown column mode: {}", other),
    };
    let mut dense = false;
    for &part in &parts[1..] {
        match part {
            "dense" => dense = true,
            "nodense" => dense = false,
            other => bail!("unknown column mode modifier: {}", other),
        }
    }
    Ok((base, dense))
}

/// Format items filling rows left-to-right, wrapping when width exceeded.
fn format_rows(
    items: &[String],
    width: usize,
    padding: usize,
    indent: &str,
    out: &mut impl Write,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    let max_item = items.iter().map(|s| s.len()).max().unwrap_or(0);
    let col_width = max_item + padding;
    let usable = width.saturating_sub(indent.len());
    let num_cols = usable.checked_div(col_width).unwrap_or(0).max(1);

    let mut col = 0;
    for (i, item) in items.iter().enumerate() {
        if col == 0 {
            write!(out, "{indent}")?;
        }
        let is_last_item = i + 1 == items.len();
        col += 1;
        if col < num_cols && !is_last_item {
            write!(out, "{item:<width$}", width = col_width)?;
        } else if col < num_cols {
            write!(out, "{item}")?;
        } else {
            writeln!(out, "{item}")?;
            col = 0;
        }
    }
    if col != 0 {
        writeln!(out)?;
    }

    Ok(())
}

/// Format items filling columns top-to-bottom, then left-to-right.
fn format_columns(
    items: &[String],
    width: usize,
    padding: usize,
    indent: &str,
    out: &mut impl Write,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    let max_item = items.iter().map(|s| s.len()).max().unwrap_or(0);
    let col_width = max_item + padding;
    let usable = width.saturating_sub(indent.len());
    let num_cols = usable.checked_div(col_width).unwrap_or(0).max(1);
    let num_rows = items.len().div_ceil(num_cols);

    for row in 0..num_rows {
        write!(out, "{indent}")?;
        for col in 0..num_cols {
            let idx = col * num_rows + row;
            if idx >= items.len() {
                break;
            }
            let item = &items[idx];
            if col + 1 < num_cols && (col + 1) * num_rows + row < items.len() {
                write!(out, "{item:<width$}", width = col_width)?;
            } else {
                write!(out, "{item}")?;
            }
        }
        writeln!(out)?;
    }

    Ok(())
}

/// Dense column layout: compute per-column widths instead of uniform max width.
/// Items fill columns top-to-bottom, then left-to-right.
fn format_columns_dense(
    items: &[String],
    width: usize,
    padding: usize,
    indent: &str,
    out: &mut impl Write,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    let usable = width.saturating_sub(indent.len());

    // Try increasing number of columns until it doesn't fit
    let mut best_cols = 1;
    let mut best_rows = items.len();
    let mut best_col_widths: Vec<usize> = vec![0];

    for num_cols in 1..=items.len() {
        let num_rows = items.len().div_ceil(num_cols);
        let mut col_widths = vec![0usize; num_cols];
        for (i, item) in items.iter().enumerate() {
            let col = i / num_rows;
            if col >= num_cols {
                break;
            }
            col_widths[col] = col_widths[col].max(item.len());
        }
        let total: usize = col_widths.iter().sum::<usize>()
            + if num_cols > 1 {
                (num_cols - 1) * padding
            } else {
                0
            };
        if total <= usable {
            best_cols = num_cols;
            best_rows = num_rows;
            best_col_widths = col_widths;
        } else {
            break;
        }
    }

    for row in 0..best_rows {
        write!(out, "{indent}")?;
        for (col, col_width) in best_col_widths.iter().enumerate().take(best_cols) {
            let idx = col * best_rows + row;
            if idx >= items.len() {
                break;
            }
            let item = &items[idx];
            if col + 1 < best_cols && (col + 1) * best_rows + row < items.len() {
                let w = *col_width + padding;
                write!(out, "{item:<width$}", width = w)?;
            } else {
                write!(out, "{item}")?;
            }
        }
        writeln!(out)?;
    }

    Ok(())
}

/// Dense row layout: compute per-column widths, fill rows left-to-right.
fn format_rows_dense(
    items: &[String],
    width: usize,
    padding: usize,
    indent: &str,
    out: &mut impl Write,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    let usable = width.saturating_sub(indent.len());

    // Try increasing number of columns until it doesn't fit
    let mut best_cols = 1;
    let mut best_col_widths: Vec<usize> = vec![0];

    for num_cols in 1..=items.len() {
        let num_rows = items.len().div_ceil(num_cols);
        let mut col_widths = vec![0usize; num_cols];
        for (i, item) in items.iter().enumerate() {
            let col = i % num_cols;
            col_widths[col] = col_widths[col].max(item.len());
        }
        let total: usize = col_widths.iter().sum::<usize>()
            + if num_cols > 1 {
                (num_cols - 1) * padding
            } else {
                0
            };
        if total <= usable {
            best_cols = num_cols;
            best_col_widths = col_widths;
        } else {
            break;
        }
        if num_rows == 1 {
            break;
        }
    }

    let mut col = 0;
    for (i, item) in items.iter().enumerate() {
        if col == 0 {
            write!(out, "{indent}")?;
        }
        let is_last_item = i + 1 == items.len();
        col += 1;
        if col < best_cols && !is_last_item {
            let w = best_col_widths[col - 1] + padding;
            write!(out, "{item:<width$}", width = w)?;
        } else {
            writeln!(out, "{item}")?;
            col = 0;
        }
    }
    if col != 0 {
        writeln!(out)?;
    }

    Ok(())
}
