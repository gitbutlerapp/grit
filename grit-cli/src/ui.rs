//! Small output-formatting helpers shared by `grit` commands.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};

use grit_lib::diff::{DiffEntry, DiffStatus};
use grit_lib::pathspec::pathdiff;

use crate::context::{self, CommitSummary};

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
        let target = self.work_tree.join(repo_rel);
        match pathdiff_relative_lexical(&self.cwd, &target) {
            Ok(rel) if rel == "." => Cow::Borrowed(repo_rel),
            Ok(rel) => Cow::Owned(rel),
            Err(_) => Cow::Borrowed(repo_rel),
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_owned()),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn pathdiff_relative_lexical(from: &Path, to: &Path) -> Result<String, ()> {
    let from_norm = normalize_path(from);
    let to_norm = normalize_path(to);
    let from_parts: Vec<_> = from_norm.components().collect();
    let to_parts: Vec<_> = to_norm.components().collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut result = PathBuf::new();
    for _ in common..from_parts.len() {
        result.push("..");
    }
    for part in &to_parts[common..] {
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(path_to_slash(&result))
    }
}

/// Width of the change-label column, sized to the longest label
/// (`"type changed"`) so the paths after it line up.
const LABEL_WIDTH: usize = 12;

/// ANSI reset.
const RESET: &str = "\x1b[0m";
/// Dim gray for the abbreviated sha and the relative date.
const FG_DIM: &str = "38;5;244";
/// Soft blue for the author column.
const FG_AUTHOR: &str = "38;5;110";

/// Whether to emit ANSI color: only when stdout is a TTY and `NO_COLOR` is unset
/// (the de-facto standard for opting out — https://no-color.org).
fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

/// Wrap `text` in the SGR `code` (e.g. `"32"`) when `color` is enabled.
fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Longest author shown before truncation.
const AUTHOR_MAX: usize = 15;

/// Format commit-list rows (status / shortlog / log) as
/// `<dim sha>  <author>  <dim relative-date>  <subject>`, with the author and
/// date columns aligned. Colors are applied only on a TTY. The author is capped
/// at [`AUTHOR_MAX`], and on a terminal the subject is truncated so the row never
/// wraps.
pub fn commit_rows(commits: &[CommitSummary]) -> Vec<String> {
    let color = use_color();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let term_width = terminal_size::terminal_size().map(|(w, _)| w.0 as usize);

    let cells: Vec<(String, String, String, &str)> = commits
        .iter()
        .map(|c| {
            let full = c.oid.to_hex();
            let sha = full.get(..7).unwrap_or(&full).to_owned();
            let author = truncate(&c.author, AUTHOR_MAX);
            let date = context::relative_date_from(c.timestamp, now);
            (sha, author, date, c.subject.as_str())
        })
        .collect();

    let author_w = cells.iter().map(|c| c.1.chars().count()).max().unwrap_or(0);
    let date_w = cells.iter().map(|c| c.2.chars().count()).max().unwrap_or(0);
    // Columns before the subject: "  " + sha(7) + "  " + author + "  " + date + "  ".
    let prefix_w = 2 + 7 + 2 + author_w + 2 + date_w + 2;

    cells
        .iter()
        .map(|(sha, author, date, subject)| {
            // Truncate the subject to the terminal width so the row doesn't wrap.
            // When not on a terminal (piped), leave it intact.
            let subject = match term_width {
                Some(w) if w > prefix_w => truncate(subject, w - prefix_w),
                _ => (*subject).to_owned(),
            };
            format!(
                "  {}  {}  {}  {subject}",
                paint(color, FG_DIM, sha),
                paint(color, FG_AUTHOR, &format!("{author:<author_w$}")),
                paint(color, FG_DIM, &format!("{date:<date_w$}")),
            )
        })
        .collect()
}

/// Truncate `s` to at most `max` display columns, using `…` for the last cell
/// when shortened.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

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

/// Print a titled group of diff entries (does nothing when empty).
///
/// Each line is `  <glyph>  <label>  <path>`: the glyph and the fixed-width label
/// column come first (colored by status on a TTY) so the paths line up.
pub fn print_change_group(
    title: &str,
    entries: &[DiffEntry],
    path_display: Option<&PathDisplayContext>,
) {
    if entries.is_empty() {
        return;
    }
    let color = use_color();
    println!("{}", paint(color, "1", title));
    for entry in entries {
        let g = glyph(&entry.status);
        let l = label(entry);
        let marker = format!("{g}  {l:<width$}", width = LABEL_WIDTH);
        let repo_path = entry_path(entry);
        let shown = path_display
            .map(|ctx| ctx.format_repo_path(repo_path).into_owned())
            .unwrap_or_else(|| repo_path.to_owned());
        println!(
            "  {}  {}",
            paint(color, status_color(&entry.status), &marker),
            shown
        );
    }
    println!();
}

/// Print the untracked-files group (does nothing when empty).
pub fn print_untracked(paths: &[String], path_display: Option<&PathDisplayContext>) {
    if paths.is_empty() {
        return;
    }
    let color = use_color();
    println!("{}", paint(color, "1", "Untracked"));
    for path in paths {
        let shown = path_display
            .map(|ctx| ctx.format_repo_path(path).into_owned())
            .unwrap_or_else(|| path.clone());
        let marker = format!("?  {l:<width$}", l = "untracked", width = LABEL_WIDTH);
        println!("  {}  {shown}", paint(color, "31", &marker));
    }
    println!();
}
