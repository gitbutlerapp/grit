//! `--json` / human / markdown rendering for `grit pull`.

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
    pub refresh: Option<crate::commands::checkout::PullRefreshJson>,
    /// What the fetch step received and where the time went, for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
}

pub fn map_outcome(outcome: LibPullOutcome) -> PullOutcome {
    let rebase_stop = outcome.result == LibPullResult::RebaseConflicts;
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
        LibPullResult::RebaseConflicts => {
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
        result: map_result(outcome.result),
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
        transfer: TransferSummary::new(outcome.transfer, Vec::new(), Default::default()),
    }
}

/// Rebase stops share `conflicts` with merge stops; `operation` tells them apart.
fn map_result(result: LibPullResult) -> PullResult {
    match result {
        LibPullResult::UpToDate => PullResult::UpToDate,
        LibPullResult::FastForward => PullResult::FastForward,
        LibPullResult::Merged => PullResult::Merged,
        LibPullResult::RefusedNotFf => PullResult::RefusedNotFf,
        LibPullResult::Conflicts | LibPullResult::RebaseConflicts => PullResult::Conflicts,
        LibPullResult::SetUpstream => PullResult::SetUpstream,
        LibPullResult::Rebased => PullResult::Rebased,
        LibPullResult::AutostashConflict => PullResult::AutostashConflict,
    }
}

fn map_check(check: LibPullCheck) -> PullCheckJson {
    PullCheckJson {
        ahead: check.ahead,
        behind: check.behind,
        fast_forward_possible: check.fast_forward_possible,
        would: map_result(check.would),
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
        if let Some(r) = &self.refresh {
            if r.moved {
                outln!(
                    w,
                    "Refreshed onto {} at {} (snapshot {})",
                    r.target,
                    short_hex(&r.base),
                    short_hex(&r.snapshot)
                );
            } else {
                outln!(
                    w,
                    "{} has not moved since this checkout's snapshot.",
                    r.target
                );
            }
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
                if let Some(details) = &self.conflict_details {
                    for item in details {
                        if let Some(summary) = &item.summary {
                            eprintln!("CONFLICT ({kind}): {summary}", kind = item.kind);
                        } else {
                            eprintln!("CONFLICT ({kind}): {}", item.path, kind = item.kind);
                        }
                    }
                } else if let Some(paths) = &self.conflicts {
                    eprintln!("Conflicts in:");
                    for path in paths {
                        eprintln!("  {path}");
                    }
                }
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
            PullResult::AutostashConflict => {
                outln!(
                    w,
                    "Pulled {}, but re-applying autostash left conflicts; the stash entry was kept.",
                    self.branch
                );
            }
        }
        if self.transfer.received_anything() {
            outln!(w);
            self.transfer.render_rows(w, stdout_color())?;
        }
        Ok(())
    }
}

fn check_would_phrase(result: PullResult) -> &'static str {
    match result {
        PullResult::UpToDate => "leave the branch up to date",
        PullResult::FastForward => "fast-forward",
        PullResult::Merged => "merge",
        PullResult::Rebased => "rebase",
        PullResult::SetUpstream => "set upstream",
        PullResult::RefusedNotFf => "refuse (not fast-forward)",
        PullResult::Conflicts => "stop with conflicts",
        PullResult::AutostashConflict => "conflict on autostash restore",
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
