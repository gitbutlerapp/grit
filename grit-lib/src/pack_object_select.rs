//! Object selection for pack-objects (walk or pack bitmap reachability).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::bitmap_walk::{BitmapWalkError, ReachabilityQuery, ReachableSet};
use crate::environment::RepositoryOptions;
use crate::error::Result;
use crate::objects::{
    oid_hash_set, parse_commit, parse_tag, Object, ObjectId, ObjectKind, OidHashSet, TreeRefIter,
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
    /// Commits, trees and tags already read during a tree walk (avoids rereading loose
    /// objects for pack build). Blobs found in trees are never read by the walk.
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
        // With local packs the delta planner copies packed objects at write time
        // instead of reusing walked payloads, so holding them would only cost memory.
        let keep_payloads = !crate::pack_objects::objects_dir_has_local_packs(odb.objects_dir());
        collect_reachable_excluding_with_name_hashes(
            odb,
            wants,
            &have_closure,
            false,
            opts.shallow_grafts,
            keep_payloads,
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
        false,
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

/// Walk objects reachable from `roots` in discovery order, with blob name hashes.
///
/// When `keep_payloads` is set, the commits, trees and tags read by the walk are
/// returned as well so the pack builder does not read them again.
#[allow(clippy::type_complexity)]
fn collect_reachable_excluding_with_name_hashes(
    odb: &Odb,
    roots: &[ObjectId],
    exclude: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
    keep_payloads: bool,
) -> Result<(
    Vec<ObjectId>,
    HashMap<ObjectId, u32>,
    HashMap<ObjectId, Object>,
)> {
    let oid_len = odb.hash_algo().len();
    let mut visited: OidHashSet = oid_hash_set();
    let mut ordered: Vec<ObjectId> = Vec::new();
    let mut queue: VecDeque<(ObjectId, String)> = VecDeque::new();
    let mut blob_name_hashes: HashMap<ObjectId, u32> = HashMap::new();
    let mut walk_prefetch: HashMap<ObjectId, Object> = HashMap::new();

    let enqueue = |oid: ObjectId,
                   path: String,
                   queue: &mut VecDeque<(ObjectId, String)>,
                   visited: &mut OidHashSet,
                   ordered: &mut Vec<ObjectId>|
     -> bool {
        if !visited.insert(oid) {
            return false;
        }
        if !exclude.contains(&oid) {
            ordered.push(oid);
        }
        queue.push_back((oid, path));
        true
    };

    for &root in roots {
        enqueue(root, String::new(), &mut queue, &mut visited, &mut ordered);
    }

    while let Some((oid, path)) = queue.pop_front() {
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
                for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                    let entry = entry?;
                    if entry.mode == 0o160000 || !visited.insert(entry.oid) {
                        continue;
                    }
                    if !exclude.contains(&entry.oid) {
                        ordered.push(entry.oid);
                    }
                    // Blobs and symlinks have no children: record them without
                    // inflating their contents. The pack writer reads (or copies
                    // verbatim) each one later and fails there if it is missing.
                    let is_tree = entry.mode == 0o040000;
                    if !is_tree && entry.mode & 0o170000 != 0o100000 {
                        continue;
                    }
                    let name = String::from_utf8_lossy(entry.name);
                    let child_path = if path.is_empty() {
                        name.into_owned()
                    } else {
                        format!("{path}/{name}")
                    };
                    if is_tree {
                        queue.push_back((entry.oid, child_path));
                    } else {
                        blob_name_hashes.insert(entry.oid, pack_name_hash(&child_path));
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                enqueue(tag.object, path, &mut queue, &mut visited, &mut ordered);
            }
            ObjectKind::Blob => {}
        }
        if keep_payloads {
            walk_prefetch.insert(oid, obj);
        }
    }

    Ok((ordered, blob_name_hashes, walk_prefetch))
}
