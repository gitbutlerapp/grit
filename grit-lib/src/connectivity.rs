//! Reachability checks for bundle connectivity verification.

use std::collections::{HashSet, VecDeque};

use crate::error::Result;
use crate::objects::{oid_hash_set, parse_commit, ObjectId, ObjectKind, TreeRefIter};
use crate::refs;
use crate::repo::Repository;
use crate::shallow;

const TREE_MODE: u32 = 0o040000;
const TYPE_MASK: u32 = 0o170000;

/// Returns every commit reachable from all refs under `refs/` (not `HEAD` alone).
///
/// Git's connectivity check uses `--all` (refs), not `HEAD` resolution: a symbolic `HEAD`
/// that points at `refs/heads/main` must not be treated as an extra root, or pushes that
/// only advance `main` would be misclassified as already connected.
fn commit_closure_from_all_refs(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut seeds = Vec::new();

    if let Ok(entries) = refs::list_refs(repo.git_dir(), "refs/") {
        for (_, oid) in entries {
            seeds.push(oid);
        }
    }

    let mut seen = oid_hash_set();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    for oid in seeds {
        if repo.odb().read_info(&oid).is_err() {
            continue;
        }
        if let Some(commit) = repo.odb().peel_to_commit(oid)? {
            if seen.insert(commit) {
                queue.push_back(commit);
            }
        }
    }

    while let Some(c) = queue.pop_front() {
        let obj = repo.odb().read(&c)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        for p in commit.parents {
            if seen.insert(p) {
                queue.push_back(p);
            }
        }
    }

    Ok(seen.into_iter().collect())
}

/// Returns whether bundle prerequisites are satisfied in `repo`.
///
/// When every prerequisite commit is already in the commit closure of refs under `refs/`, this
/// returns immediately (the common case). Otherwise, each prerequisite that exists locally but is
/// not ref-reachable is checked like Git's `verify_bundle` + `check_connected`: the exclusive
/// history (`rev-list --objects` of the prerequisites `--not --all`) must be readable in the ODB,
/// including each commit's tree and reachable trees and blobs. A lone dangling commit missing its
/// tree or parents stays disconnected; a complete object graph in the ODB is enough even when no
/// ref points at it (for example after `unbundle` of a full bundle into an empty repository).
pub fn bundle_prerequisites_connected_to_refs(
    repo: &Repository,
    prerequisites: &[ObjectId],
) -> Result<bool> {
    if prerequisites.is_empty() {
        return Ok(true);
    }
    let ref_commits = commit_closure_from_all_refs(repo)?;
    if prerequisites.iter().all(|p| ref_commits.contains(p)) {
        return Ok(true);
    }
    let shallow = shallow::load_shallow_boundaries(repo.git_dir());
    let oid_len = repo.odb().hash_algo().len();
    let mut queued: VecDeque<ObjectId> = VecDeque::new();
    let mut commits: HashSet<ObjectId> = HashSet::new();
    for &prereq in prerequisites {
        if ref_commits.contains(&prereq) {
            continue;
        }
        if commits.insert(prereq) {
            queued.push_back(prereq);
        }
    }
    while let Some(commit) = queued.pop_front() {
        if ref_commits.contains(&commit) {
            continue;
        }
        let obj = match repo.odb().read(&commit) {
            Ok(obj) => obj,
            Err(_) => return Ok(false),
        };
        if obj.kind != ObjectKind::Commit {
            return Ok(false);
        }
        let parsed = parse_commit(&obj.data)?;
        if !walk_tree_objects(repo, parsed.tree, oid_len)? {
            return Ok(false);
        }
        if shallow.contains(&commit) {
            continue;
        }
        for parent in parsed.parents {
            if ref_commits.contains(&parent) {
                continue;
            }
            if commits.insert(parent) {
                queued.push_back(parent);
            }
        }
    }
    Ok(true)
}

/// Every tree and non-gitlink blob under `root` must exist in the ODB.
fn walk_tree_objects(repo: &Repository, root: ObjectId, oid_len: usize) -> Result<bool> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(tree) = stack.pop() {
        if !seen.insert(tree) {
            continue;
        }
        let obj = match repo.odb().read(&tree) {
            Ok(obj) => obj,
            Err(_) => return Ok(false),
        };
        if obj.kind != ObjectKind::Tree {
            return Ok(false);
        }
        for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
            let entry = entry?;
            if entry.mode & TYPE_MASK == TREE_MODE {
                stack.push(entry.oid);
                continue;
            }
            if entry.mode == 0o160000 {
                continue;
            }
            if repo.odb().read(&entry.oid).is_err() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{InitOptions, Repository};

    fn blob(repo: &Repository, bytes: &[u8]) -> ObjectId {
        repo.odb().write(ObjectKind::Blob, bytes).expect("blob")
    }

    fn tree(repo: &Repository, entries: &[(&str, &str, ObjectId)]) -> ObjectId {
        let mut body = Vec::new();
        for (mode, name, oid) in entries {
            body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            body.extend_from_slice(oid.as_bytes());
        }
        repo.odb().write(ObjectKind::Tree, &body).expect("tree")
    }

    fn commit(repo: &Repository, tree: ObjectId, parents: &[ObjectId]) -> ObjectId {
        let mut body = format!("tree {tree}\n");
        for parent in parents {
            body.push_str(&format!("parent {parent}\n"));
        }
        body.push_str("author T <t@e> 1 +0000\ncommitter T <t@e> 1 +0000\n\nm\n");
        repo.odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("commit")
    }

    #[test]
    fn dangling_complete_history_is_connected_without_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) =
            Repository::init(dir.path(), &InitOptions::new().with_bare(true)).expect("init");
        let blob = blob(&repo, b"data\n");
        let root = tree(&repo, &[("100644", "f", blob)]);
        let base = commit(&repo, root, &[]);
        let tip = commit(&repo, root, &[base]);
        assert!(bundle_prerequisites_connected_to_refs(&repo, &[tip]).expect("check"));
        assert!(
            bundle_prerequisites_connected_to_refs(&repo, &[base]).expect("check base"),
            "prerequisite base with full graph"
        );
    }

    #[test]
    fn shallow_boundary_still_requires_commit_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) =
            Repository::init(dir.path(), &InitOptions::new().with_bare(true)).expect("init");
        let missing_tree = ObjectId::from_hex(&"b".repeat(40)).expect("oid");
        let mut body = format!("tree {missing_tree}\n");
        body.push_str("author T <t@e> 1 +0000\ncommitter T <t@e> 1 +0000\n\nm\n");
        let lone = repo
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
        std::fs::write(
            shallow::shallow_file_path(repo.git_dir()),
            format!("{}\n", lone.to_hex()),
        )
        .expect("shallow");
        assert!(
            !bundle_prerequisites_connected_to_refs(&repo, &[lone]).expect("check"),
            "shallow listing must not skip commit/tree validation"
        );
    }

    #[test]
    fn shallow_parent_still_requires_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) =
            Repository::init(dir.path(), &InitOptions::new().with_bare(true)).expect("init");
        let missing_tree = ObjectId::from_hex(&"c".repeat(40)).expect("oid");
        let mut parent_body = format!("tree {missing_tree}\n");
        parent_body.push_str("author T <t@e> 1 +0000\ncommitter T <t@e> 1 +0000\n\np\n");
        let parent = repo
            .odb()
            .write(ObjectKind::Commit, parent_body.as_bytes())
            .expect("parent");
        let blob = blob(&repo, b"d\n");
        let root = tree(&repo, &[("100644", "f", blob)]);
        let tip = commit(&repo, root, &[parent]);
        std::fs::write(
            shallow::shallow_file_path(repo.git_dir()),
            format!("{}\n", parent.to_hex()),
        )
        .expect("shallow");
        assert!(
            !bundle_prerequisites_connected_to_refs(&repo, &[tip]).expect("check"),
            "shallow parent reached via walk must still have a readable tree"
        );
    }

    #[test]
    fn dangling_commit_without_tree_is_not_connected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) =
            Repository::init(dir.path(), &InitOptions::new().with_bare(true)).expect("init");
        let missing_tree = ObjectId::from_hex(&"a".repeat(40)).expect("oid");
        let mut body = format!("tree {missing_tree}\n");
        body.push_str("author T <t@e> 1 +0000\ncommitter T <t@e> 1 +0000\n\nm\n");
        let lone = repo
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
        assert!(
            !bundle_prerequisites_connected_to_refs(&repo, &[lone]).expect("check"),
            "commit without tree should not satisfy prerequisites"
        );
    }

    #[test]
    fn ref_reachable_prerequisite_stays_fast_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let blob = blob(&repo, b"x\n");
        let root = tree(&repo, &[("100644", "x", blob)]);
        let tip = commit(&repo, root, &[]);
        crate::refs::update_direct_ref_on_store(
            repo.ref_store(),
            repo.git_dir(),
            crate::refs::store::RefNameRef::new("refs/heads/main").expect("valid ref name"),
            &tip,
        )
        .expect("ref");
        assert!(bundle_prerequisites_connected_to_refs(&repo, &[tip]).expect("check"));
    }
}
