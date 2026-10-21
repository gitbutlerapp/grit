//! Reorder one commit on the first-parent chain of `HEAD`.

use crate::error::Error;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::state::resolve_head;

use super::error::HistoryError;
use super::plan::plan_replay_linear_segment;
use super::types::{EmptyPolicy, RewriteOptions, RewritePlan};

fn library_err(err: Error) -> HistoryError {
    if let Error::History(inner) = err {
        return inner;
    }
    HistoryError::Library(Box::new(err))
}

fn commit_parents(repo: &Repository, oid: ObjectId) -> Result<Vec<ObjectId>, HistoryError> {
    let raw = repo.odb().read(&oid).map_err(library_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(library_err(Error::CorruptObject(format!(
            "expected commit, got {}",
            raw.kind.as_str()
        ))));
    }
    Ok(parse_commit(&raw.data).map_err(library_err)?.parents)
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
pub fn move_commit(
    repo: &Repository,
    commit: ObjectId,
    placement: Placement,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    let head = head_oid(repo)?;
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

    let mut opts = opts.clone();
    opts.empty = EmptyPolicy::Keep;
    plan_replay_linear_segment(repo, oldest, &replay_order, &opts)
}

fn head_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
    head.oid().copied().ok_or(HistoryError::NotInHistory {
        commit: ObjectId::null(repo.odb().hash_algo()),
    })
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
