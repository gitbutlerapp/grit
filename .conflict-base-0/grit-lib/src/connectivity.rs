//! Reachability checks for bundle connectivity verification.

use std::collections::{HashSet, VecDeque};

use crate::error::Result;
use crate::objects::{parse_commit, parse_tag, ObjectId, ObjectKind};
use crate::refs;
use crate::repo::Repository;

/// Returns every commit reachable from all refs under `refs/` (not `HEAD` alone).
///
/// Git's connectivity check uses `--all` (refs), not `HEAD` resolution: a symbolic `HEAD`
/// that points at `refs/heads/main` must not be treated as an extra root, or pushes that
/// only advance `main` would be misclassified as already connected.
fn commit_closure_from_all_refs(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut seeds = Vec::new();

    for prefix in &[
        "refs/heads/",
        "refs/tags/",
        "refs/remotes/",
        "refs/bundles/",
    ] {
        if let Ok(entries) = refs::list_refs(repo.git_dir(), prefix) {
            for (_, oid) in entries {
                seeds.push(oid);
            }
        }
    }

    let mut seen = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    for oid in seeds {
        let obj = match repo.odb().read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        match obj.kind {
            ObjectKind::Commit => {
                if seen.insert(oid) {
                    queue.push_back(oid);
                }
            }
            ObjectKind::Tag => {
                if let Some(target) = peel_tag_to_object(repo, &obj.data)? {
                    if let Ok(tobj) = repo.odb().read(&target) {
                        if tobj.kind == ObjectKind::Commit && seen.insert(target) {
                            queue.push_back(target);
                        }
                    }
                }
            }
            _ => {}
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

    Ok(seen)
}

fn peel_tag_to_object(repo: &Repository, tag_data: &[u8]) -> Result<Option<ObjectId>> {
    let tag = parse_tag(tag_data)?;
    let mut oid = tag.object;
    loop {
        let obj = repo.odb().read(&oid)?;
        match obj.kind {
            ObjectKind::Tag => {
                let inner = parse_tag(&obj.data)?;
                oid = inner.object;
            }
            _ => return Ok(Some(oid)),
        }
    }
}

/// Returns whether every bundle prerequisite commit is already reachable from existing refs.
///
/// Matches Git's `verify_bundle` connectivity check: prerequisite OIDs may exist loose in the ODB
/// but must still lie in the commit closure of `refs/heads/`, `refs/tags/`, `refs/remotes/`, and
/// `refs/bundles/` (so dangling objects do not satisfy prerequisites).
pub fn bundle_prerequisites_connected_to_refs(
    repo: &Repository,
    prerequisites: &[ObjectId],
) -> Result<bool> {
    if prerequisites.is_empty() {
        return Ok(true);
    }
    let closure = commit_closure_from_all_refs(repo)?;
    Ok(prerequisites.iter().all(|p| closure.contains(p)))
}
