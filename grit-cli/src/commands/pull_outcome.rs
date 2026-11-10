//! Constructors that turn library pull results into the CLI's [`PullOutcome`].

use crate::commands::checkout::{PullRefreshJson, PullServerBranchJson};
use crate::operation_output::OperationConflictBlock;
use crate::progress::summary::TransferSummary;
use crate::render::conflict::MergeConflictJson;
use crate::render::pull::{PullCheckJson, PullOutcome, PullResult};
use grit_lib::change_id::ChangeId;
use grit_lib::objects::ObjectId;
use grit_lib::operation::Conflict;
use grit_lib::porcelain::pull::{
    PullCheck as LibPullCheck, PullOutcome as LibPullOutcome, PullResult as LibPullResult,
};
use grit_lib::remote::snapshot_checkout::{
    BranchInfo, BranchRefresh, BranchRefreshResult, CheckoutMarker, CheckoutState, RefreshOutcome,
    RefreshedHead,
};
use grit_lib::remote::transfer::TransferStats;

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
            // A result this CLI does not know yet: report the branch as updated.
            _ => Self::Merged,
        }
    }
}

pub fn map_outcome(outcome: LibPullOutcome) -> PullOutcome {
    let rebase_stop = matches!(
        outcome.result,
        LibPullResult::RebaseConflicts | LibPullResult::RebaseStopped
    );
    let merge_label = outcome.label.clone();
    let details = outcome.conflicts.as_deref().unwrap_or(&[]);
    let conflict_sources = outcome.conflicts.clone();
    let conflict_merge_label = if rebase_stop {
        None
    } else {
        Some(merge_label.clone())
    };
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
        conflict_sources,
        conflict_merge_label,
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
    pub(crate) fn bare(result: PullResult, branch: String) -> Self {
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
            conflict_sources: None,
            conflict_merge_label: None,
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
    details: &[Conflict],
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
