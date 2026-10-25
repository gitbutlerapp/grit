//! Shared receive-side ref update checks for wire receive-pack and local push.

use crate::hide_refs;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::repo::Repository;

use super::receive_pack::{ReceivePolicy, RefCommand, RefUpdateRefusal};
use super::{head_info, Result};

/// Full ref name of the branch checked out in a non-bare repository, if any.
#[must_use]
pub fn checked_out_branch(repo: &Repository) -> Option<String> {
    if repo.is_bare() {
        None
    } else {
        head_info(repo).target
    }
}

/// Decide whether one ref update may be applied under `receive.*` policy.
///
/// `object_lookup`, when set, is used to test that the new tip exists (the
/// receive quarantine during wire push, or the pusher's object store during
/// [`crate::remote::transfer::push_local`] before objects are copied). When
/// `None`, the repository's own object database is used.
///
/// # Errors
///
/// Propagates [`crate::odb::Odb::contains`] failures for the pushed object.
pub fn validate_ref_update(
    repo: &Repository,
    object_lookup: Option<&Odb>,
    cmd: &RefCommand,
    policy: &ReceivePolicy,
    hidden_refs: &[String],
    current_branch: Option<&str>,
) -> Result<Option<RefUpdateRefusal>> {
    if !cmd.refname.starts_with("refs/") || !crate::refs::is_valid_storable_ref_name(&cmd.refname) {
        return Ok(Some(RefUpdateRefusal::InvalidRefName));
    }
    if hide_refs::ref_is_hidden(&cmd.refname, &cmd.refname, hidden_refs) {
        return Ok(Some(RefUpdateRefusal::HiddenRef));
    }
    let current = crate::refs::resolve_ref(repo.git_dir(), &cmd.refname).ok();
    if current != cmd.old {
        return Ok(Some(RefUpdateRefusal::StaleInfo));
    }
    let is_current_branch = current_branch == Some(cmd.refname.as_str());

    let Some(new) = cmd.new else {
        if policy.deny_deletes {
            return Ok(Some(RefUpdateRefusal::DeletionProhibited));
        }
        if is_current_branch && policy.deny_current_branch {
            return Ok(Some(RefUpdateRefusal::CurrentBranchDeletion));
        }
        return Ok(None);
    };
    let odb = object_lookup.unwrap_or_else(|| repo.odb());
    if !odb.contains(&new)? {
        return Ok(Some(RefUpdateRefusal::MissingObjects));
    }
    if is_current_branch && policy.deny_current_branch {
        return Ok(Some(RefUpdateRefusal::CurrentBranch));
    }
    if let (Some(old), true) = (current, policy.deny_non_fast_forwards) {
        if cmd.refname.starts_with("refs/heads/") {
            return Ok(fast_forward_refusal(repo, odb, old, new));
        }
    }
    Ok(None)
}

/// Refusal reason when moving a branch from `old` to `new` would discard history.
fn fast_forward_refusal(
    repo: &Repository,
    odb: &Odb,
    old: ObjectId,
    new: ObjectId,
) -> Option<RefUpdateRefusal> {
    let is_commit = |oid: &ObjectId| {
        odb.read_info(oid)
            .is_ok_and(|info| info.kind == crate::objects::ObjectKind::Commit)
    };
    if !is_commit(&old) || !is_commit(&new) {
        return Some(RefUpdateRefusal::BadRef);
    }
    let graph = repo
        .caches()
        .commit_graph_chain(&repo.git_dir().join("objects"));
    match crate::merge_base::is_ancestor_odb_walk_with_graph(odb, graph.as_deref(), old, new) {
        Ok(true) => None,
        Ok(false) => Some(RefUpdateRefusal::NonFastForward),
        Err(_) => Some(RefUpdateRefusal::FastForwardCheckFailed),
    }
}
