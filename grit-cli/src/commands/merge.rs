//! `grit merge` — merge another branch into the current one.
//!
//! Fast-forwards when possible; otherwise performs a real three-way merge and
//! records a merge commit. Conflicts leave merge-in-progress state with conflict
//! markers in the worktree.

use crate::context;
use crate::markdown;
use crate::operation_output::{render_conflicts_human, ConflictJson, OperationConflictBlock};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use anyhow::{bail, Context, Result};
use grit_lib::change_id::ChangeId;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{Conflict, ConflictKind};
use grit_lib::porcelain::merge::{
    abort_branch_merge, integrate_branch, predict_merge, IntegrateBranchOutcome,
    MergeConflictDetail, MergePrediction, MergePredictionOptions,
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
    /// Three-way merge would succeed (`grit merge --check` only).
    Clean,
    Merged,
    Conflicts,
    Aborted,
    #[allow(dead_code)]
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
    /// Abort/skip options when `result` is `conflicts` (not part of the ordered sequence).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// True for `grit merge --check` (prediction only; nothing was merged).
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) prediction: bool,
}

/// One conflicted path in `--json` output (pull v1 `conflict_details` and merge v2 items).
#[derive(Clone, Serialize, JsonSchema)]
pub struct MergeConflictJson {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

pub(crate) fn merge_conflict_json(
    detail: &MergeConflictDetail,
    merge_label: &str,
) -> MergeConflictJson {
    let (kind, summary) = match detail.kind {
        ConflictKind::Content => ("content".to_owned(), None),
        ConflictKind::AddAdd => ("add_add".to_owned(), None),
        ConflictKind::ModifyDelete { deleted_in_current } => {
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

impl MergeOutcome {
    #[must_use]
    fn with_prediction(mut self, prediction: bool) -> Self {
        self.prediction = prediction;
        self
    }

    fn up_to_date(label: &str) -> Self {
        Self {
            result: MergeResult::UpToDate,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            prediction: false,
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
            alternatives: None,
            prediction: false,
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
            alternatives: None,
            prediction: false,
        }
    }

    fn clean_check(label: &str, tree: ObjectId) -> Self {
        Self {
            result: MergeResult::Clean,
            branch: label.to_owned(),
            oid: Some(tree.to_hex()),
            change_id: None,
            upstream: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            prediction: false,
        }
    }

    fn conflicts_check(label: &str, details: Vec<MergeConflictDetail>) -> Self {
        let conflicts = Some(
            details
                .iter()
                .map(|d| merge_conflict_json(d, label))
                .map(|j| ConflictJson {
                    path: j.path,
                    kind: j.kind,
                    summary: j.summary,
                })
                .collect(),
        );
        Self {
            result: MergeResult::Conflicts,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts,
            next_steps: None,
            alternatives: None,
            prediction: false,
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
            alternatives: block.alternatives,
            prediction: false,
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
            alternatives: None,
            prediction: false,
        }
    }

    /// Reserved for historical merge JSON (`set_upstream` moved to `grit pull`).
    #[allow(dead_code)]
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
            alternatives: None,
            prediction: false,
        }
    }
}

impl HumanRender for MergeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let short = self.oid.as_deref().map(short_hex).unwrap_or_default();
        match self.result {
            MergeResult::UpToDate => outln!(w, "Already up to date."),
            MergeResult::FastForward if self.prediction => {
                outln!(w, "Would fast-forward {} → {short}", self.branch);
            }
            MergeResult::FastForward => outln!(w, "Fast-forwarded {} → {short}", self.branch),
            MergeResult::Clean => outln!(w, "Would merge {} cleanly (tree {short})", self.branch),
            MergeResult::Merged => outln!(
                w,
                "Merged {} into the current branch ({short})",
                self.branch
            ),
            MergeResult::Conflicts if self.prediction => {
                if let Some(details) = &self.conflicts {
                    render_conflicts_human(details, w)?;
                }
                crate::operation_output::render_merge_check_conflict_footer_human();
            }
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

pub fn run(branch: Option<&str>, abort: bool, check: bool) -> Result<MergeOutcome> {
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
    if check {
        return check_merge(&repo, head_oid, other_oid, branch);
    }
    integrate(&repo, &refname, head_oid, other_oid, branch)
}

fn check_merge(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
) -> Result<MergeOutcome> {
    let options = MergePredictionOptions {
        theirs_label: label.to_owned(),
        ..MergePredictionOptions::default()
    };
    match predict_merge(repo, into_oid, other_oid, &options)? {
        MergePrediction::UpToDate => Ok(MergeOutcome::up_to_date(label).with_prediction(true)),
        MergePrediction::FastForward => {
            Ok(
                MergeOutcome::fast_forward(label, other_oid, stored_change_id(repo, other_oid)?)
                    .with_prediction(true),
            )
        }
        MergePrediction::Clean { tree } => {
            Ok(MergeOutcome::clean_check(label, tree).with_prediction(true))
        }
        MergePrediction::Conflicts { conflicts } => {
            Ok(MergeOutcome::conflicts_check(label, conflicts).with_prediction(true))
        }
        MergePrediction::Unrelated => {
            bail!("'{label}' has no common history with the current branch")
        }
    }
}

fn stored_change_id(repo: &Repository, oid: ObjectId) -> Result<Option<ChangeId>> {
    let obj = repo.odb().read(&oid)?;
    Ok(grit_lib::change_id::change_id(
        &grit_lib::objects::parse_commit(&obj.data)?,
    ))
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
    fn merge_check_exit_code_and_json() {
        let fixture = CliFixture::empty();
        std::fs::write(fixture.path().join("a.txt"), "base\n").unwrap();
        fixture.run_ok(&["add", "a.txt"]);
        fixture.run_ok(&["commit", "-m", "base"]);
        fixture.run_ok(&["switch", "--create", "side"]);
        std::fs::write(fixture.path().join("a.txt"), "side\n").unwrap();
        fixture.run_ok(&["commit", "-m", "side"]);
        fixture.run_ok(&["switch", "main"]);
        std::fs::write(fixture.path().join("b.txt"), "main only\n").unwrap();
        fixture.run_ok(&["add", "b.txt"]);
        fixture.run_ok(&["commit", "-m", "main"]);

        let clean = fixture.run(&["--json", "merge", "--check", "side"]);
        assert_eq!(
            clean.status,
            Some(0),
            "{}",
            clean.dump("merge --check clean")
        );
        let v: serde_json::Value = serde_json::from_str(&clean.stdout).expect("json");
        assert_eq!(v["result"], "clean");
        assert!(v["oid"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(v["next_steps"].is_null() || v.get("next_steps").is_none());

        std::fs::write(fixture.path().join("a.txt"), "main edit\n").unwrap();
        fixture.run_ok(&["commit", "-m", "main conflict"]);
        let head_before = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();

        let conflict = fixture.run(&["--json", "merge", "--check", "side"]);
        assert_eq!(
            conflict.status,
            Some(1),
            "{}",
            conflict.dump("merge --check conflict")
        );
        let v: serde_json::Value = serde_json::from_str(&conflict.stdout).expect("json");
        assert_eq!(v["result"], "conflicts");
        assert!(v["conflicts"].as_array().is_some_and(|a| !a.is_empty()));
        let head_after = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        assert_eq!(
            head_before, head_after,
            "HEAD must be unchanged after --check"
        );
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
        let alts = v["alternatives"].as_array().expect("alternatives");
        assert!(
            alts.iter()
                .any(|s| s.as_str() == Some("grit merge --abort")),
            "{alts:?}"
        );
    }
}
