//! Outcome types and renderers for `grit pull`.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;
use grit_lib::operation::Conflict;
use schemars::JsonSchema;
use serde::Serialize;

use crate::operation_output::{render_conflicts_human, render_rebase_conflict_footer_human};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::progress::summary::TransferSummary;
use crate::render::conflict::MergeConflictJson;
use crate::render::list::{format_inline_code, short_change_id};
use crate::ui::path_display_for_discovered_repo;

/// Result of `grit pull` (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullResult {
    UpToDate,
    FastForward,
    Merged,
    RefusedNotFf,
    Conflicts,
    SetUpstream,
    Rebased,
    AutostashConflict,
    // A rebase stopped on a conflict and is left in progress (`grit rebase --continue`).
    RebaseStopped,
    // In a snapshot checkout with `HEAD` detached: only `origin/<target>` moved.
    Detached,
    // In a snapshot checkout: the branch's commits landed in the target, so it moved to the
    // new snapshot (keeping uncommitted changes) instead of replaying them.
    Landed,
    // In a snapshot checkout, `grit pull <branch>`: the checkout had no such branch, so it was
    // created at the server's.
    Created,
}

/// Dry-run summary from `grit pull --check`.
#[derive(Serialize, JsonSchema)]
pub struct PullCheckJson {
    pub ahead: usize,
    pub behind: usize,
    pub fast_forward_possible: bool,
    pub would: PullResult,
}

/// Outcome of `grit pull`.
#[derive(Serialize, JsonSchema)]
pub struct PullOutcome {
    pub result: PullResult,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflict_details: Option<Vec<MergeConflictJson>>,
    /// Paused operation when `result` is `conflicts`: `merge` or `rebase`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// Local commit the rebase stopped on (`operation` is `rebase`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Ordered `grit` commands to finish the paused operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    /// Abort/skip commands (not part of the ordered sequence).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// Autostash commit kept on `refs/stash` (rebase stopped, or re-applying it conflicted).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub autostash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub check: Option<PullCheckJson>,
    /// In a snapshot checkout, the server snapshot the pull refreshed onto.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<PullRefreshJson>,
    /// In a snapshot checkout, `grit pull <branch>`: the server's branch as
    /// the checkout sees it, and what the local branch did.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub server_branch: Option<PullServerBranchJson>,
    /// What the fetch step received and where the time went, for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
    #[serde(skip)]
    pub(crate) conflict_sources: Option<Vec<Conflict>>,
    #[serde(skip)]
    pub(crate) conflict_merge_label: Option<String>,
}

/// What `grit pull` refreshed in a checkout.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PullRefreshJson {
    /// The server's target branch.
    pub target: String,
    /// The local ref holding the newest snapshot, `refs/remotes/origin/<target>`.
    pub upstream_ref: String,
    /// The real commit the newest snapshot stands for. With `--dry-run`, where
    /// the target is now.
    pub base: String,
    /// The newest snapshot commit, now `origin/<target>`. With `--dry-run`, the
    /// checkout's snapshot as it is.
    pub snapshot: String,
    /// The real commit the checkout's previous snapshot stood for.
    pub previous_base: Option<String>,
    /// The checkout's previous snapshot.
    pub previous_snapshot: Option<String>,
    /// Whether the target had moved since the checkout's last snapshot.
    pub moved: bool,
    /// What changed in the checkout's paths between the two snapshots; null
    /// when the target didn't move.
    pub changes: Option<RefreshChangesJson>,
    /// With `--dry-run`: nothing was fetched or changed, and the server made
    /// no snapshot.
    pub dry_run: bool,
    /// Local branches with no commits of their own that moved to the new
    /// snapshot: the one named after the target, and others whose commits
    /// all landed in the target.
    pub followed: Vec<RefreshFollowedJson>,
    /// Other branches with commits of their own still on an older snapshot.
    pub behind: Vec<RefreshBehindJson>,
    /// Commits made with plain Git on the current branch that were given a
    /// change id before the rebase (the branch was rewritten with it).
    pub gave_change_ids: usize,
}

/// What changed in a checkout's paths between two snapshots, as
/// `git diff --shortstat` counts it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshChangesJson {
    /// Files changed.
    pub files: usize,
    /// Lines added.
    pub insertions: usize,
    /// Lines removed.
    pub deletions: usize,
}

/// A branch a refresh moved along with the snapshot.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshFollowedJson {
    /// The branch.
    pub name: String,
    /// The older snapshot it was at.
    pub from: String,
    /// The new snapshot it is at.
    pub to: String,
}

/// A branch still building on an older snapshot after a refresh.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshBehindJson {
    /// The branch.
    pub name: String,
    /// Its tip.
    pub tip: String,
}

/// What `grit pull <branch>` did in a checkout: the server's branch as the
/// checkout sees it, and the local branch brought onto it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PullServerBranchJson {
    /// The branch, as the server spells it (which can differ in case from the
    /// name given, on a server that ignores case).
    pub name: String,
    /// The local ref holding the server's branch as this checkout sees it,
    /// `refs/remotes/origin/<branch>`.
    pub upstream_ref: String,
    /// Where the server's branch is, on its real history.
    pub tip: String,
    /// What this checkout sees of it, now at `upstream_ref`.
    pub projected: String,
    /// The server branch's commits the target lacks, all shown to this
    /// checkout (cut down to its paths).
    pub commits: usize,
    /// How many of them were made afresh for this checkout.
    pub made: usize,
    /// How many were commits that were there already: checkout commits on
    /// the same snapshots that the server's are grafts of (this checkout's
    /// own among them), and what it was shown before.
    pub reused: usize,
    /// Where the local branch was; null when it was created.
    pub previous: Option<String>,
    /// Commits of the local branch replayed onto the server's (its own
    /// commits, without copies of the server's).
    pub own: usize,
    /// Whether `HEAD` was moved onto the branch to rebase it.
    pub switched: bool,
    /// The branch `HEAD` was on before, if any.
    pub was_on: Option<String>,
    /// Commits made with plain Git on the branch that were given a change id
    /// first (the branch was rewritten with it).
    pub gave_change_ids: usize,
}

impl HumanRender for PullOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(r) = &self.refresh {
            render_refresh_head(w, r)?;
            if r.dry_run {
                return Ok(());
            }
        }
        if let Some(b) = &self.server_branch {
            return render_server_branch(w, self, b, ctx.color);
        }
        if let Some(check) = &self.check {
            outln!(
                w,
                "Would {} ({} ahead, {} behind). Fast-forward {}possible.",
                check_would_phrase(check.would),
                check.ahead,
                check.behind,
                if check.fast_forward_possible {
                    ""
                } else {
                    "not "
                }
            );
            return Ok(());
        }
        let short = self.oid.as_deref().map(short_hex).unwrap_or_default();
        match self.result {
            PullResult::UpToDate => outln!(w, "Already up to date."),
            PullResult::FastForward => outln!(w, "Fast-forwarded {} → {short}", self.branch),
            PullResult::Merged => outln!(
                w,
                "Merged {} into the current branch ({short})",
                self.branch
            ),
            PullResult::RefusedNotFf => {
                let ahead = self.ahead.unwrap_or(0);
                let behind = self.behind.unwrap_or(0);
                outln!(
                    w,
                    "Not possible to fast-forward {}; local is {ahead} ahead, {behind} behind.",
                    self.branch
                );
            }
            PullResult::Conflicts => {
                print_conflicts_to_stderr(self);
                if self.operation.as_deref() == Some("rebase") {
                    render_rebase_conflict_footer_human();
                    if let Some(stash) = &self.autostash {
                        eprintln!(
                            "Your local changes are kept in the stash ({}); run `grit stash pop` once the rebase is finished or aborted.",
                            short_hex(stash)
                        );
                    }
                } else {
                    eprintln!(
                        "\nAutomatic merge failed; fix conflicts and run `grit commit`, or run `grit merge --abort`."
                    );
                }
            }
            PullResult::SetUpstream => outln!(
                w,
                "Set {} to {}.",
                self.branch,
                self.upstream.as_deref().unwrap_or_default()
            ),
            PullResult::Rebased => outln!(w, "Rebased onto {} ({short})", self.branch),
            PullResult::Landed => outln!(
                w,
                "Moved to {} ({short}), which has this branch's commits; uncommitted changes are kept.",
                self.branch
            ),
            PullResult::AutostashConflict => {
                outln!(
                    w,
                    "Pulled {}, but re-applying autostash left conflicts; the stash entry was kept.",
                    self.branch
                );
            }
            PullResult::RebaseStopped => {
                print_conflicts_to_stdout(w, self)?;
                outln!(
                    w,
                    "Rebasing onto {} stopped on a conflict; the rebase is in progress.",
                    self.branch
                );
                outln!(
                    w,
                    "Resolve the conflicts, `grit add` the files, then run `grit rebase --continue`; or run `grit rebase --abort` to put the branch back where it was."
                );
            }
            PullResult::Detached => outln!(
                w,
                "HEAD is detached, so only {} moved; the new snapshot is there.",
                self.branch
            ),
            PullResult::Created => outln!(w, "Created a branch at {} ({short}).", self.branch),
        }
        if let Some(r) = &self.refresh {
            render_refresh_branches(w, r)?;
        }
        if self.transfer.received_anything() {
            outln!(w);
            self.transfer.render_rows(w, ctx.color)?;
        }
        Ok(())
    }
}

/// The `CONFLICT (...)` lines of a pull that stopped, on stderr.
fn print_conflicts_to_stderr(outcome: &PullOutcome) {
    let path_opts = path_display_for_discovered_repo();
    if let Some(details) = &outcome.conflict_sources {
        let _ = render_conflicts_human(
            details,
            outcome.conflict_merge_label.as_deref(),
            path_opts.as_ref(),
            &mut std::io::sink(),
        );
    } else if let Some(paths) = &outcome.conflicts {
        eprintln!("Conflicts in:");
        for path in paths {
            let shown = path_opts
                .as_ref()
                .map(|opts| opts.format_path_for_human(path))
                .unwrap_or_else(|| path.clone());
            eprintln!("  {shown}");
        }
    }
}

fn print_conflicts_to_stdout(w: &mut dyn Write, outcome: &PullOutcome) -> Result<()> {
    let path_opts = path_display_for_discovered_repo();
    if let Some(details) = &outcome.conflict_sources {
        for conflict in details {
            outln!(
                w,
                "{}",
                crate::operation_output::format_conflict_human_line(
                    conflict,
                    outcome.conflict_merge_label.as_deref(),
                    path_opts.as_ref(),
                )
            );
        }
    }
    Ok(())
}

/// The first line of a refresh: whether the target moved, and what changed
/// in the checkout's paths.
fn render_refresh_head(w: &mut dyn Write, r: &PullRefreshJson) -> Result<()> {
    if r.gave_change_ids > 0 {
        outln!(w, "Gave {}.", gave_change_ids_phrase(r.gave_change_ids));
    }
    let previous = r.previous_base.as_deref().map(short_hex).unwrap_or("?");
    if !r.moved {
        outln!(
            w,
            "{} has not moved since this checkout's last snapshot (still at {}).",
            r.target,
            short_hex(&r.base)
        );
        return Ok(());
    }
    if r.dry_run {
        outln!(
            w,
            "{} has moved {previous} → {} since this checkout's last snapshot; `grit pull` would refresh onto it.",
            r.target,
            short_hex(&r.base)
        );
        return Ok(());
    }
    outln!(
        w,
        "{} moved {previous} → {}; new snapshot {} at origin/{}",
        r.target,
        short_hex(&r.base),
        short_hex(&r.snapshot),
        r.target
    );
    match &r.changes {
        Some(c) if c.files == 0 => outln!(w, "  nothing changed in this checkout's paths"),
        Some(c) => outln!(w, "  {} in this checkout's paths", changes_phrase(c)),
        None => {}
    }
    Ok(())
}

/// `grit pull <branch>` in a checkout: where the server's branch is and
/// what the local branch did.
fn render_server_branch(
    w: &mut dyn Write,
    outcome: &PullOutcome,
    b: &PullServerBranchJson,
    color: bool,
) -> Result<()> {
    if b.gave_change_ids > 0 {
        outln!(w, "Gave {}.", gave_change_ids_phrase(b.gave_change_ids));
    }
    outln!(
        w,
        "The server's {} is at {}: {}, at {} ({}).",
        b.name,
        short_hex(&b.tip),
        commits_on_snapshots(b.commits),
        outcome.branch,
        short_hex(&b.projected)
    );
    if let Some(check) = &outcome.check {
        outln!(
            w,
            "Would {} ({} ahead, {} behind). Fast-forward {}possible.",
            check_would_phrase(check.would),
            check.ahead,
            check.behind,
            if check.fast_forward_possible {
                ""
            } else {
                "not "
            }
        );
        return Ok(());
    }
    let short = outcome.oid.as_deref().map(short_hex).unwrap_or_default();
    match outcome.result {
        PullResult::Created => outln!(
            w,
            "Created {} at {short}, the server's; `grit switch {}` to work on it.",
            b.name,
            b.name
        ),
        PullResult::UpToDate => outln!(w, "{} already has them.", b.name),
        PullResult::FastForward => outln!(
            w,
            "Moved {} to {} ({short}); it had nothing of its own.",
            b.name,
            outcome.branch
        ),
        PullResult::Rebased => outln!(
            w,
            "Rebased {} onto {} ({short}), {} of yours on top.",
            b.name,
            outcome.branch,
            plural(b.own, "commit", "commits")
        ),
        PullResult::RebaseStopped => {
            print_conflicts_to_stdout(w, outcome)?;
            outln!(
                w,
                "Rebasing {} onto {} stopped on a conflict; the rebase is in progress.",
                b.name,
                outcome.branch
            );
            outln!(
                w,
                "Resolve the conflicts, `grit add` the files, then run `grit rebase --continue`; or run `grit rebase --abort` to put {} back where it was.",
                b.name
            );
        }
        PullResult::Conflicts => {
            print_conflicts_to_stderr(outcome);
            eprintln!(
                "\nRebasing {} onto {} would stop on a conflict, and the index or worktree has changes of its own, so nothing changed.\nCommit or stash them, then run `grit pull {}` again to resolve the conflict in a rebase.",
                b.name, outcome.branch, b.name
            );
        }
        PullResult::AutostashConflict => outln!(
            w,
            "Rebased {} onto {}, but re-applying the stashed changes left conflicts; the stash entry was kept.",
            b.name,
            outcome.branch
        ),
        _ => {}
    }
    if b.switched {
        match &b.was_on {
            Some(was) => outln!(w, "Now on {}; it was {was}.", b.name),
            None => outln!(w, "Now on {}; HEAD was detached.", b.name),
        }
    }
    if outcome.result == PullResult::Rebased {
        outln!(
            w,
            "{} builds on the server's now; `grit checkin {}` sends it.",
            b.name,
            b.name
        );
    }
    if outcome.transfer.received_anything() {
        outln!(w);
        outcome.transfer.render_rows(w, color)?;
    }
    Ok(())
}

/// `N commit(s) on this checkout's snapshots`.
pub(crate) fn commits_on_snapshots(n: usize) -> String {
    format!(
        "{} on this checkout's snapshots",
        plural(n, "commit", "commits")
    )
}

/// `n` and the word for it, one or many.
fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `N commit(s) made with git a change id`, after "gave".
pub(crate) fn gave_change_ids_phrase(n: usize) -> String {
    format!(
        "{n} commit{} made with git a change id",
        if n == 1 { "" } else { "s" }
    )
}

/// `N files changed, I insertions(+), D deletions(-)`, as `git diff --shortstat` words it.
pub(crate) fn changes_phrase(c: &RefreshChangesJson) -> String {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let mut parts = vec![plural(c.files, "file changed", "files changed")];
    if c.insertions > 0 || c.deletions == 0 {
        parts.push(plural(c.insertions, "insertion(+)", "insertions(+)"));
    }
    if c.deletions > 0 {
        parts.push(plural(c.deletions, "deletion(-)", "deletions(-)"));
    }
    parts.join(", ")
}

/// The other branches a refresh moved, or left on an older snapshot.
fn render_refresh_branches(w: &mut dyn Write, r: &PullRefreshJson) -> Result<()> {
    for f in &r.followed {
        outln!(w, "{} follows to {}", f.name, short_hex(&f.to));
    }
    for b in &r.behind {
        outln!(
            w,
            "{} still builds on an older snapshot; run `grit switch {}`, then `grit pull`",
            b.name,
            b.name
        );
    }
    Ok(())
}

fn check_would_phrase(result: PullResult) -> &'static str {
    match result {
        PullResult::UpToDate => "leave the branch up to date",
        PullResult::FastForward => "fast-forward",
        PullResult::Merged => "merge",
        PullResult::Rebased => "rebase",
        PullResult::SetUpstream => "set upstream",
        PullResult::RefusedNotFf => "refuse (not fast-forward)",
        PullResult::Conflicts | PullResult::RebaseStopped => "stop with conflicts",
        PullResult::AutostashConflict => "conflict on autostash restore",
        PullResult::Detached => "leave HEAD detached",
        PullResult::Landed => "move to the upstream, which has this branch's commits",
        PullResult::Created => "create the branch at the server's",
    }
}

impl MarkdownRender for PullOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_pull_outcome(w, self)?;
        Ok(())
    }
}

fn print_pull_conflict_bullets(w: &mut dyn Write, outcome: &PullOutcome) -> anyhow::Result<()> {
    let path_opts = crate::ui::path_display_for_discovered_repo();
    if let Some(sources) = &outcome.conflict_sources {
        crate::operation_output::render_conflicts_markdown(
            sources,
            outcome.conflict_merge_label.as_deref(),
            path_opts.as_ref(),
            w,
        )?;
    } else if let Some(paths) = outcome.conflicts.as_deref() {
        for path in paths {
            let shown = path_opts
                .as_ref()
                .map(|opts| format_inline_code(&opts.format_path_for_human(path)))
                .unwrap_or_else(|| format_inline_code(path));
            outln!(w, "- {shown}");
        }
    }
    Ok(())
}

/// Result of `grit pull`.
fn print_pull_outcome(w: &mut dyn Write, outcome: &PullOutcome) -> anyhow::Result<()> {
    outln!(w, "## Pull");
    outln!(w);
    if let Some(r) = &outcome.refresh {
        print_refresh_head(w, r)?;
        if r.dry_run {
            return Ok(());
        }
    }
    if let Some(b) = &outcome.server_branch {
        return print_pull_server_branch(w, outcome, b);
    }
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
            print_pull_conflict_bullets(w, outcome)?;
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
        PullResult::Landed => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(
                w,
                "- Moved to **`{}`** · `{short}`, which has this branch's commits; uncommitted changes kept",
                outcome.branch
            );
        }
        PullResult::AutostashConflict => {
            outln!(
                w,
                "- Pulled **`{}`**, but re-applying autostash left conflicts (stash kept)",
                outcome.branch
            );
        }
        PullResult::RebaseStopped => {
            outln!(
                w,
                "- Rebase onto **`{}`** stopped on a conflict; the rebase is in progress",
                outcome.branch
            );
            outln!(w);
            print_pull_conflict_bullets(w, outcome)?;
            outln!(
                w,
                "→ resolve the conflicts, `grit add` the files, then `grit rebase --continue`; or `grit rebase --abort` to put the branch back"
            );
        }
        PullResult::Detached => {
            outln!(
                w,
                "- `HEAD` is detached, so only **`{}`** moved; the new snapshot is there",
                outcome.branch
            );
        }
        PullResult::Created => {
            let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
            outln!(
                w,
                "- Created a branch at **`{}`** · `{short}`",
                outcome.branch
            );
        }
    }
    if let Some(r) = &outcome.refresh {
        for f in &r.followed {
            outln!(w, "- `{}` follows to `{}`", f.name, short_hex(&f.to));
        }
        for b in &r.behind {
            outln!(
                w,
                "- `{}` still builds on an older snapshot → `grit switch {}`, then `grit pull`",
                b.name,
                b.name
            );
        }
    }
    Ok(())
}

/// `grit pull <branch>` in a snapshot checkout: the server's branch as the
/// checkout sees it, then what the local branch did.
fn print_pull_server_branch(
    w: &mut dyn Write,
    outcome: &PullOutcome,
    b: &PullServerBranchJson,
) -> anyhow::Result<()> {
    if b.gave_change_ids > 0 {
        outln!(
            w,
            "- Change ids: gave {}",
            gave_change_ids_phrase(b.gave_change_ids)
        );
    }
    outln!(
        w,
        "- Server branch: **`{}`** at `{}` · {} · at `{}` (`{}`)",
        b.name,
        short_hex(&b.tip),
        commits_on_snapshots(b.commits),
        b.upstream_ref,
        short_hex(&b.projected)
    );
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
    let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
    match outcome.result {
        PullResult::Created => outln!(
            w,
            "- Created **`{}`** at `{short}`, the server's → `grit switch {}` to work on it",
            b.name,
            b.name
        ),
        PullResult::UpToDate => outln!(w, "- **`{}`** already has them", b.name),
        PullResult::FastForward => outln!(
            w,
            "- Moved **`{}`** to **`{}`** · `{short}`; it had nothing of its own",
            b.name,
            outcome.branch
        ),
        PullResult::Rebased => outln!(
            w,
            "- Rebased **`{}`** onto **`{}`** · `{short}` · {} of yours on top → `grit checkin {}` sends it",
            b.name,
            outcome.branch,
            if b.own == 1 {
                "1 commit".to_owned()
            } else {
                format!("{} commits", b.own)
            },
            b.name
        ),
        PullResult::RebaseStopped => {
            outln!(
                w,
                "- Rebase of **`{}`** onto **`{}`** stopped on a conflict; the rebase is in progress",
                b.name,
                outcome.branch
            );
            outln!(w);
            print_pull_conflict_bullets(w, outcome)?;
            outln!(
                w,
                "→ resolve the conflicts, `grit add` the files, then `grit rebase --continue`; or `grit rebase --abort` to put the branch back"
            );
        }
        PullResult::Conflicts => {
            outln!(
                w,
                "- Rebasing **`{}`** onto **`{}`** would stop on a conflict",
                b.name,
                outcome.branch
            );
            outln!(w);
            print_pull_conflict_bullets(w, outcome)?;
            outln!(
                w,
                "→ the index or worktree has changes of its own, so nothing changed; commit or stash them, then `grit pull {}` again",
                b.name
            );
        }
        PullResult::AutostashConflict => outln!(
            w,
            "- Rebased **`{}`**, but re-applying the stashed changes left conflicts (stash kept)",
            b.name
        ),
        _ => {}
    }
    if b.switched {
        match &b.was_on {
            Some(was) => outln!(w, "- Now on `{}` (was `{was}`)", b.name),
            None => outln!(w, "- Now on `{}` (`HEAD` was detached)", b.name),
        }
    }
    Ok(())
}

/// The refresh line of `grit pull` in a snapshot checkout.
fn print_refresh_head(w: &mut dyn Write, r: &PullRefreshJson) -> anyhow::Result<()> {
    if r.gave_change_ids > 0 {
        outln!(
            w,
            "- Change ids: gave {}",
            gave_change_ids_phrase(r.gave_change_ids)
        );
    }
    let previous = r.previous_base.as_deref().map(short_hex).unwrap_or("?");
    if !r.moved {
        outln!(
            w,
            "- Refresh: **`{}`** has not moved since this checkout's last snapshot (`{}`)",
            r.target,
            short_hex(&r.base)
        );
    } else if r.dry_run {
        outln!(
            w,
            "- Refresh (dry run): **`{}`** moved `{previous}` → `{}`; `grit pull` would refresh onto it",
            r.target,
            short_hex(&r.base)
        );
    } else {
        let changes = match &r.changes {
            Some(c) if c.files == 0 => " · nothing changed in this checkout's paths".to_owned(),
            Some(c) => format!(" · {} in this checkout's paths", changes_phrase(c)),
            None => String::new(),
        };
        outln!(
            w,
            "- Refresh: **`{}`** moved `{previous}` → `{}` · new snapshot `{}` at `{}`{changes}",
            r.target,
            short_hex(&r.base),
            short_hex(&r.snapshot),
            r.upstream_ref
        );
    }
    Ok(())
}
