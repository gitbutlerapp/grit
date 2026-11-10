//! Shared Markdown formatters for `--markdown` command output.

pub use crate::render::list::{diff_code_fence, format_inline_code, print_fenced_text};
pub use crate::render::table::escape_table_cell;

use std::io::Write;

use grit_lib::diff::{DiffEntry, DiffStatus};
use grit_lib::objects::short_hex;

use crate::commands::add::AddOutcome;
use crate::commands::commit::CommitOutcome;
use crate::commands::diff::{file_diff_status_label, FileDiff, Hunk, Line, LineKind, Segment};
use crate::commands::serve_http::{
    ServeGrantOutcome, ServeTokenOutcome, ServeUserOutcome, ServeUsersOutcome,
};
use crate::commands::stash::{
    StashApplyOutcome, StashDropOutcome, StashPopOutcome, StashPushOutcome,
};
use crate::commands::update::UpdateOutcome;
use crate::commands::worktree::{WorktreeOutcome, WorktreeRow};
use crate::context::{self, CommitSummary};
use crate::output::{outln, RenderContext};
use crate::render::auth::{AuthOutcome, LogoutOutcome};
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
    let repo = context::discover()?;
    let sha_abbrevs = crate::log_display::abbrev_commit_summaries(&repo, commits)?;
    let now = ctx.now.unix_timestamp();
    for (c, short) in commits.iter().zip(&sha_abbrevs) {
        outln!(w, "{}", commit_bullet_with_short(c, short, now));
    }
    Ok(())
}

/// One commit bullet with optional `--decorate` parenthetical after the short oid.
pub fn commit_bullet_with_decorations(
    c: &CommitSummary,
    short: &str,
    refs: &[crate::output::CommitRefJson],
    head_branch: Option<&str>,
    now: i64,
) -> String {
    let date = context::relative_date_from(c.timestamp, now);
    let deco = crate::log_display::format_decorations_folded(refs, head_branch);
    let decoration = if deco.is_empty() {
        String::new()
    } else {
        format!(" {deco}")
    };
    let change = ui::change_short_code(&c.change_id);
    format!(
        "- `{short}` `{change}`{decoration} {} ({}, {date})",
        c.subject, c.author
    )
}

pub(crate) fn commit_bullet_with_short(c: &CommitSummary, short: &str, now: i64) -> String {
    commit_bullet_with_decorations(c, short, &[], None, now)
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
        _ => "changed",
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
    // Hunkless renames and mode changes are fully described by the heading and metadata
    // lines above; a fence holding only extended headers would add nothing (see the
    // `--markdown` section of `content/docs/commands/diff.md`).
    if file.hunks.as_deref().unwrap_or_default().is_empty() {
        return Ok(());
    }
    let patch = render_file_patch(file);
    outln!(w);
    print_fenced_diff(w, &patch)?;
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
        let short = short_hex(&entry.object);
        let preview = entry.body.trim_end().replace('\n', " ");
        outln!(w, "- `{short}` — {preview}");
    }
    Ok(())
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
        if let Some(n) = outcome.index {
            outln!(w, "- **stash@{{{n}}}** · stopped with conflicts");
        } else {
            outln!(w, "- stash **`{}`** · stopped with conflicts", outcome.oid);
        }
        for path in &outcome.conflict_paths {
            outln!(w, "  - `{path}`");
        }
        outln!(w);
        outln!(
            w,
            "→ resolve conflicts, then `grit add PATH` to mark them resolved"
        );
    } else if let Some(n) = outcome.index {
        outln!(w, "- Applied **stash@{{{n}}}**");
    } else {
        outln!(w, "- Applied stash **`{}`**", outcome.oid);
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
    outln!(
        w,
        "- Dropped **stash@{{{}}}** (`{}`)",
        outcome.index,
        outcome.oid
    );
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
        let branch = if wt.bare {
            "_(bare)_".into()
        } else {
            wt.branch
                .as_deref()
                .map(|b| format!("`{}`", escape_table_cell(b)))
                .unwrap_or_else(|| {
                    if wt.detached {
                        "_(detached)_".into()
                    } else {
                        "—".into()
                    }
                })
        };
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
            Some(user) if !outcome.verified => outln!(
                w,
                "- Stored a token for **`{user}`** on **`{}`** (not verified; the next push or fetch uses it)",
                outcome.host
            ),
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
pub fn print_add_outcome(
    w: &mut dyn Write,
    outcome: &AddOutcome,
    path_opts: Option<&PathDisplayOptions>,
) -> anyhow::Result<()> {
    outln!(w, "## Add");
    outln!(w);
    let verb = if outcome.dry_run {
        "Would stage"
    } else {
        "Staged"
    };
    if !outcome.worktree_case_collisions.is_empty() {
        let flat: Vec<String> = outcome
            .worktree_case_collisions
            .iter()
            .flat_map(|g| g.paths.clone())
            .collect();
        crate::render::clone::render_worktree_case_collision_guidance(
            w,
            crate::output::OutputMode::Markdown,
            &flat,
        )?;
    }
    if outcome.count == 0 && outcome.no_paths && outcome.worktree_case_collisions.is_empty() {
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
        let shown = path_opts
            .map(|opts| format_inline_code(&opts.format_path_for_human(path)))
            .unwrap_or_else(|| format_inline_code(path));
        outln!(w, "  - {shown}");
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

#[cfg(test)]
mod tests {
    use super::{diff_code_fence, escape_table_cell, format_inline_code, print_fenced_text};
    use crate::render::list::code_fence;

    #[test]
    fn print_fenced_text_closes_fence_when_body_contains_triple_backticks() {
        let body = "Example:\n```\nrun this";
        let mut buf = Vec::new();
        print_fenced_text(&mut buf, body).unwrap();
        let md = String::from_utf8(buf).unwrap();
        assert!(md.starts_with("````text\n"));
        assert!(md.contains("run this\n"));
        assert!(md.ends_with("````\n"));
        assert_eq!(md.matches("````").count(), 2);
    }

    #[test]
    fn print_fenced_text_fences_forged_changes_heading() {
        let body = "## Changes\n\n| File | + | - |\n| --- | ---: | ---: |\n| `evil.sh` | 1 | 0 |";
        let mut buf = Vec::new();
        print_fenced_text(&mut buf, body).unwrap();
        let md = String::from_utf8(buf).unwrap();
        assert!(md.starts_with("```text\n"));
        assert!(md.contains(body));
        assert!(md.ends_with("```\n"));
    }

    #[test]
    fn code_fence_text_uses_four_ticks_when_body_has_three() {
        let body = "```";
        let (open, close) = code_fence(body, "text");
        assert_eq!(open, "````text");
        assert_eq!(close, "````");
    }

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
            old_size: None,
            new_size: None,
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
