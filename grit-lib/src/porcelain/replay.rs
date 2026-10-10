//! Single-commit replay (cherry-pick and revert) onto the current branch.
//!
//! [`replay_commit`] performs a three-way merge, updates the working tree and
//! index on success, writes a new commit, and advances the checked-out branch
//! through [`crate::refs::update_branch_for_commit_with_config`]. On conflict or
//! when the replay would be a no-op (`Empty`, `AlreadyApplied`), nothing in the
//! worktree, index, or refs is changed.

use crate::error::{Error, Result};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, CommitData, ObjectId, ObjectKind};
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::commit::write_commit_object;
use crate::porcelain::revert::merge_commit_message_for_revert;
use crate::porcelain::worktree_guard::{ensure_worktree_clean_for_pick, prepare_tree_checkout};
use crate::refs::{update_branch_for_commit_with_config, BranchCommitRefUpdate};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

/// Whether [`replay_commit`] applies a commit's patch or its inverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayDirection {
    /// Cherry-pick: apply the diff from the commit's first parent to the commit.
    Pick,
    /// Revert: apply the inverse of that diff.
    Revert,
}

/// Inputs for [`replay_commit`].
#[derive(Debug, Clone)]
pub struct ReplayRequest {
    /// Commit object to replay (must have exactly one parent).
    pub commit: ObjectId,
    /// Pick or revert semantics for the three-way merge and commit message.
    pub direction: ReplayDirection,
    /// Committer identity in Git header form: `Name <email> <unix-time> <tz>`.
    pub committer: String,
    /// When set, replaces the default commit message (pick: source message; revert: revert template).
    pub message_override: Option<String>,
}

/// Result of [`replay_commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// A new commit was created and the branch was advanced.
    Committed {
        /// OID of the new commit.
        oid: ObjectId,
        /// Tree OID stored in the new commit.
        tree: ObjectId,
    },
    /// The three-way merge had conflicts; repository state is unchanged.
    Conflicts {
        /// Conflicted paths in repository-relative form, sorted and deduplicated.
        paths: Vec<String>,
    },
    /// The commit introduces no tree change relative to its first parent (pick only).
    Empty,
    /// The merged tree matches the current `HEAD` tree; nothing to commit.
    AlreadyApplied,
}

/// Replay `req.commit` onto the current branch using pick or revert merge semantics.
///
/// Requires a clean worktree (same checks as `grit pick`), a branch `HEAD`, and a
/// resolvable first-parent chain for the source commit. The source must not be a
/// merge commit.
///
/// # Errors
///
/// - [`Error::DetachedHead`] when `HEAD` is not on a branch.
/// - [`Error::UnbornHead`] when the branch has no commits yet.
/// - [`Error::MergeCommit`] when the source commit has more than one parent (pick or revert).
/// - [`Error::ReplaySourceAtHead`] when the source commit is already `HEAD`.
/// - I/O, ODB, merge, checkout, or ref update failures.
pub fn replay_commit(repo: &Repository, req: &ReplayRequest) -> Result<ReplayOutcome> {
    ensure_worktree_clean_for_pick(repo)?;

    let (refname, head_oid) = match resolve_head(&repo.git_dir)? {
        HeadState::Branch {
            refname,
            oid: Some(oid),
            ..
        } => (refname, oid),
        HeadState::Branch { .. } => return Err(Error::UnbornHead),
        HeadState::Detached { .. } => return Err(Error::DetachedHead),
        HeadState::Invalid => {
            return Err(Error::Message("HEAD is in an unknown state".into()));
        }
    };

    if req.commit == head_oid {
        return Err(Error::ReplaySourceAtHead);
    }

    let source_obj = repo.odb.read(&req.commit)?;
    let source = parse_commit(&source_obj.data)?;
    if source.parents.len() > 1 {
        return Err(Error::MergeCommit { oid: req.commit });
    }

    let head_tree = commit_tree(repo, &head_oid)?;
    let (base_tree, ours_tree, theirs_tree) =
        merge_trees_for_direction(repo, req.direction, &source, head_tree)?;

    if req.direction == ReplayDirection::Pick && base_tree == source.tree {
        return Ok(ReplayOutcome::Empty);
    }

    let merged = merge_trees_three_way(
        repo,
        base_tree,
        ours_tree,
        theirs_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )?;

    if has_merge_conflicts(&merged) {
        return Ok(ReplayOutcome::Conflicts {
            paths: conflict_paths(&merged),
        });
    }

    let mut index = merged.index;
    let new_tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())?;
    if new_tree == head_tree {
        return Ok(ReplayOutcome::AlreadyApplied);
    }

    prepare_tree_checkout(repo, Some(&head_tree), &new_tree)?;
    checkout_between_trees(repo, Some(&head_tree), &new_tree)?;

    let message = commit_message(req, &source)?;
    let author = match req.direction {
        ReplayDirection::Pick => source.author.clone(),
        ReplayDirection::Revert => req.committer.clone(),
    };

    let commit_data = CommitData {
        tree: new_tree,
        parents: vec![head_oid],
        author,
        committer: req.committer.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message,
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let new_oid = write_commit_object(repo, &commit_data, None)?;

    let config = repo.config()?;
    let subject = commit_data.message.lines().next().unwrap_or("").trim_end();
    let reflog_msg = match req.direction {
        ReplayDirection::Pick => format!("cherry-pick: {subject}"),
        ReplayDirection::Revert => format!("revert: {subject}"),
    };

    update_branch_for_commit_with_config(
        &repo.git_dir,
        &BranchCommitRefUpdate {
            branch_ref: &refname,
            expected_old: Some(head_oid),
            new_oid,
            identity: &req.committer,
            reflog_message: &reflog_msg,
        },
        config.as_ref(),
    )?;

    Ok(ReplayOutcome::Committed {
        oid: new_oid,
        tree: new_tree,
    })
}

fn merge_trees_for_direction(
    repo: &Repository,
    direction: ReplayDirection,
    source: &CommitData,
    head_tree: ObjectId,
) -> Result<(ObjectId, ObjectId, ObjectId)> {
    let parent_tree = if let Some(parent) = source.parents.first() {
        commit_tree(repo, parent)?
    } else {
        empty_tree(repo)?
    };
    let source_tree = source.tree;
    Ok(match direction {
        ReplayDirection::Pick => (parent_tree, head_tree, source_tree),
        ReplayDirection::Revert => (source_tree, head_tree, parent_tree),
    })
}

fn commit_message(req: &ReplayRequest, source: &CommitData) -> Result<String> {
    if let Some(msg) = &req.message_override {
        let mut message = msg.trim().to_owned();
        if !message.is_empty() && !message.ends_with('\n') {
            message.push('\n');
        }
        return Ok(message);
    }
    match req.direction {
        ReplayDirection::Pick => Ok(source.message.clone()),
        ReplayDirection::Revert => {
            let (title, body) = merge_commit_message_for_revert(source, req.commit, false, '#');
            Ok(format!("{title}{body}"))
        }
    }
}

fn commit_tree(repo: &Repository, commit_oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "expected commit, got {}",
            obj.kind.as_str()
        )));
    }
    Ok(parse_commit(&obj.data)?.tree)
}

fn empty_tree(repo: &Repository) -> Result<ObjectId> {
    repo.odb.write(ObjectKind::Tree, &[])
}

fn has_merge_conflicts(merged: &crate::merge_trees::TreeMergeOutput) -> bool {
    !merged.conflict_content.is_empty() || merged.index.entries().iter().any(|e| e.stage() != 0)
}

fn conflict_paths(merged: &crate::merge_trees::TreeMergeOutput) -> Vec<String> {
    let mut paths: Vec<String> = merged
        .conflict_content
        .keys()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .collect();
    if paths.is_empty() {
        paths = merged
            .index
            .entries()
            .iter()
            .filter(|e| e.stage() != 0)
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
    }
    paths.sort();
    paths.dedup();
    paths
}
