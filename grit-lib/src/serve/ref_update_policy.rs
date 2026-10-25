//! Shared receive-side ref update checks for wire receive-pack and local push.

use crate::hide_refs;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::repo::Repository;

use super::receive_pack::{ReceivePolicy, RefCommand, RefUpdateRefusal};
use super::{head_info, Result};

/// Full ref name of the branch `HEAD` points at, bare repository or not.
#[must_use]
pub fn head_branch(repo: &Repository) -> Option<String> {
    head_info(repo).target
}

/// Full ref name of the branch checked out in a non-bare repository, if any.
#[must_use]
pub fn checked_out_branch(repo: &Repository) -> Option<String> {
    if repo.is_bare() {
        None
    } else {
        head_branch(repo)
    }
}

/// The branch `HEAD` points at, as the current-branch checks see it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeadBranches<'a> {
    /// The full ref `HEAD` points at, bare repository or not
    /// (`receive.denyDeleteCurrent`); see [`head_branch`].
    pub head: Option<&'a str>,
    /// The same, only in a repository with a working tree, where it is
    /// checked out (`receive.denyCurrentBranch`); see [`checked_out_branch`].
    pub checked_out: Option<&'a str>,
}

/// Decide whether one ref update may be applied under `receive.*` policy.
///
/// `object_lookup`, when set, is the object database used for existence and
/// fast-forward checks (the receive quarantine during wire push, which falls
/// back to the repository via alternates; or a local push dry-run lookup with
/// the pusher's store primary and the remote as alternates). When `None`, the
/// repository's own object database is used (local push after objects are copied).
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
    branches: HeadBranches<'_>,
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
    let is_current_branch = branches.checked_out == Some(cmd.refname.as_str());

    let Some(new) = cmd.new else {
        // As in Git, the checked-out branch's own check comes first, then the
        // deletion checks.
        if is_current_branch && policy.deny_current_branch {
            return Ok(Some(RefUpdateRefusal::CurrentBranch));
        }
        if policy.deny_deletes {
            return Ok(Some(RefUpdateRefusal::DeletionProhibited));
        }
        if branches.head == Some(cmd.refname.as_str()) && policy.deny_delete_current {
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
    let graph = repo.caches().commit_graph_chain(repo.odb().objects_dir());
    match crate::merge_base::is_ancestor_odb_walk_with_graph(odb, graph.as_deref(), old, new) {
        Ok(true) => None,
        Ok(false) => Some(RefUpdateRefusal::NonFastForward),
        Err(_) => Some(RefUpdateRefusal::FastForwardCheckFailed),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::config::environment::RepositoryOptions;
    use crate::objects::ObjectId;
    use crate::odb::Odb;
    use crate::repo::Repository;
    use grit_test_support::{git, git_cmd};

    use super::{validate_ref_update, ReceivePolicy, RefCommand, RefUpdateRefusal};

    fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
        ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
    }

    /// Remote tip exists only on the remote; pusher store is primary with remote alternates.
    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn validate_ff_with_remote_tip_in_alternate_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seed = tmp.path().join("seed");
        let bare = tmp.path().join("r.git");
        let local = tmp.path().join("w");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main", "."]);
        std::fs::write(seed.join("a"), "a\n").unwrap();
        git(&seed, &["add", "a"]);
        git(&seed, &["commit", "-q", "-m", "c1"]);
        std::fs::write(seed.join("b"), "b\n").unwrap();
        git(&seed, &["add", "b"]);
        git(&seed, &["commit", "-q", "-m", "c2"]);
        git_cmd(&["clone", "-q", "--bare", "seed", "r.git"])
            .in_dir(tmp.path())
            .suc();
        git_cmd(&["clone", "-q", "r.git", "w"])
            .in_dir(tmp.path())
            .suc();
        let local_git = local.join(".git");
        std::fs::write(local.join("c"), "c\n").unwrap();
        git(&local, &["add", "c"]);
        git(&local, &["commit", "-q", "-m", "c3"]);
        let c3 = rev_parse(&local, "HEAD");
        let c2 = rev_parse(&local, "HEAD~1");
        let c2_hex = c2.to_hex();
        let loose = local_git.join(format!(
            "objects/{}/{rest}",
            &c2_hex[..2],
            rest = &c2_hex[2..]
        ));
        std::fs::remove_file(&loose).expect("remove local copy of parent tip");

        let remote_repo =
            Repository::open(&bare, &RepositoryOptions::empty()).expect("open remote");
        let lookup = Odb::new(&local_git.join("objects"))
            .with_env_alternate_dirs(vec![bare.join("objects")]);
        let policy = ReceivePolicy {
            deny_non_fast_forwards: true,
            deny_deletes: false,
            deny_current_branch: false,
            deny_delete_current: false,
        };
        let cmd = RefCommand {
            refname: "refs/heads/main".to_owned(),
            old: Some(c2),
            new: Some(c3),
        };
        let refusal = validate_ref_update(
            &remote_repo,
            Some(&lookup),
            &cmd,
            &policy,
            &[],
            super::HeadBranches::default(),
        )
        .expect("validate");
        assert_eq!(refusal, None);

        let local_only = Odb::new(&local_git.join("objects"));
        let bad = validate_ref_update(
            &remote_repo,
            Some(&local_only),
            &cmd,
            &policy,
            &[],
            super::HeadBranches::default(),
        )
        .expect("validate local only");
        assert_eq!(bad, Some(RefUpdateRefusal::BadRef));
    }
}
