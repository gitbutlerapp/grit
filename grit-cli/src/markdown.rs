//! Shared Markdown formatters for `--markdown` command output.

use std::io::Write as _;

use grit_lib::diff::{DiffEntry, DiffStatus};

use crate::commands::diff::{FileDiff, Hunk, Line, LineKind, Segment};
use crate::context::{self, CommitSummary};
use crate::stdio;
use crate::ui::{self, PathDisplayContext};

/// Print a list of commits as Markdown bullets (`oid`, subject, author, date).
pub fn print_commit_list(commits: &[CommitSummary]) {
    if commits.is_empty() {
        println!("No commits.");
        return;
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    for c in commits {
        println!("{}", commit_bullet(c, now));
    }
}

fn commit_bullet(c: &CommitSummary, now: i64) -> String {
    let full = c.oid.to_hex();
    let short = full.get(..7).unwrap_or(&full);
    let date = context::relative_date_from(c.timestamp, now);
    format!("- `{short}` {} ({}, {date})", c.subject, c.author)
}

/// Print a titled group of index/worktree changes.
pub fn print_change_section(
    title: &str,
    entries: &[DiffEntry],
    path_display: Option<&PathDisplayContext>,
) {
    if entries.is_empty() {
        return;
    }
    println!("## {title}");
    for entry in entries {
        let repo_path = ui::entry_path(entry);
        let shown = path_display
            .map(|ctx| ctx.format_repo_path(repo_path).into_owned())
            .unwrap_or_else(|| repo_path.to_owned());
        println!("- **{}** `{shown}`", change_label(&entry.status));
    }
    println!();
}

fn change_label(status: &DiffStatus) -> &'static str {
    match status {
        DiffStatus::Added => "new",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Modified => "modified",
        DiffStatus::TypeChanged => "type changed",
        DiffStatus::Renamed => "renamed",
        DiffStatus::Copied => "copied",
        DiffStatus::Unmerged => "conflict",
    }
}

/// Print untracked paths under a heading.
pub fn print_untracked(paths: &[String], path_display: Option<&PathDisplayContext>) {
    if paths.is_empty() {
        return;
    }
    println!("## Untracked");
    for path in paths {
        let shown = path_display
            .map(|ctx| ctx.format_repo_path(path).into_owned())
            .unwrap_or_else(|| path.clone());
        println!("- `{shown}`");
    }
    println!();
}

/// Render a full diff outcome as Markdown (one fenced `diff` block per file).
pub fn print_diff_files(files: &[FileDiff]) {
    if files.is_empty() {
        println!("No changes.");
        return;
    }
    for (i, file) in files.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let header = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path),
            None => file.path.clone(),
        };
        println!("## {header}");
        if file.binary {
            println!();
            println!("Binary file differs.");
            continue;
        }
        println!();
        print_fenced_diff(&render_file_patch(file));
    }
}

fn print_fenced_diff(body: &str) {
    println!("```diff");
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = stdio::io_result(writeln!(lock, "{body}"));
    let _ = stdio::io_result(writeln!(lock, "```"));
}

fn render_file_patch(file: &FileDiff) -> String {
    let mut out = String::new();
    for hunk in &file.hunks {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&render_hunk_header(hunk));
        out.push('\n');
        for line in &hunk.lines {
            out.push_str(&render_patch_line(line));
        }
    }
    out.trim_end_matches('\n').to_owned()
}

fn render_hunk_header(hunk: &Hunk) -> String {
    let old_len = hunk
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Add)
        .count();
    let new_len = hunk
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Del)
        .count();
    if let Some(ctx) = &hunk.context {
        format!(
            "@@ -{},{} +{},{} @@ {}",
            hunk.old_start, old_len, hunk.new_start, new_len, ctx
        )
    } else {
        format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, old_len, hunk.new_start, new_len
        )
    }
}

fn render_patch_line(line: &Line) -> String {
    let sign = match line.kind {
        LineKind::Context => ' ',
        LineKind::Add => '+',
        LineKind::Del => '-',
    };
    format!("{sign}{}", segments_text(&line.segments))
}

fn segments_text(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect()
}
