//! `grit merge` — merge another branch into the current one.
//!
//! Fast-forwards when possible; otherwise performs a real three-way merge and
//! records a merge commit. Conflicts leave merge-in-progress state with conflict
//! markers in the worktree.

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{abort_branch_merge, integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::worktree_guard::ensure_worktree_clean_for_merge;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use serde::Serialize;
use std::io::Write;

/// Result of `grit merge` (and the merge half of `grit pull`).
#[derive(Serialize)]
pub struct MergeOutcome {
    /// `up_to_date` | `fast_forward` | `merged` | `conflicts` | `aborted` | `set_upstream`.
    pub result: String,
    /// The merge source label (for `set_upstream`, the branch being set).
    pub branch: String,
    /// Resulting commit (fast-forward target, merge commit, or adopted upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// For `set_upstream` (unborn `grit pull`): the upstream the branch was set to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Conflicted paths when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
}

impl MergeOutcome {
    fn up_to_date(label: &str) -> Self {
        Self {
            result: "up_to_date".to_owned(),
            branch: label.to_owned(),
            oid: None,
            upstream: None,
            conflicts: None,
        }
    }

    fn fast_forward(label: &str, oid: ObjectId) -> Self {
        Self {
            result: "fast_forward".to_owned(),
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: None,
            conflicts: None,
        }
    }

    fn merged(label: &str, oid: ObjectId) -> Self {
        Self {
            result: "merged".to_owned(),
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: None,
            conflicts: None,
        }
    }

    fn conflicts(label: &str, paths: Vec<String>) -> Self {
        Self {
            result: "conflicts".to_owned(),
            branch: label.to_owned(),
            oid: None,
            upstream: None,
            conflicts: Some(paths),
        }
    }

    fn aborted() -> Self {
        Self {
            result: "aborted".to_owned(),
            branch: String::new(),
            oid: None,
            upstream: None,
            conflicts: None,
        }
    }

    /// `grit pull` on an unborn branch adopting the upstream as the first commit.
    pub fn set_upstream(branch: &str, upstream: &str, oid: ObjectId) -> Self {
        Self {
            result: "set_upstream".to_owned(),
            branch: branch.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: Some(upstream.to_owned()),
            conflicts: None,
        }
    }
}

impl HumanRender for MergeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let short = self.oid.as_deref().map(short_hex).unwrap_or_default();
        match self.result.as_str() {
            "up_to_date" => outln!(w, "Already up to date."),
            "fast_forward" => outln!(w, "Fast-forwarded {} → {short}", self.branch),
            "merged" => outln!(
                w,
                "Merged {} into the current branch ({short})",
                self.branch
            ),
            "conflicts" => {
                eprintln!("Merge has conflicts in:");
                for path in self.conflicts.as_deref().unwrap_or(&[]) {
                    eprintln!("  {path}");
                }
                eprintln!(
                    "\nAutomatic merge failed; fix conflicts and run `grit commit`, or run `grit merge --abort`."
                );
            }
            "aborted" => outln!(w, "Merge aborted."),
            "set_upstream" => outln!(
                w,
                "Set {} to {}.",
                self.branch,
                self.upstream.as_deref().unwrap_or_default()
            ),
            other => outln!(w, "{other}"),
        }
        Ok(())
    }
}

impl MarkdownRender for MergeOutcome {}

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

    let (refname, head_oid) = match resolve_head(&repo.git_dir)? {
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
        IntegrateBranchOutcome::Conflicts { paths } => Ok(MergeOutcome::conflicts(label, paths)),
    }
}

/// Resolve a branch name to a commit, trying local then remote-tracking refs.
fn resolve_branch(repo: &Repository, name: &str) -> Result<ObjectId> {
    for candidate in [
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        name.to_owned(),
    ] {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &candidate) {
            return Ok(oid);
        }
    }
    bail!("no branch named '{name}'")
}
