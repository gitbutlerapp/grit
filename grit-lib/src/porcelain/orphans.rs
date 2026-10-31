//! Commits a `HEAD` move leaves unreachable from every ref, stash, and worktree `HEAD`.
//!
//! Shared by [`crate::porcelain::reset`] (moving a branch or detached `HEAD` backwards) and
//! [`crate::porcelain::switch`] (leaving a detached `HEAD` that carries commits of its own).

use crate::change_id::resolved_change_id;
use crate::error::Result;
use crate::git_path::path_for_disk_compare;
use crate::merge_base::{
    count_commits_left_behind, count_commits_left_behind_after, is_ancestor_unless_rewound,
};
use crate::objects::{parse_commit, read_commit_subject_line, ObjectId};
use crate::refs;
use crate::repo::Repository;
use crate::state::HeadState;
use crate::worktree::{list_worktrees, WorktreeEntry};

/// Default cap on how many orphaned commits are listed by reset and switch.
pub const DEFAULT_ORPHAN_LIMIT: usize = 20;

/// One commit that becomes unreachable when `HEAD` moves away from it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
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
/// The walk stays near the commits being left behind instead of walking history down to the
/// oldest ref. With a commit-graph the result is exact. Without one it ends by committer
/// date like Git's slop-based `git rev-list <previous> --not <refs>`, so the count can be
/// wrong under heavy clock skew (a ref reaching a commit through history dated more than a
/// day before it). The report is advisory; nothing is written based on it.
///
/// With `now` (Unix seconds), the walk first leaves out the refs `packed-refs` records and
/// keeps that answer when every commit left behind is dated well after the file was written
/// ([`count_commits_left_behind_after`]): a fresh commit undone in a repository with
/// hundreds of packed tags then reads none of the tagged commits.
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
    now: Option<i64>,
) -> Result<OrphanComputation> {
    if previous == target || is_ancestor_unless_rewound(repo, previous, target)? {
        return Ok(OrphanComputation::default());
    }
    let tips = std::slice::from_ref(&previous);
    let hide = orphan_hide_tips(repo, head, target)?;
    let bounded = match (now, hide.packed_written_at) {
        (Some(now), Some(written_at)) if !hide.packed.is_empty() => {
            count_commits_left_behind_after(repo, tips, &hide.unpacked, limit, written_at, now)?
        }
        _ => None,
    };
    let (total, oids) = match bounded {
        Some(found) => found,
        None => count_commits_left_behind(repo, tips, &hide.all(), limit)?,
    };
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

/// The tips that keep commits reachable once `HEAD` moves, split by where their value is
/// recorded.
struct OrphanHideTips {
    /// Refs whose value `packed-refs` records.
    packed: Vec<ObjectId>,
    /// `target`, loose refs, `refs/stash` and other worktrees' `HEAD`s.
    unpacked: Vec<ObjectId>,
    /// When `packed-refs` was last written (Unix seconds, rounded up), read after the listing.
    packed_written_at: Option<i64>,
}

impl OrphanHideTips {
    fn all(&self) -> Vec<ObjectId> {
        let mut all = [self.packed.as_slice(), self.unpacked.as_slice()].concat();
        all.sort_unstable();
        all.dedup();
        all
    }
}

fn orphan_hide_tips(
    repo: &Repository,
    head: &HeadState,
    target: ObjectId,
) -> Result<OrphanHideTips> {
    let moving_branch = match head {
        HeadState::Branch { refname, .. } => Some(refname.as_str()),
        _ => None,
    };
    // Tips stay unpeeled: the walk peels each one once and skips those that name no commit.
    // A `packed-refs` peel line already names the tag's target, so that tag is never read.
    // A malformed `packed-refs` only loses the shortcut.
    let listed = refs::list_refs_for_repository(repo, "refs/")?;
    let peels = refs::PackedRefPeels::for_store(repo.refs(), repo.git_dir()).ok();
    let packed_written_at = repo
        .refs()
        .as_files()
        .and_then(|files| files.packed_refs_written_at())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|age| i64::try_from(age.as_secs() + u64::from(age.subsec_nanos() > 0)).ok());
    let mut packed = Vec::new();
    let mut hide = vec![target];
    for (refname, oid) in listed {
        if moving_branch == Some(refname.as_str()) {
            continue;
        }
        match peels.as_ref().and_then(|p| p.record(&refname, &oid)) {
            Some(record) => packed.push(record.peeled.flatten().unwrap_or(oid)),
            None => hide.push(oid),
        }
    }
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
    Ok(OrphanHideTips {
        packed,
        unpacked: hide,
        packed_written_at,
    })
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
