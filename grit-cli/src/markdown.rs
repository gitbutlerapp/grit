//! Shared Markdown formatters for `--markdown` command output.

use std::io::Write;

use grit_lib::diff::{DiffEntry, DiffStatus};

use crate::commands::diff::{FileDiff, Hunk, Line, LineKind, Segment};
use crate::context::{self, CommitSummary};
use crate::output::{outln, RenderContext};
use crate::stdio;
use crate::ui::{self, PathDisplayContext};

/// Print a list of commits as Markdown bullets (`oid`, subject, author, date).
pub fn print_commit_list(
    w: &mut dyn Write,
    commits: &[CommitSummary],
    ctx: &RenderContext,
) -> anyhow::Result<()> {
    if commits.is_empty() {
        outln!(w, "No commits.");
        return Ok(());
    }
    let now = ctx.now.unix_timestamp();
    for c in commits {
        outln!(w, "{}", commit_bullet(c, now));
    }
    Ok(())
}

fn commit_bullet(c: &CommitSummary, now: i64) -> String {
    let full = c.oid.to_hex();
    let short = full.get(..7).unwrap_or(&full);
    let date = context::relative_date_from(c.timestamp, now);
    format!("- `{short}` {} ({}, {date})", c.subject, c.author)
}

/// Print a titled group of index/worktree changes.
pub fn print_change_section(
    w: &mut dyn Write,
    title: &str,
    entries: &[DiffEntry],
    path_display: Option<&PathDisplayContext>,
) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    outln!(w, "## {title}");
    for entry in entries {
        let repo_path = ui::entry_path(entry);
        let shown = path_display
            .map(|ctx| ctx.format_repo_path(repo_path).into_owned())
            .unwrap_or_else(|| repo_path.to_owned());
        outln!(w, "- **{}** `{shown}`", change_label(&entry.status));
    }
    outln!(w);
    Ok(())
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
pub fn print_untracked(
    w: &mut dyn Write,
    paths: &[String],
    path_display: Option<&PathDisplayContext>,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    outln!(w, "## Untracked");
    for path in paths {
        let shown = path_display
            .map(|ctx| ctx.format_repo_path(path).into_owned())
            .unwrap_or_else(|| path.clone());
        outln!(w, "- `{shown}`");
    }
    outln!(w);
    Ok(())
}

/// Render a full diff outcome as Markdown (one fenced `diff` block per file).
pub fn print_diff_files(w: &mut dyn Write, files: &[FileDiff]) -> anyhow::Result<()> {
    if files.is_empty() {
        outln!(w, "No changes.");
        return Ok(());
    }
    for (i, file) in files.iter().enumerate() {
        if i > 0 {
            outln!(w);
        }
        let header = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path),
            None => file.path.clone(),
        };
        outln!(w, "## {header}");
        if file.binary {
            if file.content_changed {
                outln!(w);
                outln!(w, "Binary file differs.");
            }
            continue;
        }
        outln!(w);
        print_fenced_diff(w, &render_file_patch(file))?;
    }
    Ok(())
}

/// Pick a fence delimiter longer than any run of backticks appearing in `body`.
#[must_use]
pub fn diff_code_fence(body: &str) -> (String, String) {
    let mut ticks = 3usize;
    loop {
        let marker = "`".repeat(ticks);
        if !body.contains(&marker) {
            return (format!("{marker}diff"), marker);
        }
        ticks += 1;
    }
}

fn print_fenced_diff(w: &mut dyn Write, body: &str) -> anyhow::Result<()> {
    let (open, close) = diff_code_fence(body);
    outln!(w, "{open}");
    stdio::io_result(writeln!(w, "{body}")).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    outln!(w, "{close}");
    Ok(())
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
pub fn print_branch_list(
    w: &mut dyn Write,
    branches: &[crate::commands::branch::BranchEntry],
) -> anyhow::Result<()> {
    if branches.is_empty() {
        outln!(w, "No branches yet.");
        return Ok(());
    }
    outln!(w, "## Branches");
    for branch in branches {
        if branch.current {
            outln!(w, "- **`{}`** (current)", branch.name);
        } else {
            outln!(w, "- `{}`", branch.name);
        }
    }
    Ok(())
}

/// Config entries as a two-column table.
pub fn print_config_entries(
    w: &mut dyn Write,
    entries: &[crate::commands::config::ConfigEntry],
) -> anyhow::Result<()> {
    if entries.is_empty() {
        outln!(w, "No config entries.");
        return Ok(());
    }
    outln!(w, "| Key | Value |");
    outln!(w, "| --- | --- |");
    for entry in entries {
        let value = entry.value.as_deref().unwrap_or("");
        outln!(w, "| `{}` | {} |", entry.key, escape_table_cell(value));
    }
    Ok(())
}

/// Escape cell text so it stays on one Markdown table row.
///
/// Trims trailing line terminators (common in blame content on CRLF files), replaces
/// any remaining `\r`/`\n` with visible escapes, and escapes `|`.
#[must_use]
pub fn escape_table_cell(s: &str) -> String {
    let trimmed = s.trim_end_matches(['\n', '\r']);
    trimmed
        .replace('|', "\\|")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// Remotes as a two-column table.
pub fn print_remote_list(
    w: &mut dyn Write,
    remotes: &[crate::commands::remote::RemoteEntry],
) -> anyhow::Result<()> {
    if remotes.is_empty() {
        outln!(
            w,
            "No remotes. Add one with: `grit remote add <name> <url>`."
        );
        return Ok(());
    }
    outln!(w, "| Name | URL |");
    outln!(w, "| --- | --- |");
    for remote in remotes {
        outln!(
            w,
            "| `{}` | {} |",
            remote.name,
            escape_table_cell(&remote.url)
        );
    }
    Ok(())
}

/// Tag names with short oids.
pub fn print_tag_list(
    w: &mut dyn Write,
    tags: &[crate::commands::tag::TagEntry],
) -> anyhow::Result<()> {
    if tags.is_empty() {
        outln!(w, "No tags yet.");
        return Ok(());
    }
    outln!(w, "## Tags");
    for tag in tags {
        let short = tag.oid.get(..7).unwrap_or(&tag.oid);
        let peel = tag
            .peeled
            .as_ref()
            .map(|p| format!(" → `{}`", p.get(..7).unwrap_or(p.as_str())))
            .unwrap_or_default();
        outln!(w, "- `{}` · {} `{short}`{peel}", tag.name, tag.kind);
    }
    Ok(())
}

/// Fetch ref updates as bullets (`ref` · old → new).
pub fn print_fetch_updates(
    w: &mut dyn Write,
    remote: &str,
    updates: &[crate::commands::fetch::FetchUpdate],
    updated: usize,
) -> anyhow::Result<()> {
    if updates.is_empty() && updated == 0 {
        outln!(w, "Already up to date with `{remote}`.");
        return Ok(());
    }
    outln!(w, "## Updates from `{remote}`");
    for update in updates {
        let from = update.old_oid.as_deref().map(short_hex).unwrap_or("new");
        let to = update
            .new_oid
            .as_deref()
            .map(short_hex)
            .unwrap_or("deleted");
        outln!(w, "- `{}` · `{from}` → `{to}`", update.ref_name);
    }
    if updated > 0 {
        outln!(w);
        outln!(
            w,
            "Fetched **{updated}** update{}.",
            if updated == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// Per-ref push results.
pub fn print_push_results(
    w: &mut dyn Write,
    remote: &str,
    branch: &str,
    results: &[crate::commands::push::PushRefResult],
) -> anyhow::Result<()> {
    if results.is_empty() {
        outln!(w, "No refs pushed to `{remote}`.");
        return Ok(());
    }
    outln!(w, "## Push to `{remote}`");
    for result in results {
        let target = format!("{remote} {}", result.ref_name);
        match result.status.as_str() {
            "ok" => outln!(
                w,
                "- **`{}`** · pushed `{branch}` → `{target}`",
                result.ref_name
            ),
            "up_to_date" => outln!(
                w,
                "- **`{}`** · `{target}` already up to date",
                result.ref_name
            ),
            _ => {
                let reason = result.reason.as_deref().unwrap_or("rejected");
                outln!(
                    w,
                    "- **`{}`** · rejected `{target}`: {reason}",
                    result.ref_name
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{diff_code_fence, escape_table_cell};

    #[test]
    fn diff_code_fence_uses_longer_delimiter_when_body_has_triple_backticks() {
        let body = " context\n+```\n";
        let (open, close) = diff_code_fence(body);
        assert_eq!(open, "````diff");
        assert_eq!(close, "````");
    }

    #[test]
    fn diff_code_fence_stays_at_three_when_body_is_safe() {
        let body = "+hello\n";
        let (open, close) = diff_code_fence(body);
        assert_eq!(open, "```diff");
        assert_eq!(close, "```");
    }

    #[test]
    fn escape_table_cell_trims_trailing_line_terminators() {
        assert_eq!(escape_table_cell("one\r\n"), "one");
        assert_eq!(escape_table_cell("two\n"), "two");
    }

    #[test]
    fn escape_table_cell_replaces_internal_newlines_and_pipes() {
        assert_eq!(escape_table_cell("a|b\nc"), "a\\|b\\nc");
    }
}
