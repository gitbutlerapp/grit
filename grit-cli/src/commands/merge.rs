//! `grit merge` — merge another branch into the current one.
//!
//! Fast-forwards when possible; otherwise performs a real three-way merge and
//! records a merge commit. Conflicts are reported (without leaving a
//! half-finished state) — resolving them is out of scope for `grit`.

use crate::context;
use crate::output::{HumanRender, MarkdownRender};
use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::merge_base::{is_ancestor, merge_bases_first_vs_rest};
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::{CommitData, ObjectId};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::commit::write_commit_object;
use grit_lib::porcelain::worktree_guard::{ensure_worktree_clean_for_merge, prepare_tree_checkout};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use serde::Serialize;

/// Result of `grit merge` (and the merge half of `grit pull`).
#[derive(Serialize)]
pub struct MergeOutcome {
    /// `up_to_date` | `fast_forward` | `merged` | `set_upstream`.
    pub result: String,
    /// The merge source label (for `set_upstream`, the branch being set).
    pub branch: String,
    /// Resulting commit (fast-forward target, merge commit, or adopted upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// For `set_upstream` (unborn `grit pull`): the upstream the branch was set to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

impl MergeOutcome {
    fn up_to_date(label: &str) -> Self {
        Self {
            result: "up_to_date".to_owned(),
            branch: label.to_owned(),
            oid: None,
            upstream: None,
        }
    }

    fn fast_forward(label: &str, oid: ObjectId) -> Self {
        Self {
            result: "fast_forward".to_owned(),
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: None,
        }
    }

    fn merged(label: &str, oid: ObjectId) -> Self {
        Self {
            result: "merged".to_owned(),
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: None,
        }
    }

    /// `grit pull` on an unborn branch adopting the upstream as the first commit.
    pub fn set_upstream(branch: &str, upstream: &str, oid: ObjectId) -> Self {
        Self {
            result: "set_upstream".to_owned(),
            branch: branch.to_owned(),
            oid: Some(oid.to_hex()),
            upstream: Some(upstream.to_owned()),
        }
    }
}

impl HumanRender for MergeOutcome {
    fn render_human(&self) {
        let short = self.oid.as_deref().map(short_hex).unwrap_or_default();
        match self.result.as_str() {
            "up_to_date" => println!("Already up to date."),
            "fast_forward" => println!("Fast-forwarded {} → {short}", self.branch),
            "merged" => println!("Merged {} into the current branch ({short})", self.branch),
            "set_upstream" => println!(
                "Set {} to {}.",
                self.branch,
                self.upstream.as_deref().unwrap_or_default()
            ),
            other => println!("{other}"),
        }
    }
}

impl MarkdownRender for MergeOutcome {}

fn short_hex(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub fn run(branch: &str) -> Result<MergeOutcome> {
    let repo = context::discover()?;

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
    if into_oid == other_oid || is_ancestor(repo, other_oid, into_oid)? {
        return Ok(MergeOutcome::up_to_date(label));
    }

    let into_tree = context::commit_tree(repo, &into_oid)?;
    let other_tree = context::commit_tree(repo, &other_oid)?;

    if is_ancestor(repo, into_oid, other_oid)? {
        prepare_tree_checkout(repo, Some(&into_tree), &other_tree)
            .map_err(anyhow::Error::new)
            .context("could not verify working tree")?;
        checkout_between_trees(repo, Some(&into_tree), &other_tree)
            .context("could not update the working tree")?;
        move_branch(
            repo,
            into_ref,
            into_oid,
            other_oid,
            &format!("merge {label}: fast-forward"),
        )?;
        return Ok(MergeOutcome::fast_forward(label, other_oid));
    }

    let base_oid = merge_bases_first_vs_rest(repo, into_oid, &[other_oid])?
        .into_iter()
        .next()
        .with_context(|| format!("'{label}' has no common history with the current branch"))?;
    let base_tree = context::commit_tree(repo, &base_oid)?;

    let merged = merge_trees_three_way(
        repo,
        base_tree,
        into_tree,
        other_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .context("could not merge")?;

    if !merged.conflict_content.is_empty() {
        let mut paths: Vec<String> = merged
            .conflict_content
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect();
        paths.sort();
        bail!(
            "merge has conflicts in:\n  {}\n\nNothing was changed. grit can't resolve conflicts yet — run `git merge {label}` to resolve them.",
            paths.join("\n  ")
        );
    }

    let mut index = merged.index;
    let merged_tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())
        .context("could not write merged tree")?;
    prepare_tree_checkout(repo, Some(&into_tree), &merged_tree)
        .map_err(anyhow::Error::new)
        .context("could not verify working tree")?;
    checkout_between_trees(repo, Some(&into_tree), &merged_tree)
        .context("could not update the working tree")?;

    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(&repo.git_dir), true).context("could not load config")?;
    let now = context::wall_clock_now(env);
    let author = context::identity(env, &config, IdentRole::Author, "GIT_AUTHOR_DATE", now)?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        now,
    )?;

    let commit = CommitData {
        tree: merged_tree,
        parents: vec![into_oid, other_oid],
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("Merge {label}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let oid = write_commit_object(repo, &commit, None).context("could not store merge commit")?;

    move_branch(repo, into_ref, into_oid, oid, &format!("merge {label}"))?;
    Ok(MergeOutcome::merged(label, oid))
}

/// Point a branch (and HEAD's reflog) at `new`, logging the transition.
fn move_branch(
    repo: &Repository,
    refname: &str,
    old: ObjectId,
    new: ObjectId,
    reason: &str,
) -> Result<()> {
    refs::write_ref(&repo.git_dir, refname, &new).context("could not update branch")?;
    let env = repo.environment();
    let config = ConfigSet::load(env, Some(&repo.git_dir), true).unwrap_or_default();
    let who = context::reflog_identity(env, &config, context::wall_clock_now(env));
    let _ = refs::append_reflog(&repo.git_dir, refname, &old, &new, &who, reason, false);
    let _ = refs::append_reflog(&repo.git_dir, "HEAD", &old, &new, &who, reason, false);
    Ok(())
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
