//! Partial-clone promisor bookkeeping used by Grit.
//!
//! Git records missing objects via the promisor protocol; Grit uses a marker file
//! (`grit-promisor-missing`) so commands like `rev-list --missing=print` and
//! `backfill` can track which blob OIDs are not present locally.
//!
//! Pack files with a sibling `.promisor` marker (e.g. `pack-abc.promisor` next to
//! `pack-abc.pack`) match Git's promisor packs: objects in those packs are treated as
//! promised by the remote for `fsck` and `rev-list --exclude-promisor-objects`.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::{parse_commit, parse_tag, ObjectId, ObjectKind, TreeRefIter};
use crate::odb::Odb;
use crate::repo::Repository;
use std::ops::ControlFlow;

/// Basename of the marker file under the git directory.
pub const PROMISOR_MISSING_FILE: &str = "grit-promisor-missing";

/// Read OIDs listed in the promisor-missing marker (40-char hex lines).
///
/// Order matches the file; duplicate lines are skipped after the first.
#[must_use]
pub fn read_promisor_missing_oids(git_dir: &Path) -> Vec<ObjectId> {
    let path = git_dir.join(PROMISOR_MISSING_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.len() != 40 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if let Ok(oid) = ObjectId::from_hex(t) {
            if seen.insert(oid) {
                out.push(oid);
            }
        }
    }
    out
}

/// Rewrite the promisor-missing marker from a set of OIDs (sorted, one per line).
pub fn write_promisor_marker(git_dir: &Path, oids: &HashSet<ObjectId>) -> Result<()> {
    let path = git_dir.join(PROMISOR_MISSING_FILE);
    let mut v: Vec<String> = oids.iter().map(|o| o.to_hex()).collect();
    v.sort();
    if v.is_empty() {
        fs::write(&path, b"")?;
    } else {
        fs::write(&path, format!("{}\n", v.join("\n")))?;
    }
    Ok(())
}

/// Returns `true` when the repository should honor promisor pack semantics (Git's
/// `repo_has_promisor_remote`): `extensions.partialclone` is set or some
/// `remote.*.promisor=true`.
#[must_use]
pub fn repo_treats_promisor_packs(_git_dir: &Path, config: &ConfigSet) -> bool {
    if config.get("extensions.partialclone").is_some() {
        return true;
    }
    config
        .entries()
        .iter()
        .any(|e| e.key.ends_with(".promisor") && e.value.as_deref() == Some("true"))
}

/// All object IDs stored in packfiles that have a sibling `.promisor` marker file.
#[must_use]
pub fn promisor_pack_object_ids(objects_dir: &Path) -> HashSet<ObjectId> {
    let odb = Odb::new(objects_dir);
    let Ok(primary) = odb.primary() else {
        return HashSet::new();
    };
    let mut ids = HashSet::new();
    let _ = primary
        .packed_objects()
        .for_each_promisor_object(&mut |oid| {
            ids.insert(*oid);
            ControlFlow::Continue(())
        });
    ids
}

/// Every `tag.object` OID for tag objects stored in promisor packs (the peeled target).
///
/// Used for `rev-list --exclude-promisor-objects` when a missing commit was promised only via an
/// annotated tag object still in the promisor pack (t0410).
#[must_use]
pub fn promisor_pack_peeled_tag_targets(repo: &Repository) -> HashSet<ObjectId> {
    let seeds = promisor_pack_object_ids(repo.odb().objects_dir());
    let mut out = HashSet::new();
    for oid in seeds {
        let Ok(obj) = repo.odb().read(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Tag {
            continue;
        }
        let Ok(t) = parse_tag(&obj.data) else {
            continue;
        };
        out.insert(t.object);
    }
    out
}

/// Promisor pack member OIDs plus peeled tag targets (one hop from each tag in the pack).
pub fn promisor_pack_and_tag_targets(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let seeds = promisor_pack_object_ids(repo.odb().objects_dir());
    let mut out = seeds.clone();
    for oid in seeds {
        let obj = match repo.odb().read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if obj.kind != ObjectKind::Tag {
            continue;
        }
        let t = parse_tag(&obj.data)?;
        out.insert(t.object);
    }
    Ok(out)
}

/// Objects considered "promisor" for traversal, matching Git's `is_promisor_object` set:
/// every object in promisor packs plus referenced OIDs (commit tree/parents, tag targets,
/// tree entry OIDs), and [`read_promisor_missing_oids`] entries. Missing targets
/// remain promises; this classification never fetches from the remote.
///
/// # Errors
///
/// Returns parse errors for invalid available objects and I/O errors for unreadable
/// pack indexes or object storage.
pub fn promisor_expanded_object_ids(repo: &Repository) -> Result<HashSet<ObjectId>> {
    maintenance_promise_boundary(repo.odb())
}

/// The objects a promisor remote promises, as Git's `is_promisor_object` defines
/// them: every object stored in a promisor pack, plus the objects those refer to
/// directly (a commit's tree and parents, a tag's target, a tree's entries), plus
/// [`read_promisor_missing_oids`].
///
/// Unlike [`maintenance_promise_boundary`] there is no further walk: an object
/// named only by an object outside the promisor packs is not promised, even when
/// that object is itself reachable from promisor history. This is the rule for
/// deciding whether a missing object is acceptable (serving, receiving, fsck).
/// Only objects already in promisor packs are read; nothing is fetched.
///
/// # Errors
///
/// Pack index and object read failures, and parse errors in promisor-pack objects.
pub(crate) fn promisor_referenced_objects(odb: &Odb) -> Result<HashSet<ObjectId>> {
    let mut members = Vec::new();
    if odb.uses_files_primary() {
        odb.primary()?
            .packed_objects()
            .for_each_promisor_object(&mut |oid| {
                members.push(*oid);
                ControlFlow::Continue(())
            })?;
    }
    let mut set: HashSet<ObjectId> = HashSet::with_capacity(members.len());
    for oid in members {
        set.insert(oid);
        match odb.read_info_without_fetch(&oid) {
            Ok(info) if info.kind == ObjectKind::Blob => continue,
            Err(crate::error::Error::ObjectNotFound(_)) => continue,
            Err(err) => return Err(err),
            Ok(_) => {}
        }
        let Some(obj) = odb.try_read_once(&oid)? else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                let c = parse_commit(&obj.data)?;
                set.insert(c.tree);
                set.extend(c.parents);
            }
            ObjectKind::Tag => {
                set.insert(parse_tag(&obj.data)?.object);
            }
            ObjectKind::Tree => {
                for entry in TreeRefIter::with_oid_len(&obj.data, odb.hash_algo().len()) {
                    set.insert(entry?.oid);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    if let Some(git_dir) = odb.config_git_dir() {
        set.extend(read_promisor_missing_oids(git_dir));
    }
    Ok(set)
}

/// Classify promises using only available objects. Never hydrate missing targets while
/// deciding which objects maintenance may omit from local packs and reachability walks.
///
/// A custom primary store has no promisor packs, so only the recorded missing ids apply.
pub(crate) fn maintenance_promise_boundary(odb: &Odb) -> Result<HashSet<ObjectId>> {
    let mut queue = VecDeque::new();
    if odb.uses_files_primary() {
        odb.primary()?
            .packed_objects()
            .for_each_promisor_object(&mut |oid| {
                queue.push_back(*oid);
                ControlFlow::Continue(())
            })?;
    }
    let mut set: HashSet<ObjectId> = HashSet::new();

    while let Some(oid) = queue.pop_front() {
        if !set.insert(oid) {
            continue;
        }
        match odb.read_info_without_fetch(&oid) {
            Ok(info) if info.kind == ObjectKind::Blob => continue,
            Err(crate::error::Error::ObjectNotFound(_)) => continue,
            Err(err) => return Err(err),
            Ok(_) => {}
        }
        let Some(obj) = odb.try_read_once(&oid)? else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                let c = parse_commit(&obj.data)?;
                queue.push_back(c.tree);
                for p in c.parents {
                    queue.push_back(p);
                }
            }
            ObjectKind::Tag => {
                let t = parse_tag(&obj.data)?;
                queue.push_back(t.object);
            }
            ObjectKind::Tree => {
                for entry in TreeRefIter::with_oid_len(&obj.data, odb.hash_algo().len()) {
                    let entry = entry?;
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if entry.mode & 0o170000 == 0o040000 {
                        queue.push_back(entry.oid);
                    } else {
                        // Files are leaves: record the promise without probing absent blobs.
                        set.insert(entry.oid);
                    }
                }
            }
            ObjectKind::Blob => {}
        }
    }

    if let Some(git_dir) = odb.config_git_dir() {
        set.extend(read_promisor_missing_oids(git_dir));
    }
    Ok(set)
}

/// Blobs some checkout of the repository needs on disk: every regular file and
/// symlink in each worktree's index, and in the tree of each worktree's `HEAD`.
///
/// Maintenance that drops promised blobs from the local object store (offloading
/// to a large-object promisor, reclaiming space in a partial clone) must keep these,
/// so a checkout never needs the network to show or restore its own files. Bare
/// worktrees contribute nothing. Objects that cannot be read (already missing, or
/// beyond a shallow boundary) are skipped; nothing is fetched.
///
/// # Errors
///
/// Worktree listing, index parse and object read failures.
pub(crate) fn blobs_needed_by_checkouts(repo: &Repository) -> Result<crate::objects::OidHashSet> {
    use crate::index::MODE_GITLINK;
    use crate::state::HeadState;

    let is_file_mode = |mode: u32| matches!(mode & 0o170000, 0o100000 | 0o120000);
    let odb = repo.odb();
    let mut blobs = crate::objects::oid_hash_set();
    let mut trees_seen = crate::objects::oid_hash_set();
    for wt in crate::worktree::list_worktrees(repo)? {
        if wt.is_bare {
            continue;
        }
        let index = if wt.admin_dir == repo.git_dir() {
            repo.load_index()?
        } else {
            crate::index::Index::load(&wt.admin_dir.join("index"))?
        };
        blobs.extend(
            index
                .entries()
                .iter()
                .filter(|e| e.mode != MODE_GITLINK && is_file_mode(e.mode) && !e.oid.is_zero())
                .map(|e| e.oid),
        );
        let head = match wt.head {
            HeadState::Detached { oid } | HeadState::Branch { oid: Some(oid), .. } => oid,
            HeadState::Branch { .. } | HeadState::Invalid => continue,
        };
        let Some(commit) = odb.try_read_once(&head)? else {
            continue;
        };
        if commit.kind != ObjectKind::Commit {
            continue;
        }
        let mut trees = vec![parse_commit(&commit.data)?.tree];
        while let Some(tree) = trees.pop() {
            if !trees_seen.insert(tree) {
                continue;
            }
            let Some(obj) = odb.try_read_once(&tree)? else {
                continue;
            };
            for entry in TreeRefIter::with_oid_len(&obj.data, odb.hash_algo().len()) {
                let entry = entry?;
                if entry.mode & 0o170000 == 0o040000 {
                    trees.push(entry.oid);
                } else if is_file_mode(entry.mode) {
                    blobs.insert(entry.oid);
                }
            }
        }
    }
    Ok(blobs)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod checkout_blob_tests {
    use super::*;
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::repo::InitOptions;

    fn commit_all(repo: &Repository, message: &str) {
        stage_for_commit(repo, &mut NullProgress, &NeverCancel).unwrap();
        let ident = "T <t@example.com> 1 +0000".to_string();
        create_commit(
            repo,
            &CommitRequest {
                message: message.into(),
                author: Some(ident.parse().unwrap()),
                committer: Some(ident.parse().unwrap()),
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
    }

    #[test]
    fn checkout_blobs_cover_index_and_head_but_not_history() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let work = repo.work_tree().unwrap().to_path_buf();
        let blob = |data: &[u8]| repo.odb().hash(ObjectKind::Blob, data).unwrap();
        std::fs::write(work.join("a"), b"old\n").unwrap();
        commit_all(&repo, "one");
        std::fs::write(work.join("a"), b"new\n").unwrap();
        commit_all(&repo, "two");
        std::fs::write(work.join("a"), b"staged\n").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).unwrap();

        let needed = blobs_needed_by_checkouts(&repo).unwrap();
        assert!(needed.contains(&blob(b"staged\n")), "index");
        assert!(needed.contains(&blob(b"new\n")), "HEAD tree");
        assert!(!needed.contains(&blob(b"old\n")), "history only");
    }

    #[test]
    fn bare_repositories_need_no_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        assert!(blobs_needed_by_checkouts(&repo).unwrap().is_empty());
    }
}
