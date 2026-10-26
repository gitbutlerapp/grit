//! `--json` / human / markdown rendering for `grit pull`.

use crate::commands::checkout::{PullRefreshJson, PullServerBranchJson};
use crate::commands::merge::MergeConflictJson;
use crate::markdown;
use crate::operation_output::{render_rebase_conflict_footer_human, OperationConflictBlock};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::progress::summary::{stdout_color, TransferSummary};
use anyhow::Result;
use grit_lib::change_id::ChangeId;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::MergeConflictDetail;
use grit_lib::porcelain::pull::{
    PullCheck as LibPullCheck, PullOutcome as LibPullOutcome, PullResult as LibPullResult,
};
use grit_lib::remote::snapshot_checkout::{
    BranchInfo, BranchRefresh, BranchRefreshResult, CheckoutMarker, CheckoutState, RefreshOutcome,
    RefreshedHead,
};
use grit_lib::remote::transfer::TransferStats;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

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

impl From<LibPullResult> for PullResult {
    fn from(result: LibPullResult) -> Self {
        match result {
            LibPullResult::UpToDate => Self::UpToDate,
            LibPullResult::FastForward => Self::FastForward,
            LibPullResult::Merged => Self::Merged,
            LibPullResult::RefusedNotFf => Self::RefusedNotFf,
            LibPullResult::Conflicts | LibPullResult::RebaseConflicts => Self::Conflicts,
            LibPullResult::SetUpstream => Self::SetUpstream,
            LibPullResult::Rebased => Self::Rebased,
            LibPullResult::AutostashConflict => Self::AutostashConflict,
            LibPullResult::RebaseStopped => Self::RebaseStopped,
            LibPullResult::Landed => Self::Landed,
        }
    }
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
}

pub fn map_outcome(outcome: LibPullOutcome) -> PullOutcome {
    let rebase_stop = matches!(
        outcome.result,
        LibPullResult::RebaseConflicts | LibPullResult::RebaseStopped
    );
    let details = outcome.conflicts.as_deref().unwrap_or(&[]);
    let (conflicts, conflict_details) = outcome
        .conflicts
        .as_ref()
        .map(|details| map_conflicts(details, &outcome.label, rebase_stop))
        .unwrap_or((None, None));
    // With `--check`, `result` is only a prediction: no operation is in progress to finish.
    let guidance = match outcome.result {
        _ if outcome.check.is_some() => None,
        LibPullResult::Conflicts => Some((
            "merge",
            OperationConflictBlock::for_merge(details, &outcome.label),
        )),
        LibPullResult::RebaseConflicts | LibPullResult::RebaseStopped => {
            Some(("rebase", OperationConflictBlock::for_rebase(details)))
        }
        _ => None,
    };
    let (operation, next_steps, alternatives) = match guidance {
        Some((operation, block)) => (
            Some(operation.to_owned()),
            Some(block.next_steps),
            block.alternatives,
        ),
        None => (None, None, None),
    };
    PullOutcome {
        result: outcome.result.into(),
        branch: outcome.label,
        oid: outcome.oid.as_ref().map(ObjectId::to_hex),
        change_id: outcome
            .change_id
            .as_ref()
            .map(|id: &ChangeId| id.as_str().to_owned()),
        upstream: outcome.upstream,
        conflicts,
        conflict_details,
        operation,
        commit: outcome.stopped_commit.as_ref().map(ObjectId::to_hex),
        next_steps,
        alternatives,
        autostash: outcome.autostash.as_ref().map(ObjectId::to_hex),
        ahead: outcome.ahead,
        behind: outcome.behind,
        check: outcome.check.map(map_check),
        refresh: None,
        server_branch: None,
        transfer: TransferSummary::new(outcome.transfer, Vec::new(), Default::default()),
    }
}

/// Rebase stops share `conflicts` with merge stops; `operation` tells them apart.
fn map_check(check: LibPullCheck) -> PullCheckJson {
    PullCheckJson {
        ahead: check.ahead,
        behind: check.behind,
        fast_forward_possible: check.fast_forward_possible,
        would: check.would.into(),
    }
}

impl PullOutcome {
    /// A pull with nothing but `result` and `branch` set.
    fn bare(result: PullResult, branch: String) -> Self {
        Self {
            result,
            branch,
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: None,
            conflict_details: None,
            operation: None,
            commit: None,
            next_steps: None,
            alternatives: None,
            autostash: None,
            ahead: None,
            behind: None,
            check: None,
            refresh: None,
            server_branch: None,
            transfer: TransferSummary::default(),
        }
    }

    /// `grit pull` in a snapshot checkout: what the refresh did to the
    /// current branch, with the refresh itself in [`Self::refresh`].
    pub fn from_refresh(refreshed: RefreshOutcome) -> Self {
        let refresh = PullRefreshJson::from_outcome(&refreshed);
        let mut outcome = match refreshed.head {
            RefreshedHead::Branch { pull, .. } => map_outcome(*pull),
            RefreshedHead::Detached => {
                Self::bare(PullResult::Detached, format!("origin/{}", refreshed.target))
            }
        };
        outcome.refresh = Some(refresh);
        outcome.transfer = TransferSummary::new(refreshed.transfer, Vec::new(), Default::default());
        outcome
    }

    /// `grit pull <branch>` in a snapshot checkout: what the local branch did
    /// with the server's branch `info`, fetched with `transfer`. With
    /// `--check`, `result` and [`Self::check`] say what it would do.
    pub fn from_server_branch(
        info: &BranchInfo,
        refreshed: &BranchRefresh,
        transfer: TransferStats,
    ) -> Self {
        let label = format!("origin/{}", refreshed.name);
        let (result, conflicts) = match &refreshed.result {
            BranchRefreshResult::Created => (PullResult::Created, None),
            BranchRefreshResult::UpToDate => (PullResult::UpToDate, None),
            BranchRefreshResult::FastForwarded => (PullResult::FastForward, None),
            BranchRefreshResult::Rebased { .. } => (PullResult::Rebased, None),
            BranchRefreshResult::Stopped { conflicts } => {
                (PullResult::RebaseStopped, Some(conflicts.as_slice()))
            }
            BranchRefreshResult::Conflicts { conflicts } => {
                (PullResult::Conflicts, Some(conflicts.as_slice()))
            }
            BranchRefreshResult::AutostashConflict => (PullResult::AutostashConflict, None),
        };
        let mut outcome = Self::bare(result, label.clone());
        outcome.oid = Some(refreshed.tip.to_hex());
        if let Some(details) = conflicts {
            (outcome.conflicts, outcome.conflict_details) =
                map_conflicts(details, &label, result == PullResult::RebaseStopped);
        }
        if let Some((ahead, behind)) = refreshed.ahead_behind.filter(|_| refreshed.check) {
            outcome.ahead = Some(ahead);
            outcome.behind = Some(behind);
            outcome.check = Some(PullCheckJson {
                ahead,
                behind,
                fast_forward_possible: matches!(
                    result,
                    PullResult::UpToDate | PullResult::FastForward | PullResult::Created
                ),
                would: result,
            });
        }
        outcome.server_branch = Some(PullServerBranchJson::from_refresh(info, refreshed));
        outcome.transfer = TransferSummary::new(transfer, Vec::new(), Default::default());
        outcome
    }

    /// `grit pull --dry-run` in a snapshot checkout: where the target is now,
    /// with nothing fetched or changed.
    pub fn refresh_dry_run(marker: &CheckoutMarker, state: &CheckoutState) -> Self {
        let mut outcome = Self::bare(PullResult::UpToDate, format!("origin/{}", marker.branch));
        outcome.refresh = Some(PullRefreshJson::dry_run(marker, state));
        outcome
    }
}

fn map_conflicts(
    details: &[MergeConflictDetail],
    merge_label: &str,
    rebase_stop: bool,
) -> (Option<Vec<String>>, Option<Vec<MergeConflictJson>>) {
    let paths: Vec<String> = details.iter().map(|d| d.path.clone()).collect();
    // Merge-style summaries name the current branch as "ours"; during a rebase replay the
    // sides are swapped, so rebase stops report kinds only, as `grit rebase` does.
    let conflict_details = Some(
        details
            .iter()
            .map(|d| {
                if rebase_stop {
                    MergeConflictJson {
                        path: d.path.clone(),
                        kind: d.kind.as_str().to_owned(),
                        summary: None,
                    }
                } else {
                    crate::commands::merge::merge_conflict_json(d, merge_label)
                }
            })
            .collect(),
    );
    (Some(paths), conflict_details)
}

impl HumanRender for PullOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(r) = &self.refresh {
            render_refresh_head(w, r)?;
            if r.dry_run {
                return Ok(());
            }
        }
        if let Some(b) = &self.server_branch {
            return render_server_branch(w, self, b);
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
                for item in self.conflict_details.as_deref().unwrap_or(&[]) {
                    outln!(w, "CONFLICT ({}): {}", item.kind, item.path);
                }
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
            self.transfer.render_rows(w, stdout_color())?;
        }
        Ok(())
    }
}

/// The `CONFLICT (...)` lines of a pull that stopped, on stderr.
fn print_conflicts_to_stderr(outcome: &PullOutcome) {
    if let Some(details) = &outcome.conflict_details {
        for item in details {
            if let Some(summary) = &item.summary {
                eprintln!("CONFLICT ({kind}): {summary}", kind = item.kind);
            } else {
                eprintln!("CONFLICT ({kind}): {}", item.path, kind = item.kind);
            }
        }
    } else if let Some(paths) = &outcome.conflicts {
        eprintln!("Conflicts in:");
        for path in paths {
            eprintln!("  {path}");
        }
    }
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
            for item in outcome.conflict_details.as_deref().unwrap_or(&[]) {
                outln!(w, "CONFLICT ({}): {}", item.kind, item.path);
            }
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
        outcome.transfer.render_rows(w, stdout_color())?;
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
pub(crate) fn changes_phrase(c: &crate::commands::checkout::RefreshChangesJson) -> String {
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
        markdown::print_pull_outcome(w, self)?;
        Ok(())
    }
}

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}
