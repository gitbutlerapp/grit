//! Object selection for pack-objects (walk or pack bitmap reachability).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::bitmap_walk::{BitmapWalkError, ReachabilityQuery, ReachableSet};
use crate::environment::RepositoryOptions;
use crate::error::Result;
use crate::objects::{
    oid_hash_set, parse_commit, parse_tag, parse_tree, Object, ObjectId, ObjectKind, OidHashSet,
};
use crate::odb::Odb;
use crate::pack_bitmap::BitmapIndex;
use crate::pack_name_hash::pack_name_hash;
use crate::repo::Repository;
use crate::rev_list::{MissingAction, ObjectFilter};

/// Objects to emit in a negotiated pack plus peer-held closure for thin deltas.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct PackObjectSelection {
    /// Objects reachable from `wants` but not from `haves`, in valid pack order.
    pub send: Vec<ObjectId>,
    /// Full object closure reachable from `haves` (peer-held bases for thin packs).
    pub have_closure: HashSet<ObjectId>,
    /// Git pack name-hash per blob (from bitmap cache or tree walk).
    pub blob_name_hashes: HashMap<ObjectId, u32>,
    /// Objects already loaded during a tree walk (avoids rereading loose objects for pack build).
    pub walk_prefetch: HashMap<ObjectId, Object>,
}

/// Parameters for [`enumerate_pack_objects`].
#[derive(Clone, Copy, Debug)]
pub struct PackEnumerateOptions<'a> {
    /// Partial-clone filter applied to the want side (when supported).
    pub filter: Option<&'a ObjectFilter>,
    /// Shallow graft boundaries on the want side: do not walk past these commits
    /// when enumerating objects to send. When non-empty, bitmap enumeration is
    /// skipped and the object walk is used.
    pub shallow_grafts: &'a HashSet<ObjectId>,
    /// Shallow graft boundaries when walking client `haves` to build the peer
    /// closure. Defaults to [`Self::shallow_grafts`] when unset.
    pub have_shallow_grafts: Option<&'a HashSet<ObjectId>>,
    /// When false, always use the object walk even if bitmaps exist.
    pub use_bitmaps: bool,
    /// Precomputed peer object closure to subtract from `wants` during a walk
    /// (for example when haves live in a different ODB than `wants`).
    pub exclude_objects: Option<&'a HashSet<ObjectId>>,
    /// When false, the object walk records oids only: blob bodies are not read and
    /// [`PackObjectSelection::walk_prefetch`] stays empty (reuse-only repacks).
    pub prefetch_walk_objects: bool,
}

/// Select pack objects using bitmap reachability when possible, otherwise the object walk.
///
/// # Errors
///
/// Propagates repository read failures. Missing `want` objects error; missing `have` objects are skipped.
pub fn enumerate_pack_objects(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
) -> Result<PackObjectSelection> {
    if opts.use_bitmaps {
        if let Some(sel) = try_bitmap_enumeration(odb, wants, haves, opts)? {
            return Ok(sel);
        }
    }
    walk_enumeration(odb, wants, haves, opts)
}

/// Bitmap reachability for upload-pack when shallow grafts and filters allow it.
///
/// Returns the send-side [`ReachableSet`] and the peer `have` closure as object ids.
pub fn try_bitmap_reachable_for_upload(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: Option<&ObjectFilter>,
    shallow_grafts: &HashSet<ObjectId>,
) -> Result<Option<(ReachableSet, HashSet<ObjectId>)>> {
    if !shallow_grafts.is_empty() {
        return Ok(None);
    }
    let Some(git_dir) = odb.config_git_dir() else {
        return Ok(None);
    };
    let repo = match Repository::open(git_dir, &RepositoryOptions::empty()) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let Some(index) = BitmapIndex::open(&repo).ok().flatten() else {
        return Ok(None);
    };
    let index = Arc::new(index);
    let missing_haves = MissingAction::Allow;
    let missing_wants = MissingAction::Error;

    let have_set = match index.reachability(
        &repo,
        ReachabilityQuery {
            wants: haves,
            haves: &[],
            filter: None,
        },
        missing_haves,
    ) {
        Ok(set) => set,
        Err(BitmapWalkError::Unsupported(_)) => return Ok(None),
        Err(BitmapWalkError::Bitmap(_)) => return Ok(None),
        Err(BitmapWalkError::Repository(e)) => return Err(e),
    };

    let send_set = match index.reachability(
        &repo,
        ReachabilityQuery {
            wants,
            haves,
            filter,
        },
        missing_wants,
    ) {
        Ok(set) => set,
        Err(BitmapWalkError::Unsupported(_)) => return Ok(None),
        Err(BitmapWalkError::Bitmap(_)) => return Ok(None),
        Err(BitmapWalkError::Repository(e)) => return Err(e),
    };

    Ok(Some((send_set, have_set.object_ids())))
}

fn try_bitmap_enumeration(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
) -> Result<Option<PackObjectSelection>> {
    let Some((send_set, have_closure)) =
        try_bitmap_reachable_for_upload(odb, wants, haves, opts.filter, opts.shallow_grafts)?
    else {
        return Ok(None);
    };
    let index = send_set.bitmap_index();
    let send: Vec<ObjectId> = send_set.iter_grouped_by_kind().collect();
    let blob_name_hashes = blob_name_hashes_from_bitmap(index, &send, &have_closure);
    Ok(Some(PackObjectSelection {
        send,
        have_closure,
        blob_name_hashes,
        walk_prefetch: HashMap::new(),
    }))
}

pub(crate) fn blob_name_hashes_for_send(
    index: &BitmapIndex,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
) -> HashMap<ObjectId, u32> {
    blob_name_hashes_from_bitmap(index, send, have_closure)
}

fn blob_name_hashes_from_bitmap(
    index: &BitmapIndex,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
) -> HashMap<ObjectId, u32> {
    let mut out = HashMap::new();
    for oid in send.iter().chain(have_closure.iter()) {
        let Some(pos) = index.position_of(oid) else {
            continue;
        };
        if let Some(h) = index.name_hash(pos) {
            out.insert(*oid, h);
        }
    }
    out
}

fn walk_enumeration(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
) -> Result<PackObjectSelection> {
    let have_shallow = opts.have_shallow_grafts.unwrap_or(opts.shallow_grafts);
    let have_closure = if let Some(ex) = opts.exclude_objects {
        ex.clone()
    } else {
        reachable_closure_walk(odb, haves, &HashSet::new(), true, have_shallow)?
    };
    let (send, blob_name_hashes, walk_prefetch) = if opts.filter.is_some() {
        let (send, blob_name_hashes) = rev_list_filtered_objects(odb, wants, haves, opts.filter)?;
        (send, blob_name_hashes, HashMap::new())
    } else {
        collect_reachable_excluding_with_name_hashes(
            odb,
            wants,
            &have_closure,
            false,
            opts.shallow_grafts,
            opts.prefetch_walk_objects,
        )?
    };
    Ok(PackObjectSelection {
        send,
        have_closure,
        blob_name_hashes,
        walk_prefetch,
    })
}

/// Compute the full object closure reachable from `roots`, stopping descent into `stop`.
pub(crate) fn reachable_closure_walk(
    odb: &Odb,
    roots: &[ObjectId],
    stop: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let order = collect_reachable_excluding_with_name_hashes(
        odb,
        roots,
        stop,
        skip_missing,
        shallow_grafts,
        true,
    )?
    .0;
    for oid in order {
        seen.insert(oid);
    }
    Ok(seen)
}

fn rev_list_filtered_objects(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: Option<&ObjectFilter>,
) -> Result<(Vec<ObjectId>, HashMap<ObjectId, u32>)> {
    let Some(git_dir) = odb.config_git_dir() else {
        return Ok((Vec::new(), HashMap::new()));
    };
    let repo = Repository::open(git_dir, &RepositoryOptions::empty())?;
    let wants: Vec<String> = wants.iter().map(ObjectId::to_hex).collect();
    let have_args: Vec<String> = haves.iter().map(|h| format!("^{}", h.to_hex())).collect();
    let mut rev_args = wants;
    rev_args.extend(have_args);
    let opts = crate::rev_list::RevListOptions {
        objects: true,
        no_object_names: true,
        quiet: true,
        filter: filter.cloned(),
        ..Default::default()
    };
    let result = crate::rev_list::rev_list(&repo, &rev_args, &[], &opts)?;
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    for oid in result
        .commits
        .iter()
        .chain(result.objects.iter().map(|(o, _)| o))
    {
        if seen.insert(*oid) {
            ordered.push(*oid);
        }
    }
    Ok((ordered, HashMap::new()))
}

#[allow(clippy::type_complexity)]
fn collect_reachable_excluding_with_name_hashes(
    odb: &Odb,
    roots: &[ObjectId],
    exclude: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
    prefetch_walk_objects: bool,
) -> Result<(
    Vec<ObjectId>,
    HashMap<ObjectId, u32>,
    HashMap<ObjectId, Object>,
)> {
    let mut visited: OidHashSet = oid_hash_set();
    let mut ordered: Vec<ObjectId> = Vec::new();
    let mut queue: VecDeque<(ObjectId, String)> = VecDeque::new();
    let mut blob_name_hashes: HashMap<ObjectId, u32> = HashMap::new();
    let mut walk_prefetch: HashMap<ObjectId, Object> = HashMap::new();

    let record = |oid: ObjectId, visited: &mut OidHashSet, ordered: &mut Vec<ObjectId>| -> bool {
        if !visited.insert(oid) {
            return false;
        }
        if !exclude.contains(&oid) {
            ordered.push(oid);
        }
        true
    };
    let enqueue = |oid: ObjectId,
                   path: String,
                   queue: &mut VecDeque<(ObjectId, String)>,
                   visited: &mut OidHashSet,
                   ordered: &mut Vec<ObjectId>|
     -> bool {
        if !record(oid, visited, ordered) {
            return false;
        }
        queue.push_back((oid, path));
        true
    };

    for &root in roots {
        enqueue(root, String::new(), &mut queue, &mut visited, &mut ordered);
    }

    while let Some((oid, path)) = queue.pop_front() {
        if !prefetch_walk_objects {
            let info = match odb.read_info(&oid) {
                Ok(i) => i,
                Err(_) if skip_missing => continue,
                Err(e) => return Err(e),
            };
            if info.kind == ObjectKind::Blob {
                continue;
            }
        }
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            Err(_) if skip_missing => continue,
            Err(e) => return Err(e),
        };
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                if !shallow_grafts.contains(&oid) {
                    for parent in commit.parents {
                        enqueue(
                            parent,
                            String::new(),
                            &mut queue,
                            &mut visited,
                            &mut ordered,
                        );
                    }
                }
                enqueue(
                    commit.tree,
                    String::new(),
                    &mut queue,
                    &mut visited,
                    &mut ordered,
                );
            }
            ObjectKind::Tree => {
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    let name = String::from_utf8_lossy(&entry.name);
                    let child_path = if path.is_empty() {
                        name.into_owned()
                    } else {
                        format!("{path}/{name}")
                    };
                    let is_file = entry.mode & 0o170000 == 0o100000;
                    if entry.mode == 0o040000 {
                        enqueue(
                            entry.oid,
                            child_path,
                            &mut queue,
                            &mut visited,
                            &mut ordered,
                        );
                    } else if !prefetch_walk_objects && (is_file || entry.mode == 0o120000) {
                        // Blobs have nothing to walk: record them from the tree entry
                        // instead of reading each one back from the store.
                        if record(entry.oid, &mut visited, &mut ordered) && is_file {
                            blob_name_hashes.insert(entry.oid, pack_name_hash(&child_path));
                        }
                    } else if is_file {
                        if enqueue(
                            entry.oid,
                            child_path.clone(),
                            &mut queue,
                            &mut visited,
                            &mut ordered,
                        ) {
                            blob_name_hashes.insert(entry.oid, pack_name_hash(&child_path));
                        }
                    } else {
                        enqueue(
                            entry.oid,
                            child_path,
                            &mut queue,
                            &mut visited,
                            &mut ordered,
                        );
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                enqueue(tag.object, path, &mut queue, &mut visited, &mut ordered);
            }
            ObjectKind::Blob => {}
        }
        if prefetch_walk_objects {
            walk_prefetch.insert(oid, obj);
        }
    }

    Ok((ordered, blob_name_hashes, walk_prefetch))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::NullProgress;
    use crate::repo::{InitOptions, Repository};
    use tempfile::TempDir;

    #[test]
    fn walk_prefetch_skipped_when_prefetch_disabled() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("blob"), b"payload").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        let tip = create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
        )
        .expect("commit");
        let empty = HashSet::new();
        let with_prefetch = enumerate_pack_objects(
            repo.odb(),
            &[tip.oid],
            &[],
            &PackEnumerateOptions {
                filter: None,
                shallow_grafts: &empty,
                have_shallow_grafts: Some(&empty),
                use_bitmaps: false,
                exclude_objects: None,
                prefetch_walk_objects: true,
            },
        )
        .expect("prefetch");
        assert!(
            !with_prefetch.walk_prefetch.is_empty(),
            "default walk should prefetch object bodies"
        );
        let without_prefetch = enumerate_pack_objects(
            repo.odb(),
            &[tip.oid],
            &[],
            &PackEnumerateOptions {
                filter: None,
                shallow_grafts: &empty,
                have_shallow_grafts: Some(&empty),
                use_bitmaps: false,
                exclude_objects: None,
                prefetch_walk_objects: false,
            },
        )
        .expect("no prefetch");
        assert_eq!(
            with_prefetch.send, without_prefetch.send,
            "send list must match"
        );
        assert!(
            without_prefetch.walk_prefetch.is_empty(),
            "reuse-only repack walk must not retain blob payloads"
        );
    }
}
