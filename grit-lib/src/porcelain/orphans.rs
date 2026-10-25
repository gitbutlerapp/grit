//! Commits a `HEAD` move leaves unreachable from every ref, stash, and worktree `HEAD`.
//!
//! Shared by [`crate::porcelain::reset`] (moving a branch or detached `HEAD` backwards) and
//! [`crate::porcelain::switch`] (leaving a detached `HEAD` that carries commits of its own).

use crate::change_id::resolved_change_id;
use crate::error::Result;
use crate::git_path::path_for_disk_compare;
use crate::merge_base::{count_commits_left_behind, is_ancestor};
use crate::objects::{parse_commit, read_commit_subject_line, ObjectId};
use crate::refs;
use crate::repo::Repository;
use crate::state::HeadState;
use crate::worktree::{list_worktrees, WorktreeEntry};

/// Default cap on how many orphaned commits are listed by reset and switch.
pub const DEFAULT_ORPHAN_LIMIT: usize = 20;

/// One commit that becomes unreachable when `HEAD` moves away from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedCommitSummary {
    /// Commit object id.
    pub oid: ObjectId,
    /// Stored or derived change id for the commit.
    pub change_id: String,
    /// First line of the commit message.
    pub subject: String,
}

/// Newest-first sample of orphaned commits plus the full count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OrphanComputation {
    pub(crate) commits: Vec<OrphanedCommitSummary>,
    pub(crate) total: usize,
    pub(crate) truncated: bool,
}

/// Commits reachable from `previous` that nothing keeps once `head` moves to `target`.
///
/// The hide set is `target`, every ref under `refs/` except the branch `head` is moving,
/// `refs/stash`, and the `HEAD` of every worktree other than the one being moved. When
/// `previous` is `target` or one of its ancestors the walk is skipped.
///
/// `limit` caps [`OrphanComputation::commits`]; [`OrphanComputation::total`] still counts all.
///
/// # Errors
///
/// Ref listing, worktree listing, or commit reads fail.
pub(crate) fn compute_orphan_info(
    repo: &Repository,
    head: &HeadState,
    previous: ObjectId,
    target: ObjectId,
    limit: usize,
) -> Result<OrphanComputation> {
    if previous == target || is_ancestor(repo, previous, target)? {
        return Ok(OrphanComputation::default());
    }
    let hide = orphan_hide_tips(repo, head, target)?;
    let (total, oids) =
        count_commits_left_behind(repo, std::slice::from_ref(&previous), &hide, limit)?;
    let truncated = total > oids.len();
    let commits = oids
        .into_iter()
        .map(|oid| orphan_commit_summary(repo, oid))
        .collect::<Result<Vec<_>>>()?;
    Ok(OrphanComputation {
        commits,
        total,
        truncated,
    })
}

fn orphan_hide_tips(
    repo: &Repository,
    head: &HeadState,
    target: ObjectId,
) -> Result<Vec<ObjectId>> {
    let moving_branch = match head {
        HeadState::Branch { refname, .. } => Some(refname.as_str()),
        _ => None,
    };
    // Tips stay unpeeled: the walk peels each one once and skips those that name no commit.
    let mut hide = vec![target];
    hide.extend(
        refs::list_refs_for_repository(repo, "refs/")?
            .into_iter()
            .filter(|(refname, _)| moving_branch != Some(refname.as_str()))
            .map(|(_, oid)| oid),
    );
    if let Ok(oid) = repo.resolve_ref_oid("refs/stash") {
        hide.push(oid);
    }
    for wt in list_worktrees(repo)? {
        if worktree_is_moving_head(repo, head, &wt) {
            continue;
        }
        match wt.head {
            HeadState::Detached { oid } | HeadState::Branch { oid: Some(oid), .. } => {
                hide.push(oid);
            }
            HeadState::Branch { .. } | HeadState::Invalid => {}
        }
    }
    hide.sort_unstable();
    hide.dedup();
    Ok(hide)
}

/// True when this listed worktree is the one whose `HEAD` ref or file is being moved.
fn worktree_is_moving_head(repo: &Repository, head: &HeadState, wt: &WorktreeEntry) -> bool {
    let Some(repo_wt) = repo.work_tree() else {
        return false;
    };
    if path_for_disk_compare(&wt.path) != path_for_disk_compare(repo_wt) {
        return false;
    }
    match head {
        HeadState::Branch { refname, .. } => matches!(
            &wt.head,
            HeadState::Branch { refname: wt_ref, .. } if wt_ref == refname
        ),
        HeadState::Detached { oid } => matches!(
            &wt.head,
            HeadState::Detached { oid: wt_oid } if wt_oid == oid
        ),
        HeadState::Invalid => false,
    }
}

fn orphan_commit_summary(repo: &Repository, oid: ObjectId) -> Result<OrphanedCommitSummary> {
    let obj = repo.odb().read(&oid)?;
    let data = parse_commit(&obj.data)?;
    let subject = read_commit_subject_line(&obj.data).unwrap_or_default();
    let change_id = resolved_change_id(&oid, &data).id.as_str().to_owned();
    Ok(OrphanedCommitSummary {
        oid,
        change_id,
        subject,
    })
}
