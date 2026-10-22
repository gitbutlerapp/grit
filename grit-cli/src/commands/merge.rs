//! `grit merge` — merge another branch into the current one.
//!
//! Fast-forwards when possible; otherwise performs a real three-way merge and
//! records a merge commit. Conflicts leave merge-in-progress state with conflict
//! markers in the worktree.

use crate::context;
use crate::markdown;
use crate::operation_output::{
    render_conflicts_human, ConflictJson, OperationConflictBlock,
};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use anyhow::{bail, Context, Result};
use grit_lib::change_id::ChangeId;
use grit_lib::objects::ObjectId;
use grit_lib::operation::Conflict;
use grit_lib::porcelain::merge::{
    abort_branch_merge, integrate_branch, IntegrateBranchOutcome, MergeConflictDetail,
};
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
    /// Stored `change-id` on the resulting commit when `oid` is set and a header is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// For `set_upstream` (unborn `grit pull`): the upstream the branch was set to.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Structured conflicts when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    /// Suggested `grit` commands when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
}

impl MergeOutcome {
    fn up_to_date(label: &str) -> Self {
        Self {
            result: MergeResult::UpToDate,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: None,
            next_steps: None,
        }
    }

    fn fast_forward(label: &str, oid: ObjectId, change_id: Option<ChangeId>) -> Self {
        Self {
            result: MergeResult::FastForward,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            upstream: None,
            conflicts: None,
            next_steps: None,
        }
    }

    fn merged(label: &str, oid: ObjectId, change_id: Option<ChangeId>) -> Self {
        Self {
            result: MergeResult::Merged,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            upstream: None,
            conflicts: None,
            next_steps: None,
        }
    }

    fn conflicts(label: &str, details: Vec<MergeConflictDetail>) -> Self {
        let library_conflicts: Vec<Conflict> = details
            .iter()
            .map(|d| Conflict {
                path: d.path.clone(),
                kind: d.kind,
            })
            .collect();
        let block = OperationConflictBlock::for_merge(&library_conflicts, label);
        Self {
            result: MergeResult::Conflicts,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: Some(block.conflicts),
            next_steps: Some(block.next_steps),
        }
    }

    fn aborted() -> Self {
        Self {
            result: MergeResult::Aborted,
            branch: String::new(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: None,
            next_steps: None,
        }
    }

    /// `grit pull` on an unborn branch adopting the upstream as the first commit.
    pub fn set_upstream(
        branch: &str,
        upstream: &str,
        oid: ObjectId,
        change_id: Option<String>,
    ) -> Self {
        Self {
            result: MergeResult::SetUpstream,
            branch: branch.to_owned(),
            oid: Some(oid.to_hex()),
            change_id,
            upstream: Some(upstream.to_owned()),
            conflicts: None,
            next_steps: None,
        }
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
                if let Some(details) = &self.conflicts {
                    render_conflicts_human(details, w)?;
                }
                crate::operation_output::render_merge_conflict_footer_human();
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
        IntegrateBranchOutcome::FastForward { oid, change_id } => {
            Ok(MergeOutcome::fast_forward(label, oid, change_id))
        }
        IntegrateBranchOutcome::Merged { oid, change_id } => {
            Ok(MergeOutcome::merged(label, oid, change_id))
        }
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
    use grit_lib::operation::ConflictKind;
    use grit_test_support::CliFixture;

    #[test]
    fn merge_result_serializes_snake_case() {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
        let cases = [
            (MergeOutcome::up_to_date("topic"), "up_to_date"),
            (
                MergeOutcome::fast_forward("topic", oid, None),
                "fast_forward",
            ),
            (MergeOutcome::merged("topic", oid, None), "merged"),
            (
                MergeOutcome::conflicts(
                    "topic",
                    vec![MergeConflictDetail {
                        path: "a.txt".into(),
                        kind: ConflictKind::Content,
                    }],
                ),
                "conflicts",
            ),
            (MergeOutcome::aborted(), "aborted"),
            (
                MergeOutcome::set_upstream("main", "origin/main", oid, None),
                "set_upstream",
            ),
        ];
        for (outcome, expected) in cases {
            let value = serde_json::to_value(&outcome).expect("json");
            assert_eq!(value["result"].as_str(), Some(expected), "{value}");
        }
    }

    #[test]
    fn merge_conflict_json_shape() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("f.txt"), "base\n").unwrap();
        fixture.run_ok(&["add", "f.txt"]);
        fixture.run_ok(&["commit", "-m", "base"]);
        fixture.run_ok(&["switch", "--create", "side"]);
        std::fs::write(fixture.path().join("f.txt"), "side\n").unwrap();
        fixture.run_ok(&["commit", "-m", "side"]);
        fixture.run_ok(&["switch", "main"]);
        std::fs::write(fixture.path().join("f.txt"), "main\n").unwrap();
        fixture.run_ok(&["commit", "-m", "main"]);

        let out = fixture.run(&["--json", "merge", "side"]);
        assert_eq!(out.status, Some(1), "{}", out.dump("merge"));
        let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
        assert_eq!(v["result"], "conflicts");
        let conflicts = v["conflicts"].as_array().expect("conflicts array");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0]["path"], "f.txt");
        assert_eq!(conflicts[0]["kind"], "content");
        let next = v["next_steps"].as_array().expect("next_steps");
        assert!(
            next.iter().any(|s| s.as_str() == Some("grit diff")),
            "{next:?}"
        );
        assert!(
            next.iter()
                .any(|s| s.as_str() == Some("grit merge --abort")),
            "{next:?}"
        );
    }
}
