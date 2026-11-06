//! Reorder one commit on the first-parent chain of `HEAD`.

use crate::objects::ObjectId;
use crate::repo::Repository;

use super::error::HistoryError;
use super::plan::plan_replay_linear_segment;
use super::types::{RewriteOptions, RewritePlan};

fn commit_parents(repo: &Repository, oid: ObjectId) -> Result<Vec<ObjectId>, HistoryError> {
    Ok(repo.odb().read_commit(&oid)?.parents)
}

/// Where to place a moved commit relative to an anchor on the first-parent chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Immediately before the anchor commit.
    Before(ObjectId),
    /// Immediately after the anchor commit.
    After(ObjectId),
}

/// Plan moving `commit` to `placement` on the linear first-parent history of `HEAD`.
///
/// Writes replacement commit objects into the ODB; refs are updated only when the caller applies
/// the returned [`RewritePlan`].
///
/// # Errors
///
/// Returns [`HistoryError::NotInHistory`], [`HistoryError::MoveTargetNotInHistory`],
/// [`HistoryError::NoOp`], [`HistoryError::NotLinear`], or errors from replay planning.
/// An unborn or invalid `HEAD` is [`HistoryError::Library`] wrapping
/// [`Error::UnbornHead`](crate::error::Error::UnbornHead) or
/// [`Error::InvalidHead`](crate::error::Error::InvalidHead).
pub fn move_commit(
    repo: &Repository,
    commit: ObjectId,
    placement: Placement,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    let head = repo.head_commit()?;
    let mut chain = first_parent_chain(repo, head)?;
    if chain.is_empty() {
        return Err(HistoryError::NotInHistory { commit });
    }

    let original = chain.clone();
    let Some(old_idx) = chain.iter().position(|&c| c == commit) else {
        return Err(HistoryError::NotInHistory { commit });
    };

    let anchor = match placement {
        Placement::Before(anchor) | Placement::After(anchor) => anchor,
    };
    if anchor == commit {
        return Err(HistoryError::NoOp { commit });
    }
    let Some(mut anchor_idx) = chain.iter().position(|&c| c == anchor) else {
        return Err(HistoryError::MoveTargetNotInHistory { commit: anchor });
    };

    chain.remove(old_idx);
    if anchor_idx > old_idx {
        anchor_idx -= 1;
    }
    let insert_at = match placement {
        Placement::Before(_) => anchor_idx,
        Placement::After(_) => anchor_idx + 1,
    };
    chain.insert(insert_at, commit);

    if chain == original {
        return Err(HistoryError::NoOp { commit });
    }

    let new_idx = chain
        .iter()
        .position(|&c| c == commit)
        .ok_or(HistoryError::Planning {
            message: "commit missing from reordered chain".into(),
        })?;
    let segment_start = old_idx.min(new_idx);
    let oldest = original[segment_start];
    let replay_order: Vec<ObjectId> = chain[segment_start..].to_vec();

    plan_replay_linear_segment(repo, oldest, &replay_order, opts)
}

fn first_parent_chain(repo: &Repository, tip: ObjectId) -> Result<Vec<ObjectId>, HistoryError> {
    let mut newest_first = Vec::new();
    let mut current = Some(tip);
    while let Some(oid) = current {
        newest_first.push(oid);
        let parents = commit_parents(repo, oid)?;
        current = parents.first().copied();
    }
    newest_first.reverse();
    Ok(newest_first)
}
