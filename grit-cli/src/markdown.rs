//! Shared Markdown formatters for `--markdown` command output.

use std::io::Write;

use grit_lib::diff::{DiffEntry, DiffStatus};

use crate::commands::add::AddOutcome;
use crate::commands::auth::{AuthOutcome, LogoutOutcome};
use crate::commands::commit::CommitOutcome;
use crate::commands::diff::{file_diff_status_label, FileDiff, Hunk, Line, LineKind, Segment};
use crate::commands::history::{
    HeadPair, HistoryAbsorbOutcome, HistoryRewriteOutcome, RebaseCheckOutcome, RebaseCheckResult,
    RefPair, RewrittenPair,
};
use crate::commands::merge::{MergeOutcome, MergeResult};
use crate::commands::pick::PickOutcome;
use crate::commands::revert::RevertOutcome;
use crate::commands::serve_http::{
    ServeGrantOutcome, ServeTokenOutcome, ServeUserOutcome, ServeUsersOutcome,
};
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

/// Render one file diff as Markdown (optional blank line before the header).
pub fn print_diff_file(
    w: &mut dyn Write,
    file: &FileDiff,
    path_opts: Option<&PathDisplayOptions>,
    separator_before: bool,
) -> anyhow::Result<()> {
    if separator_before {
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
        return Ok(());
    }
    let patch = render_file_patch(file);
    if !patch.is_empty() {
        outln!(w);
        print_fenced_diff(w, &patch)?;
    }
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
        print_diff_file(w, file, path_opts, i > 0)?;
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
    code_fence(body, "diff")
}

/// Opening (with `lang`) and closing fence longer than any run of backticks in `body`.
fn code_fence(body: &str, lang: &str) -> (String, String) {
    let mut ticks = 3usize;
    loop {
        let marker = "`".repeat(ticks);
        if !body.contains(&marker) {
            return (format!("{marker}{lang}"), marker);
        }
        ticks += 1;
    }
}

/// Render captured hook output as labelled `text` code blocks; empty streams are skipped.
pub fn render_hook_output(w: &mut dyn Write, stdout: &str, stderr: &str) -> anyhow::Result<()> {
    for (label, body) in [("stdout", stdout), ("stderr", stderr)] {
        if body.is_empty() {
            continue;
        }
        let body = body.strip_suffix('\n').unwrap_or(body);
        let (open, close) = code_fence(body, "text");
        outln!(w, "\n**Hook {label}:**\n");
        outln!(w, "{open}");
        stdio::io_result(writeln!(w, "{body}"))
            .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
        outln!(w, "{close}");
    }
    Ok(())
}

fn print_fenced_diff(w: &mut dyn Write, body: &str) -> anyhow::Result<()> {
    let (open, close) = diff_code_fence(body);
    outln!(w, "{open}");
    stdio::io_result(writeln!(w, "{body}")).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    outln!(w, "{close}");
    Ok(())
}

/// Git-style extended header lines (`rename from`/`rename to`, `old mode`/`new mode`)
/// for a file whose path or mode changed. Empty for a plain content change.
fn extended_header_lines(file: &FileDiff) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(old_path) = file.old_path.as_deref() {
        lines.push(format!("rename from {old_path}"));
        lines.push(format!("rename to {}", file.path));
    }
    if let (Some(old_mode), Some(new_mode)) = (file.old_mode.as_deref(), file.new_mode.as_deref()) {
        if old_mode != new_mode {
            lines.push(format!("old mode {old_mode}"));
            lines.push(format!("new mode {new_mode}"));
        }
    }
    lines
}

fn render_file_patch(file: &FileDiff) -> String {
    let mut out = String::new();
    for line in extended_header_lines(file) {
        out.push_str(&line);
        out.push('\n');
    }
    for (i, hunk) in file.hunks.as_deref().unwrap_or_default().iter().enumerate() {
        if i > 0 {
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

/// Branch list as a table with upstream and worktree columns when present.
pub fn print_branch_list(
    w: &mut dyn Write,
    branches: &[crate::commands::branch::BranchEntry],
    scope: crate::commands::branch::BranchListScope,
) -> anyhow::Result<()> {
    if branches.is_empty() {
        outln!(w, "{}", crate::commands::branch::empty_list_message(scope));
        return Ok(());
    }
    outln!(w, "## Branches");
    outln!(
        w,
        "| Branch | Current | OID | Subject | Upstream | Worktree |"
    );
    outln!(w, "| --- | --- | --- | --- | --- | --- |");
    for branch in branches {
        let short_oid = branch
            .short_oid
            .as_deref()
            .or(branch.oid.as_deref())
            .map(|oid| oid.get(..7).unwrap_or(oid))
            .unwrap_or("");
        let current = if branch.current { "yes" } else { "" };
        let subject = branch
            .subject
            .as_deref()
            .map(escape_table_cell)
            .unwrap_or_default();
        let upstream = branch
            .upstream
            .as_ref()
            .map(|up| {
                let label = if up.gone {
                    format!("{} [gone]", up.ref_name)
                } else {
                    match (up.ahead, up.behind) {
                        (Some(a), Some(b)) => format!("{} (+{a}/-{b})", up.ref_name),
                        _ => up.ref_name.clone(),
                    }
                };
                escape_table_cell(&label)
            })
            .unwrap_or_default();
        let worktree = branch
            .worktree
            .as_deref()
            .map(escape_table_cell)
            .unwrap_or_default();
        outln!(
            w,
            "| `{}` | {} | `{}` | {} | {} | {} |",
            escape_table_cell(&branch.name),
            current,
            short_oid,
            subject,
            upstream,
            worktree
        );
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
        let subject = tag
            .annotation
            .as_ref()
            .map(|a| format!(" — {}", a.subject))
            .unwrap_or_default();
        outln!(
            w,
            "- `{}` · {} `{short}`{peel}{subject}",
            tag.name,
            tag.kind
        );
    }
    Ok(())
}

/// Fetch ref updates as bullets (`ref` · old → new).
pub fn print_fetch_updates(
    w: &mut dyn Write,
    remote: &str,
    updates: &[crate::commands::fetch::FetchUpdate],
    updated: usize,
    dry_run: bool,
) -> anyhow::Result<()> {
    use crate::commands::fetch::FetchUpdateStatus;
    let pending = updates
        .iter()
        .filter(|u| u.status != FetchUpdateStatus::UpToDate)
        .count();
    let summary_count = if dry_run { pending } else { updated };
    if updates.is_empty() && summary_count == 0 {
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
    } else if dry_run && pending > 0 {
        outln!(w);
        outln!(
            w,
            "Would fetch **{pending}** update{} from `{remote}`.",
            if pending == 1 { "" } else { "s" }
        );
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

/// First seven hex digits of `oid` (all of it when shorter).
pub(crate) fn short_hex(oid: &str) -> &str {
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
            outcome.branch_label()
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
            outcome.branch_label(),
            outcome.subject
        );
    } else {
        outln!(
            w,
            "- **`{}`** · `{short}` — {}",
            outcome.branch_label(),
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

fn write_merge_fast_forward_markdown(
    w: &mut dyn Write,
    outcome: &MergeOutcome,
    would: bool,
) -> anyhow::Result<()> {
    let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
    let head = outcome
        .updated_branch
        .as_deref()
        .unwrap_or(outcome.branch.as_str());
    let source = outcome
        .updated_branch
        .as_ref()
        .map(|_| outcome.branch.as_str());
    match (would, source) {
        (true, Some(src)) => outln!(
            w,
            "- Would fast-forward **`{head}`** → `{short}` (**`{src}`**)"
        ),
        (true, None) => outln!(w, "- Would fast-forward **`{head}`** → `{short}`"),
        (false, Some(src)) => outln!(w, "- Fast-forwarded **`{head}`** → `{short}` (**`{src}`**)"),
        (false, None) => outln!(w, "- Fast-forwarded **`{head}`** → `{short}`"),
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
            write_merge_fast_forward_markdown(w, outcome, true)?;
        }
        MergeResult::FastForward => {
            write_merge_fast_forward_markdown(w, outcome, false)?;
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
        MergeResult::Squashed => {
            outln!(
                w,
                "- Squash merge from **`{}`** applied to the index (see `SQUASH_MSG`)",
                outcome.branch
            );
        }
        MergeResult::Stopped => {
            outln!(
                w,
                "- Merge from **`{}`** applied; finish with `grit commit` or `grit merge --abort`",
                outcome.branch
            );
        }
        MergeResult::RefusedNotFf => {
            outln!(
                w,
                "- **`--ff-only`**: cannot fast-forward **`{}`** (histories diverged)",
                outcome.branch
            );
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
    }
    if let Some(steps) = &outcome.next_steps {
        let already_rendered = matches!(outcome.result, MergeResult::Conflicts) && !outcome.check;
        if !already_rendered {
            outln!(w);
            for step in steps {
                outln!(w, "- `{step}`");
            }
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
    if let Some(check) = &outcome.check {
        outln!(
            w,
            "- Check: {ahead} ahead, {behind} behind · fast-forward {} · would `{would:?}`",
            if check.fast_forward_possible {
                "possible"
            } else {
                "not possible"
            },
            ahead = check.ahead,
            behind = check.behind,
            would = check.would
        );
        return Ok(());
    }
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
            if outcome.operation.as_deref() == Some("rebase") {
                if let Some(commit) = &outcome.commit {
                    outln!(
                        w,
                        "- Rebase stopped at commit `{short}`",
                        short = short_hex(commit)
                    );
                }
                if let Some(stash) = &outcome.autostash {
                    outln!(
                        w,
                        "- Local changes kept in the stash (`{short}`); `grit stash pop` after the rebase",
                        short = short_hex(stash)
                    );
                }
                crate::operation_output::render_operation_guidance_markdown(
                    outcome.next_steps.as_deref().unwrap_or(&[]),
                    outcome.alternatives.as_deref(),
                    w,
                )?;
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
        PullResult::AutostashConflict => {
            outln!(
                w,
                "- Pulled **`{}`**, but re-applying autostash left conflicts (stash kept)",
                outcome.branch
            );
        }
    }
    Ok(())
}

/// Result of `grit pick`.
pub fn print_pick_outcome(w: &mut dyn Write, outcome: &PickOutcome) -> anyhow::Result<()> {
    use crate::commands::pick::PickResult;
    outln!(w, "## Pick");
    outln!(w);
    match outcome.result {
        PickResult::Picked | PickResult::Skipped => {
            if outcome.dry_run == Some(true) {
                outln!(w, "- Would pick **{}** commit(s)", outcome.commits.len());
                for row in &outcome.commits {
                    let src = short_hex(&row.source);
                    outln!(w, "  - `{src}`");
                }
            } else {
                for row in &outcome.commits {
                    let src = short_hex(&row.source);
                    if let Some(oid) = &row.oid {
                        let new = short_hex(oid);
                        outln!(w, "- `{src}` → `{new}` — {}", row.subject);
                    } else {
                        outln!(w, "- `{src}` — {} (no commit)", row.subject);
                    }
                }
                if !outcome.dropped.is_empty() {
                    outln!(w, "- Dropped **{}** empty commit(s)", outcome.dropped.len());
                }
            }
        }
        PickResult::Conflicts => {
            if outcome.check {
                outln!(w, "- Would conflict during pick");
                if let Some(details) = &outcome.conflicts {
                    for item in details {
                        outln!(w, "  - `{path}`", path = item.path);
                    }
                }
            } else {
                outln!(w, "- Pick stopped with conflicts");
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
        }
        PickResult::Clean => {
            outln!(w, "- Would pick cleanly");
            if let Some(oid) = &outcome.oid {
                outln!(w, "  - tree `{oid}`");
            }
        }
        PickResult::Empty => {
            if outcome.check {
                outln!(w, "- Would stop on an empty pick step");
            } else if let Some(row) = outcome.commits.last() {
                let src = short_hex(&row.source);
                outln!(w, "- Stopped on empty commit `{src}` — {}", row.subject);
                crate::operation_output::render_operation_guidance_markdown(
                    outcome.next_steps.as_deref().unwrap_or(&[]),
                    outcome.alternatives.as_deref(),
                    w,
                )?;
            } else {
                outln!(w, "- Pick stopped on an empty commit");
                crate::operation_output::render_operation_guidance_markdown(
                    outcome.next_steps.as_deref().unwrap_or(&[]),
                    outcome.alternatives.as_deref(),
                    w,
                )?;
            }
        }
        PickResult::Aborted => outln!(w, "Pick aborted."),
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
            if outcome.check {
                outln!(w, "- Would conflict during revert");
                if let Some(details) = &outcome.conflicts {
                    for item in details {
                        outln!(w, "  - `{path}`", path = item.path);
                    }
                }
            } else {
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
        }
        RevertResult::Clean => {
            outln!(w, "- Would revert cleanly");
            if let Some(oid) = &outcome.oid {
                outln!(w, "  - tree `{oid}`");
            }
        }
        RevertResult::Empty => {
            if outcome.check {
                outln!(w, "- Would stop on an empty revert step");
            } else {
                outln!(w, "- Revert stopped on an empty commit");
                crate::operation_output::render_operation_guidance_markdown(
                    outcome.next_steps.as_deref().unwrap_or(&[]),
                    outcome.alternatives.as_deref(),
                    w,
                )?;
            }
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

/// Result of `grit rebase`.
pub fn print_rebase_outcome(
    w: &mut dyn Write,
    outcome: &crate::commands::history::RebaseCliOutcome,
) -> anyhow::Result<()> {
    use crate::commands::history::RebaseResult;
    outln!(w, "## Rebase");
    outln!(w);
    match outcome.result {
        RebaseResult::Aborted => outln!(w, "Rebase aborted."),
        RebaseResult::ExecDirty => {
            outln!(
                w,
                "- Rebase stopped: exec left tracked changes; edits are preserved."
            );
            outln!(
                w,
                "Save edits before continuing or skipping. Aborting discards exec edits."
            );
            for path in &outcome.paths {
                outln!(w, "- `{path}`");
            }
            crate::operation_output::render_operation_guidance_markdown(
                outcome.next_steps.as_deref().unwrap_or(&[]),
                outcome.alternatives.as_deref(),
                w,
            )?;
        }
        RebaseResult::Conflicts => {
            outln!(w, "- Rebase stopped with conflicts");
            outln!(w);
            if let Some(details) = &outcome.conflicts {
                crate::operation_output::render_conflicts_markdown(details, w)?;
            }
            if let Some(commit) = &outcome.commit {
                outln!(
                    w,
                    "- Stopped at commit `{short}`",
                    short = short_hex(commit)
                );
            }
            crate::operation_output::render_operation_guidance_markdown(
                outcome.next_steps.as_deref().unwrap_or(&[]),
                outcome.alternatives.as_deref(),
                w,
            )?;
        }
        RebaseResult::Completed => {
            if outcome.dry_run {
                outln!(w, "- Dry run (refs unchanged)");
            }
            if outcome.rewritten.is_empty() {
                outln!(w, "- Already up to date");
            } else {
                print_history_rewrite_rows(
                    w,
                    &outcome.rewritten,
                    &outcome.refs,
                    outcome.head.as_ref(),
                )?;
            }
        }
    }
    Ok(())
}

/// Result of `grit rebase --check`.
pub fn print_rebase_check_outcome(
    w: &mut dyn Write,
    outcome: &RebaseCheckOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Rebase check");
    outln!(w);
    match outcome.result {
        RebaseCheckResult::UpToDate => outln!(w, "- Already up to date"),
        RebaseCheckResult::Clean => {
            let n = outcome.commits.as_ref().map_or(0, Vec::len);
            outln!(w, "- Would rebase {n} commit(s) cleanly");
        }
        RebaseCheckResult::Conflicts => {
            outln!(w, "- Would conflict during rebase");
            if let Some(conflicts) = &outcome.conflicts {
                for c in conflicts {
                    outln!(w, "- `{}` ({})", c.path, c.kind);
                }
            }
        }
    }
    Ok(())
}

pub fn print_history_rewrite_outcome(
    w: &mut dyn Write,
    title: &str,
    outcome: &HistoryRewriteOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## {title}");
    outln!(w);
    if outcome.operation == "rebase" && outcome.rewritten.is_empty() {
        outln!(w, "- Already up to date");
        return Ok(());
    }
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
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(
                        w,
                        "- commit `{}` → `{}` — {subject}",
                        short_hex(&pair.old),
                        short_hex(new)
                    );
                } else {
                    outln!(
                        w,
                        "- commit `{}` → `{}`",
                        short_hex(&pair.old),
                        short_hex(new)
                    );
                }
                if let Some(id) = pair.change_id.as_deref() {
                    outln!(w, "  - change id `{short}`", short = short_change_id(id));
                }
            }
            None => {
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(
                        w,
                        "- commit `{}` → (dropped) — {subject}",
                        short_hex(&pair.old)
                    );
                } else {
                    outln!(w, "- commit `{}` → (dropped)", short_hex(&pair.old));
                }
            }
        }
    }
    for update in refs {
        outln!(
            w,
            "- `{}` `{}` → `{}`",
            update.ref_name,
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

fn absorb_hunk_header(range: &crate::commands::history::HunkRangeOut) -> String {
    use grit_lib::revwalk::history::{format_hunk_range_header, HunkRange};
    format_hunk_range_header(&HunkRange {
        old_start: range.old_start,
        old_lines: range.old_lines,
        new_start: range.new_start,
        new_lines: range.new_lines,
    })
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
        outln!(w);
    }
    if !outcome.assignments.is_empty() {
        outln!(w, "### Assigned hunks");
        outln!(w);
        outln!(w, "| Path | Hunk | Commit | Subject |");
        outln!(w, "| --- | --- | --- | --- |");
        for row in &outcome.assignments {
            outln!(
                w,
                "| `{}` | `{}` | `{}` | {} |",
                escape_table_cell(&row.path),
                absorb_hunk_header(&row.hunk_range),
                short_hex(&row.commit),
                escape_table_cell(&row.subject)
            );
        }
        outln!(w);
    }
    if !outcome.unassigned.is_empty() {
        outln!(w, "### Unassigned hunks");
        outln!(w);
        outln!(w, "| Path | Hunk | Reason |");
        outln!(w, "| --- | --- | --- |");
        for row in &outcome.unassigned {
            outln!(
                w,
                "| `{}` | `{}` | {} |",
                escape_table_cell(&row.path),
                absorb_hunk_header(&row.hunk_range),
                escape_table_cell(&row.reason)
            );
        }
        outln!(w);
    }
    print_history_rewrite_rows(w, &outcome.rewritten, &outcome.refs, outcome.head.as_ref())
}

/// Result of `grit switch`.
pub fn print_switch_outcome(w: &mut dyn Write, outcome: &SwitchOutcome) -> anyhow::Result<()> {
    outln!(w, "## Switch");
    outln!(w);
    if outcome.dry_run {
        outln!(
            w,
            "- Would switch to **`{}`** ({} path(s) would change)",
            outcome.target_label(),
            outcome.changed_paths.unwrap_or(0)
        );
        return print_switch_stranded(w, outcome);
    }
    match outcome.branch.as_deref() {
        None => outln!(w, "- HEAD detached at `{}`", outcome.target_label()),
        Some(branch) if outcome.created => {
            outln!(w, "- Created and switched to **`{branch}`**")
        }
        Some(branch) => outln!(w, "- Switched to **`{branch}`**"),
    }
    if !outcome.hooks_run.is_empty() {
        outln!(w, "- Hooks: `{}`", outcome.hooks_run.join("`, `"));
    }
    print_switch_stranded(w, outcome)
}

fn print_switch_stranded(w: &mut dyn Write, outcome: &SwitchOutcome) -> anyhow::Result<()> {
    let Some(newest) = outcome.stranded_commits.first() else {
        return Ok(());
    };
    let count = outcome.stranded_total;
    let (noun, them) = if count == 1 {
        ("commit", "it")
    } else {
        ("commits", "them")
    };
    let would = if outcome.dry_run { " would be" } else { "" };
    outln!(w);
    outln!(
        w,
        "> **Warning:** {count} {noun}{would} left behind, not on any branch:"
    );
    outln!(w);
    for commit in &outcome.stranded_commits {
        outln!(w, "- `{}` {}", short_hex(&commit.oid), commit.subject);
    }
    if outcome.stranded_truncated {
        outln!(w, "- … and {} more", count - outcome.stranded_commits.len());
    }
    outln!(w);
    outln!(w, "Keep {them} with `grit branch <name> {}`.", newest.oid);
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
        StashPushOutcome::Saved {
            oid,
            message,
            dry_run,
            ..
        } => {
            let short = short_hex(oid);
            let verb = if *dry_run { "Would stash" } else { "Saved" };
            if message.is_empty() {
                outln!(w, "- {verb} working tree and index state · `{short}`");
            } else {
                outln!(w, "- {verb} **{message}** · `{short}`");
            }
        }
        StashPushOutcome::Preview {
            paths, untracked, ..
        } => {
            outln!(w, "- Would stash **{}** tracked path(s)", paths.len());
            for path in paths {
                outln!(w, "  - `{path}`");
            }
            if !untracked.is_empty() {
                outln!(w, "- Would stash **{}** untracked path(s)", untracked.len());
                for path in untracked {
                    outln!(w, "  - `{path}`");
                }
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
        outln!(w);
        outln!(
            w,
            "→ resolve conflicts, then `grit add PATH` to mark them resolved"
        );
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
        outln!(w);
        outln!(
            w,
            "→ resolve conflicts, `grit add PATH`, then `{}`",
            outcome.drop_command()
        );
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
        WorktreeOutcome::Add {
            path,
            branch,
            head,
            locked,
            checked_out,
        } => {
            outln!(w, "## Worktree add");
            outln!(w);
            let short = short_hex(head);
            if let Some(b) = branch {
                outln!(w, "- Created **`{path}`** on branch **`{b}`** · `{short}`");
            } else {
                outln!(w, "- Created **`{path}`** (detached) · `{short}`");
            }
            if *locked {
                outln!(w, "- Locked during creation");
            }
            if !checked_out {
                outln!(w, "- Registered without checkout (`--no-checkout`)");
            }
        }
        WorktreeOutcome::Remove { path } => {
            outln!(w, "## Worktree remove");
            outln!(w);
            outln!(w, "- Removed **`{path}`**");
        }
        WorktreeOutcome::Prune { dry_run, pruned } => {
            outln!(w, "## Worktree prune");
            outln!(w);
            if *dry_run {
                outln!(w, "Dry run (no registrations removed).");
                outln!(w);
            }
            if pruned.is_empty() {
                outln!(w, "No prunable worktrees.");
            } else if *dry_run {
                for row in pruned {
                    outln!(w, "- Would prune **`{}`** · {}", row.path, row.reason);
                }
            } else {
                for row in pruned {
                    outln!(w, "- Pruned **`{}`** · {}", row.path, row.reason);
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
        WorktreeOutcome::Move { from, to, dry_run } => {
            outln!(w, "## Worktree move");
            outln!(w);
            if *dry_run {
                outln!(w, "- Would move **`{from}`** → **`{to}`**");
            } else {
                outln!(w, "- Moved **`{from}`** → **`{to}`**");
            }
        }
        WorktreeOutcome::Repair { repairs, dry_run } => {
            outln!(w, "## Worktree repair");
            outln!(w);
            if repairs.is_empty() {
                outln!(w, "No worktree links to repair.");
            } else {
                for r in repairs {
                    if *dry_run {
                        outln!(w, "- Would repair **`{}`** · `{}`", r.path, r.kind);
                    } else {
                        outln!(w, "- Repaired **`{}`** · `{}`", r.path, r.kind);
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_worktree_table(w: &mut dyn Write, worktrees: &[WorktreeRow]) -> anyhow::Result<()> {
    if worktrees.is_empty() {
        outln!(w, "No worktrees.");
        return Ok(());
    }
    outln!(w, "| Path | Branch | HEAD | Current | Locked | Prunable |");
    outln!(w, "| --- | --- | --- | --- | --- | --- |");
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
        let locked = if wt.locked {
            wt.lock_reason
                .as_deref()
                .map(|r| format!("yes ({})", escape_table_cell(r)))
                .unwrap_or_else(|| "yes".into())
        } else {
            "no".into()
        };
        let prunable = wt
            .prunable_reason
            .as_deref()
            .map(escape_table_cell)
            .unwrap_or_else(|| {
                if wt.prunable {
                    "yes".into()
                } else {
                    "no".into()
                }
            });
        outln!(
            w,
            "| {} | {} | {} | {} | {} | {} |",
            escape_table_cell(&wt.path),
            branch,
            head,
            if wt.is_current { "yes" } else { "no" },
            locked,
            prunable
        );
    }
    Ok(())
}

/// Result of `grit auth` sign-in.
pub fn print_auth_outcome(w: &mut dyn Write, outcome: &AuthOutcome) -> anyhow::Result<()> {
    outln!(w, "## Auth");
    outln!(w);
    if outcome.authenticated {
        match &outcome.user {
            Some(user) => outln!(w, "- Signed in to **`{}`** as **`{user}`**", outcome.host),
            None => outln!(w, "- Signed in to **`{}`**", outcome.host),
        }
    } else {
        outln!(w, "- Not signed in to **`{}`**", outcome.host);
    }
    Ok(())
}

/// Result of `grit auth logout`.
pub fn print_logout_outcome(w: &mut dyn Write, outcome: &LogoutOutcome) -> anyhow::Result<()> {
    outln!(w, "## Auth logout");
    outln!(w);
    if outcome.dry_run {
        outln!(w, "- **dry_run:** true");
        if let Some(cred) = &outcome.credential {
            outln!(
                w,
                "- Would remove **`{}`** credential for **`{}`** (`{}`)",
                cred.username.as_deref().unwrap_or("(no username)"),
                cred.host,
                cred.protocol
            );
        } else {
            outln!(w, "- No stored credential would be removed");
        }
        return Ok(());
    }
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
/// Result of `grit serve --setup` or `grit serve add-user`.
pub fn print_serve_user_outcome(
    w: &mut dyn Write,
    outcome: &ServeUserOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Serve user");
    outln!(w);
    let role = if outcome.admin { "admin user" } else { "user" };
    outln!(w, "- Created {role} **`{}`**", outcome.user);
    outln!(w, "- Database: `{}`", outcome.database);
    if let Some(password) = &outcome.password {
        outln!(
            w,
            "- Temporary password (shown once, changed at first sign-in): `{password}`"
        );
    }
    Ok(())
}

/// Result of `grit serve token`.
pub fn print_serve_token_outcome(
    w: &mut dyn Write,
    outcome: &ServeTokenOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Serve token");
    outln!(w);
    outln!(w, "- User: **`{}`**", outcome.user);
    outln!(w, "- Token (shown once): `{}`", outcome.token);
    Ok(())
}

/// Result of `grit serve grant`.
pub fn print_serve_grant_outcome(
    w: &mut dyn Write,
    outcome: &ServeGrantOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Serve grant");
    outln!(w);
    outln!(
        w,
        "- **`{}`** has **{}** access to `{}`",
        outcome.user,
        outcome.access,
        outcome.repository
    );
    Ok(())
}

/// Result of `grit serve users`.
pub fn print_serve_users_outcome(
    w: &mut dyn Write,
    outcome: &ServeUsersOutcome,
) -> anyhow::Result<()> {
    outln!(w, "## Serve users");
    outln!(w);
    outln!(w, "Database: `{}`", outcome.database);
    outln!(w);
    outln!(w, "| User | Tokens | Access |");
    outln!(w, "| --- | ---: | --- |");
    for user in &outcome.users {
        outln!(
            w,
            "| `{}` | {} | {} |",
            user.email,
            user.tokens,
            escape_table_cell(&user.access_summary())
        );
    }
    Ok(())
}

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

/// Remote hook/diagnostic lines from a push (side-band channel 2).
pub fn print_remote_push_messages(w: &mut dyn Write, messages: &[String]) -> anyhow::Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    outln!(w, "### Remote messages");
    for line in messages {
        outln!(w, "- {}", format_inline_code(line));
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
    fn remote_push_messages_use_safe_inline_code() {
        use super::print_remote_push_messages;
        let mut buf = Vec::new();
        print_remote_push_messages(&mut buf, &["use `ticket` in message".to_owned()]).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("``use `ticket` in message``"));
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
    fn branch_list_table_escapes_pipe_in_subject() {
        use crate::commands::branch::BranchEntry;

        use crate::commands::branch::BranchListScope;

        let branches = vec![BranchEntry {
            name: "main".into(),
            current: true,
            is_remote: false,
            full_ref: Some("refs/heads/main".into()),
            remote: None,
            oid: Some("b5ea30de417ac84c09b8cfc0acf5f2c39efc611f".into()),
            short_oid: Some("b5ea30de".into()),
            subject: Some("fix|widget".into()),
            date: None,
            upstream: None,
            worktree: None,
        }];
        let mut buf = Vec::new();
        super::print_branch_list(&mut buf, &branches, BranchListScope::Local).expect("markdown");
        let rendered = String::from_utf8(buf).expect("utf-8");
        let row = rendered
            .lines()
            .find(|line| line.contains("| `main` |"))
            .expect("branch row");
        assert!(
            row.contains("fix\\|widget"),
            "subject cell should escape pipe delimiters: {row}"
        );
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
                prunable_reason: None,
                is_current: true,
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

    fn hunkless_file(
        path: &str,
        old_path: Option<&str>,
        old_mode: Option<&str>,
        new_mode: Option<&str>,
    ) -> crate::commands::diff::FileDiff {
        crate::commands::diff::FileDiff {
            path: path.to_owned(),
            old_path: old_path.map(str::to_owned),
            status: crate::output::ChangeStatus::Modified,
            old_mode: old_mode.map(str::to_owned),
            new_mode: new_mode.map(str::to_owned),
            old_encoding_lossy: false,
            new_encoding_lossy: false,
            binary: false,
            content_changed: false,
            similarity: None,
            stat: None,
            combined_status: None,
            hunks: Some(Vec::new()),
        }
    }

    #[test]
    fn hunkless_rename_and_mode_change_emit_extended_headers() {
        let file = hunkless_file("h.txt", Some("g.txt"), Some("100644"), Some("100755"));
        assert_eq!(
            super::render_file_patch(&file),
            "rename from g.txt\nrename to h.txt\nold mode 100644\nnew mode 100755"
        );
    }

    #[test]
    fn hunkless_file_without_mode_or_path_change_has_empty_body() {
        let file = hunkless_file("f.txt", None, Some("100644"), Some("100644"));
        assert_eq!(super::render_file_patch(&file), "");
    }
}
