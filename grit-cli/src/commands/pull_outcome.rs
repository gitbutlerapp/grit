//! `--json` / human / markdown rendering for `grit pull`.

use crate::commands::merge::MergeConflictJson;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub check: Option<PullCheckJson>,
}

pub fn map_outcome(outcome: LibPullOutcome) -> PullOutcome {
    let (conflicts, conflict_details) = outcome
        .conflicts
        .as_ref()
        .map(|details| map_conflicts(details, &outcome.label))
        .unwrap_or((None, None));
    PullOutcome {
        result: match outcome.result {
            LibPullResult::UpToDate => PullResult::UpToDate,
            LibPullResult::FastForward => PullResult::FastForward,
            LibPullResult::Merged => PullResult::Merged,
            LibPullResult::RefusedNotFf => PullResult::RefusedNotFf,
            LibPullResult::Conflicts => PullResult::Conflicts,
            LibPullResult::SetUpstream => PullResult::SetUpstream,
            LibPullResult::Rebased => PullResult::Rebased,
            LibPullResult::AutostashConflict => PullResult::AutostashConflict,
        },
        branch: outcome.label,
        oid: outcome.oid.as_ref().map(ObjectId::to_hex),
        change_id: outcome
            .change_id
            .as_ref()
            .map(|id: &ChangeId| id.as_str().to_owned()),
        upstream: outcome.upstream,
        conflicts,
        conflict_details,
        ahead: outcome.ahead,
        behind: outcome.behind,
        check: outcome.check.map(map_check),
    }
}

fn map_check(check: LibPullCheck) -> PullCheckJson {
    PullCheckJson {
        ahead: check.ahead,
        behind: check.behind,
        fast_forward_possible: check.fast_forward_possible,
        would: match check.would {
            LibPullResult::UpToDate => PullResult::UpToDate,
            LibPullResult::FastForward => PullResult::FastForward,
            LibPullResult::Merged => PullResult::Merged,
            LibPullResult::RefusedNotFf => PullResult::RefusedNotFf,
            LibPullResult::Conflicts => PullResult::Conflicts,
            LibPullResult::SetUpstream => PullResult::SetUpstream,
            LibPullResult::Rebased => PullResult::Rebased,
            LibPullResult::AutostashConflict => PullResult::AutostashConflict,
        },
    }
}

fn map_conflicts(
    details: &[MergeConflictDetail],
    merge_label: &str,
) -> (Option<Vec<String>>, Option<Vec<MergeConflictJson>>) {
    let paths: Vec<String> = details.iter().map(|d| d.path.clone()).collect();
    let conflict_details = Some(
        details
            .iter()
            .map(|d| crate::commands::merge::merge_conflict_json(d, merge_label))
            .collect(),
    );
    (Some(paths), conflict_details)
}

impl HumanRender for PullOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
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
                if self.oid.is_some() {
                    eprintln!(
                        "\nRebase stopped with conflicts; the branch, index, and worktree are unchanged."
                    );
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
