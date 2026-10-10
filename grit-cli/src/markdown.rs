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
            out.push('\n');
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

/// Branch list: current branch marked, others plain.
pub fn print_branch_list(branches: &[crate::commands::branch::BranchEntry]) {
    if branches.is_empty() {
        println!("No branches yet.");
        return;
    }
    println!("## Branches");
    for branch in branches {
        if branch.current {
            println!("- **`{}`** (current)", branch.name);
        } else {
            println!("- `{}`", branch.name);
        }
    }
}

/// Config entries as a two-column table.
pub fn print_config_entries(entries: &[crate::commands::config::ConfigEntry]) {
    if entries.is_empty() {
        println!("No config entries.");
        return;
    }
    println!("| Key | Value |");
    println!("| --- | --- |");
    for entry in entries {
        let value = entry.value.as_deref().unwrap_or("");
        println!("| `{}` | {} |", entry.key, escape_table_cell(value));
    }
}

fn escape_table_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Remotes as a two-column table.
pub fn print_remote_list(remotes: &[crate::commands::remote::RemoteEntry]) {
    if remotes.is_empty() {
        println!("No remotes. Add one with: `grit remote add <name> <url>`.");
        return;
    }
    println!("| Name | URL |");
    println!("| --- | --- |");
    for remote in remotes {
        println!("| `{}` | {} |", remote.name, escape_table_cell(&remote.url));
    }
}

/// Tag names with short oids.
pub fn print_tag_list(tags: &[crate::commands::tag::TagEntry]) {
    if tags.is_empty() {
        println!("No tags yet.");
        return;
    }
    println!("## Tags");
    for tag in tags {
        let short = tag.oid.get(..7).unwrap_or(&tag.oid);
        println!("- `{}` (`{short}`)", tag.name);
    }
}

/// Fetch ref updates as bullets (`ref` · old → new).
pub fn print_fetch_updates(
    remote: &str,
    updates: &[crate::commands::fetch::FetchUpdate],
    updated: usize,
) {
    if updates.is_empty() && updated == 0 {
        println!("Already up to date with `{remote}`.");
        return;
    }
    println!("## Updates from `{remote}`");
    for update in updates {
        let from = update.old_oid.as_deref().map(short_hex).unwrap_or("new");
        let to = update
            .new_oid
            .as_deref()
            .map(short_hex)
            .unwrap_or("deleted");
        println!("- `{}` · `{from}` → `{to}`", update.ref_name);
    }
    if updated > 0 {
        println!();
        println!(
            "Fetched **{updated}** update{}.",
            if updated == 1 { "" } else { "s" }
        );
    }
}

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// Per-ref push results.
pub fn print_push_results(
    remote: &str,
    branch: &str,
    results: &[crate::commands::push::PushRefResult],
) {
    if results.is_empty() {
        println!("No refs pushed to `{remote}`.");
        return;
    }
    println!("## Push to `{remote}`");
    for result in results {
        let target = format!("{remote} {}", result.ref_name);
        match result.status.as_str() {
            "ok" => println!(
                "- **`{}`** · pushed `{branch}` → `{target}`",
                result.ref_name
            ),
            "up_to_date" => println!(
                "- **`{}`** · `{target}` already up to date",
                result.ref_name
            ),
            _ => {
                let reason = result.reason.as_deref().unwrap_or("rejected");
                println!(
                    "- **`{}`** · rejected `{target}`: {reason}",
                    result.ref_name
                );
            }
        }
    }
}
