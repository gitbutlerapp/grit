//! `grit merge` — merge another branch into the current one.
//!
//! Fast-forwards when possible; otherwise performs a real three-way merge and
//! records a merge commit. Conflicts leave merge-in-progress state with conflict
//! markers in the worktree.

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{
    abort_branch_merge, integrate_branch, IntegrateBranchOutcome, MergeConflictDetail,
    MergeConflictKind,
};
use grit_lib::porcelain::worktree_guard::ensure_worktree_clean_for_merge;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// Merge / pull integration result (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeResult {
    UpToDate,
    FastForward,
    Merged,
    Conflicts,
    Aborted,
    SetUpstream,
}

/// Result of `grit merge` (and the merge half of `grit pull`).
#[derive(Serialize, JsonSchema)]
pub struct MergeOutcome {
    pub result: MergeResult,
    /// The merge source label (for `set_upstream`, the branch being set).
    pub branch: String,
    /// Resulting commit (fast-forward target, merge commit, or adopted upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// For `set_upstream` (unborn `grit pull`): the upstream the branch was set to.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Conflicted paths when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
    /// Structured conflict metadata when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflict_details: Option<Vec<MergeConflictJson>>,
}

/// One conflicted path in `--json` merge output.
#[derive(Serialize, JsonSchema)]
pub struct MergeConflictJson {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl MergeOutcome {
    fn up_to_date(label: &str) -> Self {
        Self {
            result: MergeResult::UpToDate,
            branch: label.to_owned(),
            oid: None,
            upstream: None,
            conflicts: None,
            conflict_details: None,
        }
    }

    fn fast_forward(label: &str, oid: ObjectId) -> Self {
        Self {
            result: MergeResult::FastForward,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: None,
            conflicts: None,
            conflict_details: None,
        }
    }

    fn merged(label: &str, oid: ObjectId) -> Self {
        Self {
            result: MergeResult::Merged,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: None,
            conflicts: None,
            conflict_details: None,
        }
    }

    fn conflicts(label: &str, details: Vec<MergeConflictDetail>) -> Self {
        let paths: Vec<String> = details.iter().map(|d| d.path.clone()).collect();
        let conflict_details = Some(
            details
                .iter()
                .map(|d| merge_conflict_json(d, label))
                .collect(),
        );
        Self {
            result: MergeResult::Conflicts,
            branch: label.to_owned(),
            oid: None,
            upstream: None,
            conflicts: Some(paths),
            conflict_details,
        }
    }

    fn aborted() -> Self {
        Self {
            result: MergeResult::Aborted,
            branch: String::new(),
            oid: None,
            upstream: None,
            conflicts: None,
            conflict_details: None,
        }
    }

    /// `grit pull` on an unborn branch adopting the upstream as the first commit.
    pub fn set_upstream(branch: &str, upstream: &str, oid: ObjectId) -> Self {
        Self {
            result: MergeResult::SetUpstream,
            branch: branch.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: Some(upstream.to_owned()),
            conflicts: None,
            conflict_details: None,
        }
    }
}

fn merge_conflict_json(detail: &MergeConflictDetail, merge_label: &str) -> MergeConflictJson {
    let (kind, summary) = match detail.kind {
        MergeConflictKind::Content => ("content".to_owned(), None),
        MergeConflictKind::AddAdd => ("add_add".to_owned(), None),
        MergeConflictKind::ModifyDelete { deleted_in_current } => {
            let summary = if deleted_in_current {
                Some(format!(
                    "{} deleted on the current branch and modified in {merge_label}; {merge_label}'s version is in the work tree",
                    detail.path
                ))
            } else {
                Some(format!(
                    "{} modified on the current branch and deleted in {merge_label}; the current branch's version is in the work tree",
                    detail.path
                ))
            };
            ("modify_delete".to_owned(), summary)
        }
    };
    MergeConflictJson {
        path: detail.path.clone(),
        kind,
        summary,
    }
}

impl HumanRender for MergeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let short = self.oid.as_deref().map(short_hex).unwrap_or_default();
        match self.result {
            MergeResult::UpToDate => outln!(w, "Already up to date."),
            MergeResult::FastForward => outln!(w, "Fast-forwarded {} → {short}", self.branch),
            MergeResult::Merged => outln!(
                w,
                "Merged {} into the current branch ({short})",
                self.branch
            ),
            MergeResult::Conflicts => {
                if let Some(details) = &self.conflict_details {
                    for item in details {
                        if let Some(summary) = &item.summary {
                            eprintln!("CONFLICT ({kind}): {summary}", kind = item.kind);
                        } else {
                            eprintln!("CONFLICT ({kind}): {}", item.path, kind = item.kind);
                        }
                    }
                } else {
                    eprintln!("Merge has conflicts in:");
                    for path in self.conflicts.as_deref().unwrap_or(&[]) {
                        eprintln!("  {path}");
                    }
                }
                eprintln!(
                    "\nAutomatic merge failed; fix conflicts and run `grit commit`, or run `grit merge --abort`."
                );
            }
            MergeResult::Aborted => outln!(w, "Merge aborted."),
            MergeResult::SetUpstream => outln!(
                w,
                "Set {} to {}.",
                self.branch,
                self.upstream.as_deref().unwrap_or_default()
            ),
        }
        Ok(())
    }
}

impl MarkdownRender for MergeOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_merge_outcome(w, self)?;
        Ok(())
    }
}

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run(branch: Option<&str>, abort: bool) -> Result<MergeOutcome> {
    let repo = context::discover()?;

    if abort {
        abort_branch_merge(&repo).map_err(anyhow::Error::new)?;
        return Ok(MergeOutcome::aborted());
    }

    let branch = branch.context("branch name required unless using --abort")?;

    ensure_worktree_clean_for_merge(&repo)
        .map_err(anyhow::Error::new)
        .context("could not verify worktree is clean")?;

    let (refname, head_oid) = match resolve_head(repo.git_dir())? {
        HeadState::Branch {
            refname,
            oid: Some(oid),
            ..
        } => (refname, oid),
        HeadState::Branch { .. } => bail!("no commits yet on this branch"),
        HeadState::Detached { .. } => bail!("HEAD is detached; grit merge needs a branch"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    };

    let other_oid = resolve_branch(&repo, branch)?;
    integrate(&repo, &refname, head_oid, other_oid, branch)
}

/// Integrate `other` into the branch `into_ref` (currently at `into_oid`):
/// up-to-date, fast-forward, or three-way merge. Shared with `grit pull`.
pub fn integrate(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
) -> Result<MergeOutcome> {
    match integrate_branch(repo, into_ref, into_oid, other_oid, label)? {
        IntegrateBranchOutcome::UpToDate => Ok(MergeOutcome::up_to_date(label)),
        IntegrateBranchOutcome::FastForward { oid } => Ok(MergeOutcome::fast_forward(label, oid)),
        IntegrateBranchOutcome::Merged { oid } => Ok(MergeOutcome::merged(label, oid)),
        IntegrateBranchOutcome::Conflicts { details, .. } => {
            Ok(MergeOutcome::conflicts(label, details))
        }
    }
}

/// Resolve a branch name to a commit, trying local then remote-tracking refs.
fn resolve_branch(repo: &Repository, name: &str) -> Result<ObjectId> {
    for candidate in [
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        name.to_owned(),
    ] {
        if let Ok(oid) = refs::resolve_ref(repo.git_dir(), &candidate) {
            return Ok(oid);
        }
    }
    bail!("no branch named '{name}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::objects::ObjectId;

    #[test]
    fn merge_result_serializes_snake_case() {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
        let cases = [
            (MergeOutcome::up_to_date("topic"), "up_to_date"),
            (MergeOutcome::fast_forward("topic", oid), "fast_forward"),
            (MergeOutcome::merged("topic", oid), "merged"),
            (
                MergeOutcome::conflicts(
                    "topic",
                    vec![MergeConflictDetail {
                        path: "a.txt".into(),
                        kind: MergeConflictKind::Content,
                    }],
                ),
                "conflicts",
            ),
            (MergeOutcome::aborted(), "aborted"),
            (
                MergeOutcome::set_upstream("main", "origin/main", oid),
                "set_upstream",
            ),
        ];
        for (outcome, expected) in cases {
            let value = serde_json::to_value(&outcome).expect("json");
            assert_eq!(value["result"].as_str(), Some(expected), "{value}");
        }
    }
}
