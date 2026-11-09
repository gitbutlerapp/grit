//! Small output-formatting helpers shared by `grit` commands.

use std::borrow::Cow;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::output::outln;
use crate::render::style::{self, truncate};

use grit_lib::config::git_path::{relative_path_for_display, user_display_path};
use grit_lib::diff::{DiffEntry, DiffStatus};
use grit_lib::porcelain::quote_path::quote_path_for_display;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::pathdiff;

use crate::context::{self, CommitSummary};

/// Normalize a filesystem path for human and JSON output (forward slashes, no `\\?\` prefix).
#[must_use]
pub fn filesystem_path_for_user(path: &Path) -> String {
    user_display_path(path)
}

/// When the user runs `grit status` from a subdirectory, show paths relative to cwd
/// (like Git) instead of from the repository root.
#[derive(Clone, Debug)]
pub struct PathDisplayContext {
    cwd: PathBuf,
    work_tree: PathBuf,
}

impl PathDisplayContext {
    /// Build display context when `cwd` is not the work tree root.
    pub fn from_cwd_and_work_tree(cwd: PathBuf, work_tree: PathBuf) -> Option<Self> {
        pathdiff(&cwd, &work_tree)?;
        Some(Self { cwd, work_tree })
    }

    /// Map a repository-relative path to a cwd-relative display string.
    pub fn format_repo_path<'a>(&self, repo_rel: &'a str) -> Cow<'a, str> {
        let dir_trailing_slash = repo_rel.ends_with('/');
        let target = self
            .work_tree
            .join(repo_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        match relative_path_for_display(&self.cwd, &target) {
            Some(rel) if rel == "." && dir_trailing_slash => Cow::Owned("./".to_owned()),
            Some(rel) if rel == "." => Cow::Borrowed(repo_rel),
            Some(rel) => {
                if dir_trailing_slash && !rel.ends_with('/') {
                    Cow::Owned(format!("{rel}/"))
                } else {
                    Cow::Owned(rel)
                }
            }
            None => Cow::Borrowed(repo_rel),
        }
    }
}

/// Whether human path output should octal-escape all non-ASCII bytes (`core.quotePath`).
///
/// Grit's modern CLI defaults to `false` (UTF-8 names print literally). An explicit
/// `core.quotePath` setting is still honored.
fn quote_path_fully_for_modern_cli(repo: &Repository) -> Result<bool> {
    let config = repo.config().snapshot()?;
    Ok(crate::context::config_bool_default(
        config.as_ref(),
        "core.quotePath",
        false,
    ))
}

/// Cwd-relative path display and `core.quotepath` quoting for human / Markdown output.
#[derive(Clone, Debug)]
pub struct PathDisplayOptions {
    pub path_display: Option<PathDisplayContext>,
    pub quote_path_fully: bool,
}

/// Path display options for the current repository, when the working tree is available.
#[must_use]
pub fn path_display_for_discovered_repo() -> Option<PathDisplayOptions> {
    crate::context::discover()
        .ok()
        .and_then(|repo| PathDisplayOptions::from_repo(&repo).ok())
}

impl PathDisplayOptions {
    /// Build options from an open repository and the process current directory.
    pub fn from_repo(repo: &Repository) -> Result<Self> {
        let cwd = std::env::current_dir().context("could not read the current directory")?;
        let path_display = repo
            .work_tree()
            .and_then(|wt| PathDisplayContext::from_cwd_and_work_tree(cwd, wt.to_path_buf()));
        Ok(Self {
            path_display,
            quote_path_fully: quote_path_fully_for_modern_cli(repo)?,
        })
    }

    /// Map a repository-relative path to a safe human-facing string (Git `quote.c` style).
    #[must_use]
    pub fn format_path_for_human(&self, repo_rel: &str) -> String {
        let bytes = grit_lib::repo_relpath::storage_bytes(repo_rel);
        if std::str::from_utf8(&bytes).is_err() {
            return grit_lib::repo_relpath::display(repo_rel, self.quote_path_fully);
        }
        let rel = self
            .path_display
            .as_ref()
            .map(|ctx| ctx.format_repo_path(repo_rel).into_owned())
            .unwrap_or_else(|| repo_rel.to_owned());
        quote_path_for_display(&rel, self.quote_path_fully)
    }
}

/// Format a diff file title (`old → new` or the path alone), with Git-style quoting.
#[must_use]
pub fn format_diff_file_header(
    old_path: Option<&str>,
    path: &str,
    path_opts: Option<&PathDisplayOptions>,
) -> String {
    let new_shown = path_opts
        .map(|o| o.format_path_for_human(path))
        .unwrap_or_else(|| path.to_owned());
    match old_path {
        Some(old) => {
            let old_shown = path_opts
                .map(|o| o.format_path_for_human(old))
                .unwrap_or_else(|| old.to_owned());
            format!("{old_shown} → {new_shown}")
        }
        None => new_shown,
    }
}

/// Width of the change-label column, sized to the longest label
/// (`"type changed"`) so the paths after it line up.
const LABEL_WIDTH: usize = 12;

/// Dim gray for the abbreviated sha.
const FG_DIM: &str = "38;5;244";
/// Soft blue for the author column.
const FG_AUTHOR: &str = "38;5;110";
/// Muted violet for the change id.
const FG_CHANGE: &str = "38;5;139";
/// Soft amber for the relative date.
const FG_DATE: &str = "38;5;179";
/// Green for the insertions of a line summary.
const FG_ADDED: &str = "32";
/// Red for the deletions of a line summary.
const FG_REMOVED: &str = "31";

/// Longest author shown before truncation.
const AUTHOR_MAX: usize = 15;

const CHANGE_ID_DISPLAY_LEN: usize = 8;

/// Short change-id label for human log/show (eight reverse-hex digits).
#[must_use]
pub fn change_display_segment(change_id: &str) -> String {
    change_short_code(change_id)
}

/// First eight reverse-hex digits of a change id (no `change` prefix).
#[must_use]
pub fn change_short_code(change_id: &str) -> String {
    change_id.chars().take(CHANGE_ID_DISPLAY_LEN).collect()
}

/// Widths of the author and date columns of [`commit_rows_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitColumns {
    /// Author column width; longer names are cut with `…`.
    pub author: usize,
    /// Relative date column width.
    pub date: usize,
}

impl CommitColumns {
    /// Widths that fit `commits`, and that a later, older batch of commits also fits.
    ///
    /// Relative dates of commits older than a day are at most `11 months ago` wide, so the
    /// date column is never narrower than that; names longer than the first batch's are cut.
    #[must_use]
    pub fn for_stream(commits: &[CommitSummary], now: i64) -> Self {
        let measured = Self::measure(commits, now);
        Self {
            author: measured.author,
            date: measured.date.max("11 months ago".len()),
        }
    }

    fn measure(commits: &[CommitSummary], now: i64) -> Self {
        let width = |s: &str| s.chars().count();
        Self {
            author: commits
                .iter()
                .map(|c| width(&truncate(&c.author, AUTHOR_MAX)))
                .max()
                .unwrap_or(0),
            date: commits
                .iter()
                .map(|c| width(&context::relative_date_from(c.timestamp, now)))
                .max()
                .unwrap_or(0),
        }
    }
}

/// Lines a commit added and removed, for the summary at the end of a [`commit_rows_with`] row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineCounts {
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
}

/// Per-row extras for [`commit_rows_with`]; missing entries mean nothing extra for that row.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommitRowOptions<'a> {
    /// Columns left free on row `i` for text the caller adds to it (a graph prefix,
    /// decorations).
    pub reserved: &'a [usize],
    /// A `+added -removed` summary drawn at the right edge of row `i`.
    pub line_counts: &'a [Option<LineCounts>],
    /// Column widths to use instead of measuring these rows, so batches line up.
    pub columns: Option<CommitColumns>,
    /// Collision-aware abbreviated oids aligned with `commits` (from [`crate::log_display::abbrev_commit_summaries`]).
    pub sha_abbrevs: Option<&'a [String]>,
}

/// Formats commit summaries as aligned log rows, with per-row extras in `opts`.
pub fn commit_rows_with(
    commits: &[CommitSummary],
    now: i64,
    opts: &CommitRowOptions<'_>,
) -> Vec<String> {
    let color = crate::output::stdout_color_enabled();
    let term_width = style::terminal_width();
    let columns = opts
        .columns
        .unwrap_or_else(|| CommitColumns::measure(commits, now));

    let cells: Vec<(String, String, String, String, &str)> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let sha = opts
                .sha_abbrevs
                .and_then(|abbrevs| abbrevs.get(i))
                .cloned()
                .unwrap_or_else(|| c.oid.to_short_hex());
            let change = change_display_segment(&c.change_id);
            let author = truncate(&c.author, AUTHOR_MAX.min(columns.author.max(1)));
            let date = context::relative_date_from(c.timestamp, now);
            (sha, change, author, date, c.subject.as_str())
        })
        .collect();

    let sha_w = cells.iter().map(|c| c.0.chars().count()).max().unwrap_or(7);

    let change_w = cells
        .iter()
        .map(|c| c.1.chars().count())
        .max()
        .unwrap_or(0)
        .max(if opts.columns.is_some() {
            CHANGE_ID_DISPLAY_LEN
        } else {
            0
        });
    let author_w = columns.author;
    let date_w = columns.date;
    // Columns before the subject: "  " + sha + "  " + change + "  " + author + "  " + date + "  ".
    let prefix_w = 2 + sha_w + 2 + change_w + 2 + author_w + 2 + date_w + 2;
    let digits = |n: usize| n.to_string().len();
    let added_w = opts
        .line_counts
        .iter()
        .flatten()
        .map(|c| digits(c.added))
        .max()
        .unwrap_or(0);
    let removed_w = opts
        .line_counts
        .iter()
        .flatten()
        .map(|c| digits(c.removed))
        .max()
        .unwrap_or(0);

    cells
        .iter()
        .enumerate()
        .map(|(i, (sha, change, author, date, subject))| {
            let counts = opts.line_counts.get(i).copied().flatten();
            // `  +added -removed`, right-aligned so every row's summary ends at the same column.
            let counts_w = counts.map_or(0, |_| 2 + 1 + added_w + 2 + removed_w);
            // Truncate the subject to the terminal width so the row doesn't wrap.
            // When not on a terminal (piped), leave it intact.
            let used = prefix_w + counts_w + opts.reserved.get(i).copied().unwrap_or(0);
            // With no room left, keep just the `…` so the row wraps as little as possible.
            let subject = match term_width {
                Some(w) => truncate(subject, w.saturating_sub(used).max(1)),
                None => (*subject).to_owned(),
            };
            let mut row = format!(
                "  {}  {}  {}  {}  {subject}",
                style::paint(color, FG_DIM, sha),
                style::paint(color, FG_CHANGE, &format!("{change:<change_w$}")),
                style::paint(color, FG_AUTHOR, &format!("{author:<author_w$}")),
                style::paint(color, FG_DATE, &format!("{date:<date_w$}")),
            );
            if let Some(counts) = counts {
                let fill =
                    term_width.map_or(0, |w| w.saturating_sub(used + subject.chars().count()));
                row.push_str(&" ".repeat(fill + 2));
                let added = format!("+{}", counts.added);
                let removed = format!("-{}", counts.removed);
                row.push_str(&style::paint(
                    color,
                    FG_ADDED,
                    &format!("{added:>0$}", added_w + 1),
                ));
                row.push(' ');
                row.push_str(&style::paint(
                    color,
                    FG_REMOVED,
                    &format!("{removed:>0$}", removed_w + 1),
                ));
            }
            row
        })
        .collect()
}

pub use crate::render::style::{display_width, fit_width, terminal_width};

/// The path a diff entry refers to (prefers the new side, falls back to old).
pub fn entry_path(entry: &DiffEntry) -> &str {
    entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .unwrap_or("?")
}

/// A single-character glyph summarizing a change.
fn glyph(status: &DiffStatus) -> char {
    match status {
        DiffStatus::Added => '+',
        DiffStatus::Deleted => '-',
        DiffStatus::Modified | DiffStatus::TypeChanged => '~',
        DiffStatus::Renamed | DiffStatus::Copied => '»',
        DiffStatus::Unmerged => '!',
    }
}

/// A short word describing a change, shown in the (left) label column.
fn label(entry: &DiffEntry) -> &'static str {
    match entry.status {
        DiffStatus::Added => "new",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Modified => "modified",
        DiffStatus::TypeChanged => "type changed",
        DiffStatus::Renamed => "renamed",
        DiffStatus::Copied => "copied",
        DiffStatus::Unmerged => "conflict",
    }
}

/// ANSI SGR color code for a change status (green new, red deleted/conflict,
/// yellow modified, cyan renamed/copied).
fn status_color(status: &DiffStatus) -> &'static str {
    match status {
        DiffStatus::Added => "32",
        DiffStatus::Deleted | DiffStatus::Unmerged => "31",
        DiffStatus::Modified | DiffStatus::TypeChanged => "33",
        DiffStatus::Renamed | DiffStatus::Copied => "36",
    }
}

fn format_change_path(entry: &DiffEntry, path_opts: Option<&PathDisplayOptions>) -> String {
    let format_one = |p: &str| {
        path_opts
            .map(|opts| opts.format_path_for_human(p))
            .unwrap_or_else(|| p.to_owned())
    };
    match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            let old = entry.old_path.as_deref().unwrap_or("");
            let new = entry.new_path.as_deref().unwrap_or("");
            if old.is_empty() || new.is_empty() {
                format_one(entry_path(entry))
            } else {
                format!("{} → {}", format_one(old), format_one(new))
            }
        }
        _ => format_one(entry_path(entry)),
    }
}

/// Print a titled group of diff entries (does nothing when empty).
///
/// Each line is `  <glyph>  <label>  <path>`: the glyph and the fixed-width label
/// column come first (colored by status on a TTY) so the paths line up.
pub fn print_change_group(
    w: &mut dyn Write,
    title: &str,
    entries: &[DiffEntry],
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let color = crate::output::stdout_color_enabled();
    outln!(w, "{}", style::paint(color, "1", title));
    for entry in entries {
        let g = glyph(&entry.status);
        let l = label(entry);
        let marker = format!("{g}  {l:<width$}", width = LABEL_WIDTH);
        let shown = format_change_path(entry, path_opts);
        outln!(
            w,
            "  {}  {}",
            style::paint(color, status_color(&entry.status), &marker),
            shown
        );
    }
    outln!(w);
    Ok(())
}

/// Print the untracked-files group (does nothing when empty).
pub fn print_untracked(
    w: &mut dyn Write,
    paths: &[String],
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let color = crate::output::stdout_color_enabled();
    outln!(w, "{}", style::paint(color, "1", "Untracked"));
    for path in paths {
        let shown = path_opts
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.clone());
        let marker = format!("?  {l:<width$}", l = "untracked", width = LABEL_WIDTH);
        outln!(w, "  {}  {shown}", style::paint(color, "31", &marker));
    }
    outln!(w);
    Ok(())
}

/// Print ignored paths (does nothing when empty).
pub fn print_ignored(
    w: &mut dyn Write,
    paths: &[String],
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let color = crate::output::stdout_color_enabled();
    outln!(w, "{}", style::paint(color, "1", "Ignored"));
    for path in paths {
        let shown = path_opts
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.clone());
        let marker = format!("!  {l:<width$}", l = "ignored", width = LABEL_WIDTH);
        outln!(w, "  {}  {shown}", style::paint(color, "90", &marker));
    }
    outln!(w);
    Ok(())
}

#[cfg(test)]
mod path_display_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn filesystem_path_for_user_strips_verbatim_prefix_and_uses_slashes() {
        assert_eq!(
            filesystem_path_for_user(Path::new(r"\\?\C:\Users\x\wt")),
            "C:/Users/x/wt"
        );
    }
}
