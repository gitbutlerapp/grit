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
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::pack;
use crate::repo::Repository;

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
    let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
        return HashSet::new();
    };
    let mut ids = HashSet::new();
    for idx in indexes {
        if !idx.is_promisor {
            continue;
        }
        for e in &idx.entries {
            if let Ok(oid) = crate::objects::ObjectId::from_bytes(&e.oid) {
                ids.insert(oid);
            }
        }
    }
    ids
}

/// Every `tag.object` OID for tag objects stored in promisor packs (the peeled target).
///
/// Used for `rev-list --exclude-promisor-objects` when a missing commit was promised only via an
/// annotated tag object still in the promisor pack (t0410).
#[must_use]
pub fn promisor_pack_peeled_tag_targets(repo: &Repository) -> HashSet<ObjectId> {
    let seeds = promisor_pack_object_ids(&repo.git_dir.join("objects"));
    let mut out = HashSet::new();
    for oid in seeds {
        let Ok(obj) = repo.odb.read(&oid) else {
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
    let seeds = promisor_pack_object_ids(&repo.git_dir.join("objects"));
    let mut out = seeds.clone();
    for oid in seeds {
        let obj = match repo.odb.read(&oid) {
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
/// tree entry OIDs), and [`read_promisor_missing_oids`] entries.
///
/// # Errors
///
/// Returns [`crate::error::Error::CorruptObject`] if a packed object has invalid bytes.
pub fn promisor_expanded_object_ids(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let objects_dir = repo.git_dir.join("objects");
    let seeds = promisor_pack_object_ids(&objects_dir);
    let mut set: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = seeds.iter().copied().collect();

    while let Some(oid) = queue.pop_front() {
        if !set.insert(oid) {
            continue;
        }
        let obj = match repo.odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
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
                let entries = parse_tree(&obj.data)?;
                for e in entries {
                    if e.mode != 0o160000 {
                        queue.push_back(e.oid);
                    }
                }
            }
            ObjectKind::Blob => {}
        }
    }

    for oid in read_promisor_missing_oids(&repo.git_dir) {
        set.insert(oid);
    }
    Ok(set)
}
