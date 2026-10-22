//! Shared Markdown formatters for `--markdown` command output.

use std::io::Write;

use grit_lib::diff::{DiffEntry, DiffStatus};

use crate::commands::add::AddOutcome;
use crate::commands::auth::{AuthOutcome, LogoutOutcome};
use crate::commands::commit::CommitOutcome;
use crate::commands::diff::{file_diff_status_label, FileDiff, Hunk, Line, LineKind, Segment};
use crate::commands::history::{
    HeadPair, HistoryAbsorbOutcome, HistoryRewriteOutcome, RefPair, RewrittenPair,
};
use crate::commands::merge::{MergeOutcome, MergeResult};
use crate::commands::pick::PickOutcome;
use crate::commands::revert::RevertOutcome;
use crate::commands::stash::{
    StashApplyOutcome, StashDropOutcome, StashPopOutcome, StashPushOutcome,
};
use crate::commands::switch::SwitchOutcome;
use crate::commands::update::UpdateOutcome;
use crate::commands::worktree::{WorktreeOutcome, WorktreeRow};
use crate::context::{self, CommitSummary};
use crate::output::{outln, RenderContext};
use crate::stdio;
use crate::ui::{self, PathDisplayOptions};

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

/// One commit bullet with optional `--decorate` parenthetical after the short oid.
pub fn commit_bullet_with_decorations(
    c: &CommitSummary,
    refs: &[crate::output::CommitRefJson],
    head_branch: Option<&str>,
    now: i64,
) -> String {
    let full = c.oid.to_hex();
    let short = full.get(..7).unwrap_or(&full);
    let date = context::relative_date_from(c.timestamp, now);
    let deco = crate::log_display::format_decorations_folded(refs, head_branch);
    let decoration = if deco.is_empty() {
        String::new()
    } else {
        format!(" {deco}")
    };
    let change = ui::change_display_segment(&c.change_id);
    format!(
        "- `{short}` {change}{decoration} {} ({}, {date})",
        c.subject, c.author
    )
}

fn commit_bullet(c: &CommitSummary, now: i64) -> String {
    commit_bullet_with_decorations(c, &[], None, now)
}

/// Print a titled group of index/worktree changes.
pub fn print_change_section(
    w: &mut dyn Write,
    title: &str,
    entries: &[DiffEntry],
    path_opts: Option<&PathDisplayOptions>,
) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    outln!(w, "## {title}");
    for entry in entries {
        let repo_path = ui::entry_path(entry);
        let shown = path_opts
            .map(|opts| format_inline_code(&opts.format_path_for_human(repo_path)))
            .unwrap_or_else(|| format_inline_code(repo_path));
        outln!(w, "- **{}** {shown}", change_label(&entry.status));
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
    path_opts: Option<&PathDisplayOptions>,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    outln!(w, "## Untracked");
    for path in paths {
        let shown = path_opts
            .map(|opts| format_inline_code(&opts.format_path_for_human(path)))
            .unwrap_or_else(|| format_inline_code(path));
        outln!(w, "- {shown}");
    }
    outln!(w);
    Ok(())
}

/// Print ignored paths under a heading.
pub fn print_ignored(
    w: &mut dyn Write,
    paths: &[String],
    path_opts: Option<&PathDisplayOptions>,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    outln!(w, "## Ignored");
    for path in paths {
        let shown = path_opts
            .map(|opts| format_inline_code(&opts.format_path_for_human(path)))
            .unwrap_or_else(|| format_inline_code(path));
        outln!(w, "- {shown}");
    }
    outln!(w);
    Ok(())
}

/// Render a full diff outcome as Markdown (one fenced `diff` block per file).
pub fn print_diff_files(
    w: &mut dyn Write,
    files: &[FileDiff],
    path_opts: Option<&PathDisplayOptions>,
) -> anyhow::Result<()> {
    if files.is_empty() {
        outln!(w, "No changes.");
        return Ok(());
    }
    for (i, file) in files.iter().enumerate() {
        if i > 0 {
            outln!(w);
        }
        let header = ui::format_diff_file_header(file.old_path.as_deref(), &file.path, path_opts);
        outln!(w, "## {header}");
        print_file_diff_metadata(w, file)?;
        if file.binary {
            if file.content_changed {
                outln!(w);
                outln!(w, "Binary file differs.");
            }
            continue;
        }
        let patch = render_file_patch(file);
        if !patch.is_empty() {
            outln!(w);
            print_fenced_diff(w, &patch)?;
        }
    }
    Ok(())
}

fn print_file_diff_metadata(w: &mut dyn Write, file: &FileDiff) -> anyhow::Result<()> {
    outln!(w, "**status:** {}", file_diff_status_label(file.status));
    if file.old_mode != file.new_mode {
        if let Some(m) = &file.old_mode {
            outln!(w, "**old_mode:** `{}`", m);
        }
        if let Some(m) = &file.new_mode {
            outln!(w, "**new_mode:** `{}`", m);
        }
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
    for hunk in file.hunks.as_deref().into_iter().flatten() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&render_hunk_header(hunk));
        out.push('\n');
        for line in &hunk.lines {
            out.push_str(&render_patch_line(line));
            out.push('\n');
            if line.no_newline_at_eof {
                out.push_str("\\ No newline at end of file\n");
            }
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

/// Wrap `content` in a Markdown inline code span.
///
/// Picks a backtick delimiter longer than any run of backticks in `content` (CommonMark
/// code spans do not treat backslash as escape inside single-backtick spans).
#[must_use]
pub fn format_inline_code(content: &str) -> String {
    let mut ticks = 1usize;
    loop {
        let marker = "`".repeat(ticks);
        if !content.contains(&marker) {
            let pad = content.starts_with(' ')
                || content.ends_with(' ')
                || content.starts_with('`')
                || content.ends_with('`');
            if pad {
                return format!("{marker} {content} {marker}");
            }
            return format!("{marker}{content}{marker}");
        }
        ticks += 1;
    }
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

/// Notes attached to commits under one ref.
pub fn print_notes_list(
    w: &mut dyn Write,
    notes_ref: &str,
    notes: &[crate::commands::notes::NoteEntry],
) -> anyhow::Result<()> {
    outln!(w, "## Notes (`{notes_ref}`)");
    if notes.is_empty() {
        outln!(w, "- _(no notes)_");
        return Ok(());
    }
    for entry in notes {
        let short = entry.object.get(..7).unwrap_or(&entry.object);
        let preview = entry.body.trim_end().replace('\n', " ");
        outln!(w, "- `{short}` — {preview}");
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
    let mut any_rejected = false;
    for update in updates {
        if update.status == crate::commands::fetch::FetchUpdateStatus::Rejected {
            any_rejected = true;
            let reason = update.reason.as_deref().unwrap_or("ref update rejected");
            outln!(w, "- **`{}`** · rejected: {reason}", update.ref_name);
            continue;
        }
        let from = update.old_oid.as_deref().map(short_hex).unwrap_or("new");
        let to = update
            .new_oid
            .as_deref()
            .map(short_hex)
            .unwrap_or("deleted");
        if update.remote_ref.is_empty() || update.remote_ref == update.ref_name {
            outln!(w, "- `{}` · `{from}` → `{to}`", update.ref_name);
        } else {
            outln!(
                w,
                "- `{}` ← `{}` · `{from}` → `{to}`",
                update.ref_name,
                update.remote_ref
            );
        }
    }
    if any_rejected {
        outln!(w);
        outln!(w, "Some ref updates from `{remote}` were **rejected**.");
    } else if updated > 0 {
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

fn short_change_id(id: &str) -> &str {
    id.get(..7).unwrap_or(id)
}

/// Result of `grit commit` / `grit commit --amend`.
pub fn print_commit_outcome(w: &mut dyn Write, outcome: &CommitOutcome) -> anyhow::Result<()> {
    outln!(w, "## Commit");
    outln!(w);
    if outcome.dry_run {
        outln!(
            w,
            "- **Dry run** — would commit on **`{}`**",
            outcome.branch
        );
        outln!(w, "- **Subject:** {}", outcome.subject);
        outln!(w, "- **Files:** {}", outcome.files.len());
        return Ok(());
    }
    let oid = outcome.oid.as_deref().unwrap_or("");
    let short = short_hex(oid);
    if outcome.amended {
        outln!(
            w,
            "- Amended **`{}`** at `{short}` — {}",
            outcome.branch,
            outcome.subject
        );
    } else {
        outln!(
            w,
            "- **`{}`** · `{short}` — {}",
            outcome.branch,
            outcome.subject
        );
    }
    let word = if outcome.changes == 1 {
        "change"
    } else {
        "changes"
    };
    outln!(w, "- **{}** {word} committed", outcome.changes);
    if let Some(id) = outcome.change_id.as_deref() {
        outln!(w, "- change id `{short}`", short = short_change_id(id));
    }
    if !outcome.hooks_run.is_empty() {
        outln!(w, "- Hooks: `{}`", outcome.hooks_run.join("`, `"));
    }
    Ok(())
}

/// Result of `grit merge` (and the merge half of `grit pull`).
pub fn print_merge_outcome(w: &mut dyn Write, outcome: &MergeOutcome) -> anyhow::Result<()> {
    outln!(w, "## Merge");
    outln!(w);
    match outcome.result {
        MergeResult::UpToDate => {
            outln!(w, "Already up to date with **`{}`**.", outcome.branch);
        }
        MergeResult::FastForward if outcome.check => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(
                w,
                "- Would fast-forward **`{}`** → `{short}`",
                outcome.branch
            );
        }
        MergeResult::FastForward => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(w, "- Fast-forwarded **`{}`** → `{short}`", outcome.branch);
            if let Some(id) = outcome.change_id.as_deref() {
                outln!(w, "- change id `{short}`", short = short_change_id(id));
            }
        }
        MergeResult::Clean => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(
                w,
                "- Would merge **`{}`** cleanly · tree `{short}`",
                outcome.branch
            );
        }
        MergeResult::Merged => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(
                w,
                "- Merged **`{}`** into the current branch · `{short}`",
                outcome.branch
            );
            if let Some(id) = outcome.change_id.as_deref() {
                outln!(w, "- change id `{short}`", short = short_change_id(id));
            }
        }
        MergeResult::Conflicts if outcome.check => {
            if let Some(details) = &outcome.conflicts {
                crate::operation_output::render_merge_check_conflicts_markdown(
                    &outcome.branch,
                    details,
                    w,
                )?;
            }
        }
        MergeResult::Conflicts => {
            outln!(w, "- Merge **`{}`** stopped with conflicts", outcome.branch);
            outln!(w);
            if let Some(details) = &outcome.conflicts {
                crate::operation_output::render_conflicts_markdown(details, w)?;
            }
            if outcome.next_steps.is_some() || outcome.alternatives.is_some() {
                crate::operation_output::render_operation_guidance_markdown(
                    outcome.next_steps.as_deref().unwrap_or(&[]),
                    outcome.alternatives.as_deref(),
                    w,
                )?;
            } else {
                outln!(w);
                outln!(
                    w,
                    "→ fix conflicts, then `grit merge --continue`, or `grit merge --abort`"
                );
            }
        }
        MergeResult::Aborted => outln!(w, "Merge aborted."),
        MergeResult::SetUpstream => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            let upstream = outcome.upstream.as_deref().unwrap_or("");
            outln!(
                w,
                "- Set **`{}`** to `{upstream}` · `{short}`",
                outcome.branch
            );
        }
    }
    Ok(())
}

/// Result of `grit pull`.
pub fn print_pull_outcome(
    w: &mut dyn Write,
    outcome: &crate::commands::pull::PullOutcome,
) -> anyhow::Result<()> {
    use crate::commands::pull::PullResult;
    outln!(w, "## Pull");
    outln!(w);
    match outcome.result {
        PullResult::UpToDate => {
            outln!(w, "Already up to date with **`{}`**.", outcome.branch);
        }
        PullResult::FastForward => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(w, "- Fast-forwarded **`{}`** → `{short}`", outcome.branch);
            if let Some(id) = outcome.change_id.as_deref() {
                outln!(w, "- change id `{short}`", short = short_change_id(id));
            }
        }
        PullResult::Merged => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(
                w,
                "- Merged **`{}`** into the current branch · `{short}`",
                outcome.branch
            );
            if let Some(id) = outcome.change_id.as_deref() {
                outln!(w, "- change id `{short}`", short = short_change_id(id));
            }
        }
        PullResult::RefusedNotFf => {
            let ahead = outcome.ahead.unwrap_or(0);
            let behind = outcome.behind.unwrap_or(0);
            outln!(
                w,
                "- Not possible to fast-forward **`{}`** · {ahead} ahead, {behind} behind",
                outcome.branch
            );
        }
        PullResult::Conflicts => {
            outln!(w, "- Pull **`{}`** stopped with conflicts", outcome.branch);
            outln!(w);
            if let Some(details) = &outcome.conflict_details {
                for item in details {
                    if let Some(summary) = &item.summary {
                        outln!(w, "- **{}** — {summary}", item.kind);
                    } else {
                        outln!(w, "- **{}** — `{path}`", item.kind, path = item.path);
                    }
                }
            } else {
                for path in outcome.conflicts.as_deref().unwrap_or(&[]) {
                    outln!(w, "- `{path}`");
                }
            }
            if outcome.oid.is_some() {
                outln!(
                    w,
                    "→ rebase stopped with conflicts; the branch, index, and worktree are unchanged"
                );
            } else {
                outln!(
                    w,
                    "→ fix conflicts, then `grit commit \"message\"`, or `grit merge --abort`"
                );
            }
        }
        PullResult::SetUpstream => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            let upstream = outcome.upstream.as_deref().unwrap_or("");
            outln!(
                w,
                "- Set **`{}`** to `{upstream}` · `{short}`",
                outcome.branch
            );
        }
        PullResult::Rebased => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(w, "- Rebased onto **`{}`** · `{short}`", outcome.branch);
            if let Some(id) = outcome.change_id.as_deref() {
                outln!(w, "- change id `{short}`", short = short_change_id(id));
            }
        }
    }
    Ok(())
}

/// Result of `grit pick`.
pub fn print_pick_outcome(w: &mut dyn Write, outcome: &PickOutcome) -> anyhow::Result<()> {
    outln!(w, "## Pick");
    outln!(w);
    let src = short_hex(&outcome.source);
    let new = short_hex(&outcome.oid);
    outln!(w, "- `{src}` → `{new}` — {}", outcome.subject);
    if let Some(id) = outcome.change_id.as_deref() {
        outln!(w, "- change id `{short}`", short = short_change_id(id));
    }
    Ok(())
}

/// Result of `grit revert`.
pub fn print_revert_outcome(w: &mut dyn Write, outcome: &RevertOutcome) -> anyhow::Result<()> {
    use crate::commands::revert::RevertResult;
    outln!(w, "## Revert");
    outln!(w);
    match outcome.result {
        RevertResult::Aborted => outln!(w, "Revert aborted."),
        RevertResult::Conflicts => {
            outln!(w, "- Revert stopped with conflicts");
            outln!(w);
            if let Some(details) = &outcome.conflicts {
                crate::operation_output::render_conflicts_markdown(details, w)?;
            }
            crate::operation_output::render_operation_guidance_markdown(
                outcome.next_steps.as_deref().unwrap_or(&[]),
                outcome.alternatives.as_deref(),
                w,
            )?;
        }
        RevertResult::Empty => {
            outln!(w, "- Revert stopped on an empty commit");
            crate::operation_output::render_operation_guidance_markdown(
                outcome.next_steps.as_deref().unwrap_or(&[]),
                outcome.alternatives.as_deref(),
                w,
            )?;
        }
        RevertResult::Completed => {
            if outcome.dry_run == Some(true) {
                outln!(w, "- Would revert {} commit(s)", outcome.commits.len());
                for entry in &outcome.commits {
                    let src = short_hex(&entry.source);
                    outln!(w, "  - `{src}`");
                }
            } else {
                for entry in &outcome.commits {
                    let src = short_hex(&entry.source);
                    let new = short_hex(&entry.oid);
                    outln!(w, "- `{src}` → `{new}` — {}", entry.subject);
                    if let Some(id) = entry.change_id.as_deref() {
                        outln!(w, "- change id `{short}`", short = short_change_id(id));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Result of `grit history drop`, `move`, `reword`, `fixup` or `squash`.
pub fn print_history_rewrite_outcome(
    w: &mut dyn Write,
    title: &str,
    outcome: &HistoryRewriteOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## {title}");
    outln!(w);
    if outcome.dry_run {
        outln!(w, "- Dry run (refs unchanged)");
    }
    print_history_rewrite_rows(w, &outcome.rewritten, &outcome.refs, outcome.head.as_ref())
}

fn print_history_rewrite_rows(
    w: &mut dyn Write,
    rewritten: &[RewrittenPair],
    refs: &[RefPair],
    head: Option<&HeadPair>,
) -> anyhow::Result<()> {
    for pair in rewritten {
        match &pair.new {
            Some(new) => {
                outln!(
                    w,
                    "- commit `{}` → `{}`",
                    short_hex(&pair.old),
                    short_hex(new)
                );
                if let Some(id) = pair.change_id.as_deref() {
                    outln!(w, "  - change id `{short}`", short = short_change_id(id));
                }
            }
            None => outln!(w, "- commit `{}` → (dropped)", short_hex(&pair.old)),
        }
    }
    for update in refs {
        outln!(
            w,
            "- `{}` `{}` → `{}`",
            update.name,
            short_hex(&update.old),
            short_hex(&update.new)
        );
    }
    if let Some(head) = head {
        outln!(
            w,
            "- HEAD `{}` → `{}`",
            short_hex(&head.old),
            short_hex(&head.new)
        );
    }
    Ok(())
}

/// Result of `grit history absorb`.
pub fn print_history_absorb_outcome(
    w: &mut dyn Write,
    outcome: &HistoryAbsorbOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## History absorb");
    outln!(w);
    if outcome.dry_run {
        outln!(w, "- Dry run (refs unchanged)");
    }
    for row in &outcome.assignments {
        outln!(
            w,
            "- assign `{}` hunk @{} → commit `{}`",
            row.path,
            row.hunk_range.old_start,
            short_hex(&row.commit)
        );
    }
    for row in &outcome.unassigned {
        outln!(w, "- unassigned `{}` ({})", row.path, row.reason);
    }
    print_history_rewrite_rows(w, &outcome.rewritten, &outcome.refs, outcome.head.as_ref())
}

/// Result of `grit switch`.
pub fn print_switch_outcome(w: &mut dyn Write, outcome: &SwitchOutcome) -> anyhow::Result<()> {
    outln!(w, "## Switch");
    outln!(w);
    if outcome.dry_run {
        let target = if outcome.detached {
            outcome.commit.as_deref().map(short_hex).unwrap_or("HEAD")
        } else {
            outcome.branch.as_str()
        };
        outln!(
            w,
            "- Would switch to **`{target}`** ({} path(s) would change)",
            outcome.changed_paths.unwrap_or(0)
        );
        return Ok(());
    }
    if outcome.detached {
        let short = outcome.commit.as_deref().map(short_hex).unwrap_or("HEAD");
        outln!(w, "- HEAD detached at `{short}`");
    } else if outcome.created {
        outln!(w, "- Created and switched to **`{}`**", outcome.branch);
    } else {
        outln!(w, "- Switched to **`{}`**", outcome.branch);
    }
    Ok(())
}

/// Result of `grit stash` / `grit stash push`.
pub fn print_stash_push_outcome(
    w: &mut dyn Write,
    outcome: &StashPushOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Stash");
    outln!(w);
    match outcome {
        StashPushOutcome::Nothing { .. } => outln!(w, "No local changes to save."),
        StashPushOutcome::Saved { oid, message } => {
            let short = short_hex(oid);
            if message.is_empty() {
                outln!(w, "- Saved working tree and index state · `{short}`");
            } else {
                outln!(w, "- Saved **{message}** · `{short}`");
            }
        }
    }
    Ok(())
}

/// Result of `grit stash apply`.
pub fn print_stash_apply_outcome(
    w: &mut dyn Write,
    outcome: &StashApplyOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Stash apply");
    outln!(w);
    if outcome.conflicts {
        outln!(
            w,
            "- **stash@{{{}}}** · stopped with conflicts",
            outcome.index
        );
        for path in &outcome.conflict_paths {
            outln!(w, "  - `{path}`");
        }
    } else {
        outln!(w, "- Applied **stash@{{{}}}**", outcome.index);
    }
    Ok(())
}

/// Result of `grit stash pop`.
pub fn print_stash_pop_outcome(w: &mut dyn Write, outcome: &StashPopOutcome) -> anyhow::Result<()> {
    outln!(w, "## Stash pop");
    outln!(w);
    if outcome.conflicts {
        outln!(
            w,
            "- **stash@{{{}}}** · stopped with conflicts (entry kept)",
            outcome.index
        );
        for path in &outcome.conflict_paths {
            outln!(w, "  - `{path}`");
        }
    } else {
        let note = if outcome.dropped { "applied" } else { "empty" };
        outln!(w, "- Dropped **stash@{{{}}}** ({note})", outcome.index);
    }
    Ok(())
}

/// Result of `grit stash drop`.
pub fn print_stash_drop_outcome(
    w: &mut dyn Write,
    outcome: &StashDropOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Stash drop");
    outln!(w);
    outln!(w, "- Dropped **stash@{{{}}}**", outcome.index);
    Ok(())
}

/// Result of `grit worktree` (list / add / remove / prune).
pub fn print_worktree_outcome(w: &mut dyn Write, outcome: &WorktreeOutcome) -> anyhow::Result<()> {
    match outcome {
        WorktreeOutcome::List { worktrees } => {
            outln!(w, "## Worktrees");
            outln!(w);
            print_worktree_table(w, worktrees)?;
        }
        WorktreeOutcome::Add { path, branch, head } => {
            outln!(w, "## Worktree add");
            outln!(w);
            let short = short_hex(head);
            if let Some(b) = branch {
                outln!(w, "- Created **`{path}`** on branch **`{b}`** · `{short}`");
            } else {
                outln!(w, "- Created **`{path}`** (detached) · `{short}`");
            }
        }
        WorktreeOutcome::Remove { path } => {
            outln!(w, "## Worktree remove");
            outln!(w);
            outln!(w, "- Removed **`{path}`**");
        }
        WorktreeOutcome::Prune { pruned } => {
            outln!(w, "## Worktree prune");
            outln!(w);
            if pruned.is_empty() {
                outln!(w, "No prunable worktrees.");
            } else {
                for path in pruned {
                    outln!(w, "- Pruned **`{path}`**");
                }
            }
        }
        WorktreeOutcome::Lock { path, reason } => {
            outln!(w, "## Worktree lock");
            outln!(w);
            if let Some(r) = reason {
                outln!(w, "- Locked **`{path}`** · {r}");
            } else {
                outln!(w, "- Locked **`{path}`**");
            }
        }
        WorktreeOutcome::Unlock { path } => {
            outln!(w, "## Worktree unlock");
            outln!(w);
            outln!(w, "- Unlocked **`{path}`**");
        }
    }
    Ok(())
}

fn print_worktree_table(w: &mut dyn Write, worktrees: &[WorktreeRow]) -> anyhow::Result<()> {
    if worktrees.is_empty() {
        outln!(w, "No worktrees.");
        return Ok(());
    }
    outln!(w, "| Path | Branch | HEAD |");
    outln!(w, "| --- | --- | --- |");
    for wt in worktrees {
        let branch = wt
            .branch
            .as_deref()
            .map(|b| format!("`{}`", escape_table_cell(b)))
            .unwrap_or_else(|| {
                if wt.detached {
                    "_(detached)_".into()
                } else {
                    "—".into()
                }
            });
        let head = if wt.head.is_empty() {
            "—".to_owned()
        } else {
            format!("`{}`", short_hex(&wt.head))
        };
        outln!(
            w,
            "| {} | {} | {} |",
            escape_table_cell(&wt.path),
            branch,
            head
        );
    }
    Ok(())
}

/// Result of `grit auth` sign-in.
pub fn print_auth_outcome(w: &mut dyn Write, outcome: &AuthOutcome) -> anyhow::Result<()> {
    outln!(w, "## Auth");
    outln!(w);
    if outcome.authenticated {
        outln!(w, "- Signed in to **`{}`**", outcome.host);
    } else {
        outln!(w, "- Not signed in to **`{}`**", outcome.host);
    }
    Ok(())
}

/// Result of `grit auth logout`.
pub fn print_logout_outcome(w: &mut dyn Write, outcome: &LogoutOutcome) -> anyhow::Result<()> {
    outln!(w, "## Auth logout");
    outln!(w);
    if outcome.logged_out {
        outln!(
            w,
            "- Signed out of **`{}`** (stored token removed)",
            outcome.host
        );
    } else {
        outln!(
            w,
            "- No credential helper configured for **`{}`**",
            outcome.host
        );
    }
    Ok(())
}

/// Result of `grit update`.
pub fn print_update_outcome(w: &mut dyn Write, outcome: &UpdateOutcome) -> anyhow::Result<()> {
    outln!(w, "## Update");
    outln!(w);
    if outcome.updated {
        outln!(
            w,
            "- Updated grit (ran installer; previous version **{}**)",
            outcome.version
        );
    } else {
        outln!(w, "- Update did not run (version **{}**)", outcome.version);
    }
    Ok(())
}

/// Result of `grit add`.
pub fn print_add_outcome(w: &mut dyn Write, outcome: &AddOutcome) -> anyhow::Result<()> {
    outln!(w, "## Add");
    outln!(w);
    let verb = if outcome.dry_run {
        "Would stage"
    } else {
        "Staged"
    };
    if outcome.count == 0 && outcome.no_paths {
        outln!(w, "Nothing to stage — working tree clean.");
        return Ok(());
    }
    if outcome.count == 0 {
        return Ok(());
    }
    outln!(w, "- {verb} **{}** change(s)", outcome.count);
    for path in outcome
        .added
        .iter()
        .chain(&outcome.modified)
        .chain(&outcome.removed)
        .chain(&outcome.intent_to_add)
        .take(10)
    {
        outln!(w, "  - `{path}`");
    }
    let listed = outcome.added.len()
        + outcome.modified.len()
        + outcome.removed.len()
        + outcome.intent_to_add.len();
    if listed > 10 {
        outln!(w, "  - … and {} more", listed - 10);
    }
    Ok(())
}

/// Per-ref push results.
pub fn print_push_results(
    w: &mut dyn Write,
    remote: &str,
    branch: &str,
    dry_run: bool,
    results: &[crate::commands::push::PushRefResult],
) -> anyhow::Result<()> {
    if results.is_empty() {
        outln!(w, "No refs pushed to `{remote}`.");
        return Ok(());
    }
    outln!(w, "## Push to `{remote}`");
    for result in results {
        let target = format!("{remote} {}", result.ref_name);
        match result.status {
            crate::commands::push::PushRefOutcomeStatus::Ok => {
                if dry_run {
                    outln!(
                        w,
                        "- **`{}`** · would push `{branch}` → `{target}`",
                        result.ref_name
                    );
                } else {
                    outln!(
                        w,
                        "- **`{}`** · pushed `{branch}` → `{target}`",
                        result.ref_name
                    );
                }
            }
            crate::commands::push::PushRefOutcomeStatus::UpToDate => outln!(
                w,
                "- **`{}`** · `{target}` already up to date",
                result.ref_name
            ),
            crate::commands::push::PushRefOutcomeStatus::Rejected => {
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
    use super::{diff_code_fence, escape_table_cell, format_inline_code};

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
    fn format_inline_code_uses_longer_delimiter_for_embedded_backtick() {
        assert_eq!(format_inline_code("tick`name.txt"), "``tick`name.txt``");
        assert_eq!(format_inline_code("plain.txt"), "`plain.txt`");
    }

    #[test]
    fn format_inline_code_pads_when_content_touches_delimiter() {
        assert_eq!(format_inline_code("`edge"), "`` `edge ``");
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

    #[test]
    fn worktree_list_table_escapes_pipe_in_branch_name() {
        use crate::commands::worktree::{WorktreeOutcome, WorktreeRow};

        let outcome = WorktreeOutcome::List {
            worktrees: vec![WorktreeRow {
                path: "/repo/linked-wt".into(),
                head: "b5ea30de417ac84c09b8cfc0acf5f2c39efc611f".into(),
                branch: Some("wt|branch".into()),
                detached: false,
                bare: false,
                locked: false,
                lock_reason: None,
                prunable: false,
            }],
        };
        let mut buf = Vec::new();
        super::print_worktree_outcome(&mut buf, &outcome).expect("markdown");
        let rendered = String::from_utf8(buf).expect("utf-8");
        let row = rendered
            .lines()
            .find(|line| line.contains("linked-wt"))
            .expect("worktree row");
        assert!(
            row.contains("`wt\\|branch`"),
            "branch cell should escape pipe delimiters: {row}"
        );
    }
}
