//! Object selection for pack-objects (walk or pack bitmap reachability).

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::bitmap_walk::{BitmapWalkError, ReachabilityQuery, ReachableSet};
use crate::environment::RepositoryOptions;
use crate::error::Result;
use crate::objects::{
    oid_hash_map, oid_hash_set, parse_tag, CommitRef, Object, ObjectId, ObjectKind, OidHashMap,
    OidHashSet, TreeRefIter,
};
use crate::odb::Odb;
use crate::pack_bitmap::BitmapIndex;
use crate::pack_name_hash::pack_name_hash;
use crate::repo::Repository;
use crate::rev_list::{MissingAction, ObjectFilter};

/// Objects to emit in a negotiated pack plus peer-held closure for thin deltas.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub struct PackObjectSelection {
    /// Objects reachable from `wants` but not from `haves`, in valid pack order.
    pub send: Vec<ObjectId>,
    /// Negotiation boundary tips (`haves`), not the full peer object closure.
    pub have_closure: HashSet<ObjectId>,
    /// Peer-held objects when pack reachability bitmaps are available (thin-pack bases).
    pub peer_held: Option<Arc<ReachableSet>>,
    /// Pack name hash of the path each object was found at, used to pair delta
    /// candidates: blobs from the tree walk, every object the bitmap name-hash
    /// cache covers when bitmaps are used.
    pub name_hashes: HashMap<ObjectId, u32>,
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
    /// When true, the commits, trees and tags read by the object walk are kept in
    /// [`PackObjectSelection::walk_prefetch`] so the pack builder does not read them
    /// again. They are kept only for repositories without local packs, where the
    /// builder needs payloads up front; with packs, objects are copied from the packs
    /// at write time. Blobs found in trees are never read by the walk.
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
    enumerate_pack_objects_naming(odb, wants, haves, opts, NameTrees::No)
}

/// Whether the object walk records name hashes for trees as well as blobs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameTrees {
    /// Blobs only: the packers that pair only blobs skip the extra entries.
    No,
    /// Blobs and trees, for a delta search that pairs trees too.
    Yes,
}

/// Like [`enumerate_pack_objects`], choosing whether the walk names trees.
pub(crate) fn enumerate_pack_objects_naming(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
    name_trees: NameTrees,
) -> Result<PackObjectSelection> {
    if opts.use_bitmaps {
        if let Some(sel) = try_bitmap_enumeration(odb, wants, haves, opts)? {
            return Ok(sel);
        }
    }
    walk_enumeration(odb, wants, haves, opts, name_trees)
}

/// Bitmap reachability for upload-pack when shallow grafts and filters allow it.
///
/// Returns the send-side [`ReachableSet`] and the peer-held [`ReachableSet`].
pub fn try_bitmap_reachable_for_upload(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: Option<&ObjectFilter>,
    shallow_grafts: &HashSet<ObjectId>,
) -> Result<Option<(ReachableSet, ReachableSet)>> {
    if !shallow_grafts.is_empty() || filter.is_some_and(selects_object_type) {
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
    let missing_wants = MissingAction::Error;

    let Some(have_set) = try_bitmap_peer_held(odb, haves, shallow_grafts)? else {
        return Ok(None);
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

    Ok(Some((send_set, have_set)))
}

/// Bitmap `object:type` selection also drops the wanted commits and tags, which a
/// pack must always carry, so those filters use the object walk.
fn selects_object_type(filter: &ObjectFilter) -> bool {
    match filter {
        ObjectFilter::ObjectType(_) => true,
        ObjectFilter::Combine(parts) => parts.iter().any(selects_object_type),
        _ => false,
    }
}

fn try_bitmap_enumeration(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
) -> Result<Option<PackObjectSelection>> {
    let Some((send_set, peer_held)) =
        try_bitmap_reachable_for_upload(odb, wants, haves, opts.filter, opts.shallow_grafts)?
    else {
        return Ok(None);
    };
    let index = send_set.bitmap_index();
    let send: Vec<ObjectId> = send_set.iter_grouped_by_kind().collect();
    let have_closure: HashSet<ObjectId> = haves.iter().copied().collect();
    let name_hashes = name_hashes_from_bitmap(index, &send, &HashSet::new());
    Ok(Some(PackObjectSelection {
        send,
        have_closure,
        peer_held: Some(Arc::new(peer_held)),
        name_hashes,
        walk_prefetch: HashMap::new(),
    }))
}

pub(crate) fn name_hashes_for_send(
    index: &BitmapIndex,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
) -> HashMap<ObjectId, u32> {
    name_hashes_from_bitmap(index, send, have_closure)
}

fn name_hashes_from_bitmap(
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
    name_trees: NameTrees,
) -> Result<PackObjectSelection> {
    let mut boundary: HashSet<ObjectId> = haves.iter().copied().collect();
    for &have in haves {
        boundary.extend(objects_in_commit_snapshot(odb, have)?);
    }
    if let Some(extra) = opts.exclude_objects {
        boundary.extend(extra.iter().copied());
    }
    let have_closure: HashSet<ObjectId> = haves.iter().copied().collect();
    let peer_held = try_peer_held_bitmap(odb, haves, opts)?;
    let (send, name_hashes, walk_prefetch) = match opts.filter {
        Some(filter) if filter.uses_paths() => {
            let (send, name_hashes) = rev_list_filtered_objects(odb, wants, haves, filter)?;
            (send, name_hashes, HashMap::new())
        }
        filter => {
            // With local packs the delta planner copies packed objects at write time
            // instead of reusing walked payloads, so holding them would only cost memory.
            let keep_payloads = opts.prefetch_walk_objects
                && !crate::pack_objects::objects_dir_has_local_packs(odb.objects_dir());
            collect_reachable_excluding_with_name_hashes(
                odb,
                wants,
                &HashSet::new(),
                &boundary,
                false,
                opts.shallow_grafts,
                keep_payloads,
                filter,
                name_trees,
            )?
        }
    };
    Ok(PackObjectSelection {
        send,
        have_closure,
        peer_held,
        name_hashes,
        walk_prefetch,
    })
}

/// Objects in a commit's root tree (the commit, its trees and blobs), for negotiation boundaries.
fn objects_in_commit_snapshot(odb: &Odb, commit_oid: ObjectId) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    out.insert(commit_oid);
    let commit_obj = match odb.read(&commit_oid) {
        Ok(o) => o,
        Err(_) => return Ok(out),
    };
    if commit_obj.kind != ObjectKind::Commit {
        return Ok(out);
    }
    let commit = CommitRef::parse(&commit_obj.data)?;
    let oid_len = odb.hash_algo().len();
    let mut queue = VecDeque::from([commit.tree()]);
    while let Some(tree_oid) = queue.pop_front() {
        if !out.insert(tree_oid) {
            continue;
        }
        let tree_obj = match odb.read(&tree_oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        for entry in TreeRefIter::with_oid_len(&tree_obj.data, oid_len) {
            let entry = entry?;
            if entry.mode == 0o160000 {
                continue;
            }
            if entry.mode & 0o170000 == 0o040000 {
                queue.push_back(entry.oid);
            } else {
                out.insert(entry.oid);
            }
        }
    }
    Ok(out)
}

fn try_peer_held_bitmap(
    odb: &Odb,
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
) -> Result<Option<Arc<ReachableSet>>> {
    if haves.is_empty() || !opts.use_bitmaps {
        return Ok(None);
    }
    let Some(peer) = try_bitmap_peer_held(odb, haves, opts.shallow_grafts)? else {
        return Ok(None);
    };
    Ok(Some(Arc::new(peer)))
}

fn try_bitmap_peer_held(
    odb: &Odb,
    haves: &[ObjectId],
    shallow_grafts: &HashSet<ObjectId>,
) -> Result<Option<ReachableSet>> {
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
    match index.reachability(
        &repo,
        ReachabilityQuery {
            wants: haves,
            haves: &[],
            filter: None,
        },
        MissingAction::Allow,
    ) {
        Ok(set) => Ok(Some(set)),
        Err(BitmapWalkError::Unsupported(_)) => Ok(None),
        Err(BitmapWalkError::Bitmap(_)) => Ok(None),
        Err(BitmapWalkError::Repository(e)) => Err(e),
    }
}

/// Objects for a path-based filter (`sparse:oid`), which needs the full object walk.
fn rev_list_filtered_objects(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: &ObjectFilter,
) -> Result<(Vec<ObjectId>, HashMap<ObjectId, u32>)> {
    let Some(git_dir) = odb.config_git_dir() else {
        return Ok((Vec::new(), HashMap::new()));
    };
    let repo = Repository::open(git_dir, &RepositoryOptions::empty())?;
    let wants: Vec<String> = wants.iter().map(ObjectId::to_hex).collect();
    let haves: Vec<String> = haves.iter().map(ObjectId::to_hex).collect();
    let opts = crate::rev_list::RevListOptions {
        objects: true,
        no_object_names: true,
        quiet: true,
        filter: Some(filter.clone()),
        ..Default::default()
    };
    let result = crate::rev_list::rev_list(&repo, &wants, &haves, &opts)?;
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

/// Walk objects reachable from `roots`, with the path name hash of every blob
/// (and, with [`NameTrees::Yes`], every tree) below a root tree.
///
/// Commits are walked newest committer date first (the order Git's revision walk
/// uses), all of them before any tree is expanded, so the versions of one path are
/// listed newest first: delta search breaks size ties by that order. Tree and blob
/// roots (index entries, cache-tree nodes) carry no path and are walked last, so
/// objects reachable from history keep the path they have there.
///
/// `roots` and the objects tags point at are always listed. With `filter`, parent
/// commits, trees and blobs are listed only when they pass it, the way
/// `pack-objects --filter` selects them: a tree's depth counts from its commit's root
/// tree (`0`), and a blob is checked against the depth of the tree holding it. As in
/// Git, a root or tag target that is also some commit's root tree counts as that root
/// tree. Trees whose entries cannot pass are not read, and blob sizes are only looked
/// up for `blob:limit`.
///
/// When `keep_payloads` is set, the commits, trees and tags read by the walk are
/// returned as well so the pack builder does not read them again.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn collect_reachable_excluding_with_name_hashes(
    odb: &Odb,
    roots: &[ObjectId],
    exclude: &HashSet<ObjectId>,
    boundary: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
    keep_payloads: bool,
    filter: Option<&ObjectFilter>,
    name_trees: NameTrees,
) -> Result<(
    Vec<ObjectId>,
    HashMap<ObjectId, u32>,
    HashMap<ObjectId, Object>,
)> {
    let oid_len = odb.hash_algo().len();
    let mut walk = EnumerationWalk {
        odb,
        exclude,
        boundary,
        skip_missing,
        filter,
        depth_sensitive: filter.is_some_and(ObjectFilter::is_depth_sensitive),
        name_trees,
        keep_payloads,
        visited: oid_hash_set(),
        tree_depths: oid_hash_map(),
        unlisted: oid_hash_set(),
        ordered: Vec::new(),
        commits: BinaryHeap::new(),
        next_seq: 0,
        queue: VecDeque::new(),
        name_hashes: HashMap::new(),
    };
    let mut walk_prefetch: HashMap<ObjectId, Object> = HashMap::new();
    // Commit root trees the filter drops, even where a root or tag names them.
    let mut dropped_root_trees: Option<OidHashSet> = filter
        .filter(|f| !f.includes_tree(0))
        .map(|_| oid_hash_set());

    let mut path_less_roots = Vec::new();
    for &root in roots {
        match odb.read_info(&root) {
            Ok(info) if matches!(info.kind, ObjectKind::Tree | ObjectKind::Blob) => {
                path_less_roots.push(root);
            }
            _ => walk.push_root(root)?,
        }
    }
    let mut path_less_roots = path_less_roots.into_iter();

    loop {
        if let Some(commit) = walk.commits.pop() {
            if !shallow_grafts.contains(&commit.oid) {
                for parent in commit.parents {
                    walk.push_parent(parent)?;
                }
            }
            if let Some(dropped) = dropped_root_trees.as_mut() {
                dropped.insert(commit.tree);
            }
            walk.push_tree(commit.tree, String::new(), 0);
            if let Some(obj) = commit.obj {
                walk_prefetch.insert(commit.oid, obj);
            }
            continue;
        }
        let Some((oid, path, depth)) = walk.queue.pop_front() else {
            match path_less_roots.next() {
                Some(root) => {
                    walk.push_object(root, String::new());
                    continue;
                }
                None => break,
            }
        };
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            Err(_) if skip_missing => continue,
            Err(e) => return Err(e),
        };
        match obj.kind {
            ObjectKind::Commit => {
                walk.queue_commit(oid, obj)?;
                continue;
            }
            ObjectKind::Tree => {
                for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                    let entry = entry?;
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    // Regular files and symlinks have no children: record them
                    // without inflating their contents. The pack writer reads (or
                    // copies verbatim) each one later and fails there if it is
                    // missing. Any other mode, including directories written with
                    // non-canonical permission bits, is read so the object's own
                    // kind decides whether it is walked.
                    match entry.mode & 0o170000 {
                        0o120000 => walk.push_blob(entry.oid, None, depth)?,
                        0o100000 => {
                            walk.push_blob(entry.oid, Some((&path, entry.name)), depth)?;
                        }
                        file_type => {
                            let child_path = child_path(&path, entry.name);
                            if file_type == 0o040000 {
                                walk.push_tree(entry.oid, child_path, depth + 1);
                            } else {
                                walk.push_object(entry.oid, child_path);
                            }
                        }
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                walk.push_object(tag.object, path);
            }
            ObjectKind::Blob => {}
        }
        if keep_payloads {
            walk_prefetch.insert(oid, obj);
        }
    }

    if let Some(dropped) = dropped_root_trees.filter(|d| !d.is_empty()) {
        walk.ordered.retain(|oid| !dropped.contains(oid));
    }
    Ok((walk.ordered, walk.name_hashes, walk_prefetch))
}

fn child_path(parent: &str, name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    if parent.is_empty() {
        name.into_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// State of [`collect_reachable_excluding_with_name_hashes`].
struct EnumerationWalk<'a> {
    odb: &'a Odb,
    exclude: &'a HashSet<ObjectId>,
    /// Negotiation boundaries: do not list or expand past these objects.
    boundary: &'a HashSet<ObjectId>,
    skip_missing: bool,
    filter: Option<&'a ObjectFilter>,
    /// The filter's verdict for trees and blobs depends on where they are met.
    depth_sensitive: bool,
    name_trees: NameTrees,
    /// Keep the raw object of queued commits for the caller.
    keep_payloads: bool,
    /// Objects listed (or held by the peer), and objects the filter left out whose
    /// verdict cannot change: parent commits, and without a depth-sensitive filter,
    /// trees and size-checked blobs.
    visited: OidHashSet,
    /// Shallowest depth each tree was walked at (depth-sensitive filters only).
    tree_depths: OidHashMap<u64>,
    /// Objects in [`Self::visited`] that the filter left out. A root or tag target
    /// naming one of them still lists it.
    unlisted: OidHashSet,
    ordered: Vec<ObjectId>,
    /// Commits read but not yet expanded, newest first.
    commits: BinaryHeap<QueuedCommit>,
    next_seq: u64,
    /// Objects to read: id, path for name hashes, and tree depth. Drained only
    /// while [`Self::commits`] is empty.
    queue: VecDeque<(ObjectId, String, u64)>,
    name_hashes: HashMap<ObjectId, u32>,
}

/// A commit waiting in [`EnumerationWalk::commits`], ordered newest first and,
/// among equal dates, in the order it was found.
struct QueuedCommit {
    time: i64,
    seq: std::cmp::Reverse<u64>,
    oid: ObjectId,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    /// The raw commit, kept only when the walk returns payloads.
    obj: Option<Object>,
}

impl PartialEq for QueuedCommit {
    fn eq(&self, other: &Self) -> bool {
        (self.time, self.seq) == (other.time, other.seq)
    }
}

impl Eq for QueuedCommit {}

impl PartialOrd for QueuedCommit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedCommit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.time, self.seq).cmp(&(other.time, other.seq))
    }
}

impl EnumerationWalk<'_> {
    fn list(&mut self, oid: ObjectId) {
        if !self.exclude.contains(&oid) {
            self.ordered.push(oid);
        }
    }

    fn note_boundary(&mut self, oid: ObjectId) {
        self.visited.insert(oid);
    }

    /// An object that is sent whatever the filter says: a root or a tag target.
    fn push_object(&mut self, oid: ObjectId, path: String) {
        if self.boundary.contains(&oid) {
            self.note_boundary(oid);
            return;
        }
        if self.visited.insert(oid) {
            self.list(oid);
            self.queue.push_back((oid, path, 0));
        } else if self.unlisted.remove(&oid) {
            self.list(oid);
        }
    }

    /// A root or tag target that may be a commit: read now so a commit enters the
    /// date-ordered heap at once; anything else waits in [`Self::queue`].
    fn push_root(&mut self, oid: ObjectId) -> Result<()> {
        if self.boundary.contains(&oid) {
            self.note_boundary(oid);
            return Ok(());
        }
        if !self.visited.insert(oid) {
            if self.unlisted.remove(&oid) {
                self.list(oid);
            }
            return Ok(());
        }
        self.list(oid);
        match self.read(&oid)? {
            Some(obj) if obj.kind == ObjectKind::Commit => self.queue_commit(oid, obj),
            Some(_) => {
                self.queue.push_back((oid, String::new(), 0));
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// A parent commit: always walked, listed unless the filter keeps other types only.
    /// Read now so it enters the date-ordered heap before any tree is expanded.
    fn push_parent(&mut self, oid: ObjectId) -> Result<()> {
        if self.boundary.contains(&oid) {
            self.note_boundary(oid);
            return Ok(());
        }
        let listed = self
            .filter
            .is_none_or(|f| f.includes_commit_or_tag_object(ObjectKind::Commit));
        if !self.visited.insert(oid) {
            if listed && self.unlisted.remove(&oid) {
                self.list(oid);
            }
            return Ok(());
        }
        if listed {
            self.list(oid);
        } else {
            self.unlisted.insert(oid);
        }
        match self.read(&oid)? {
            Some(obj) if obj.kind == ObjectKind::Commit => self.queue_commit(oid, obj),
            Some(_) => {
                self.queue.push_back((oid, String::new(), 0));
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        match self.odb.read(oid) {
            Ok(obj) => Ok(Some(obj)),
            Err(_) if self.skip_missing => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Hold a commit read from [`Self::queue`] until it is the newest one left.
    fn queue_commit(&mut self, oid: ObjectId, obj: Object) -> Result<()> {
        let parsed = CommitRef::parse(&obj.data)?;
        let queued = QueuedCommit {
            time: parsed.committer_unix_seconds_for_ordering(),
            seq: std::cmp::Reverse(self.next_seq),
            oid,
            tree: parsed.tree(),
            parents: parsed.parents().to_vec(),
            obj: self.keep_payloads.then_some(obj),
        };
        self.next_seq += 1;
        self.commits.push(queued);
        Ok(())
    }

    /// A commit's root tree (`depth` 0) or a subtree found in a tree.
    fn push_tree(&mut self, oid: ObjectId, path: String, depth: u64) {
        if self.name_trees == NameTrees::Yes && !path.is_empty() && !self.visited.contains(&oid) {
            self.name_hashes
                .entry(oid)
                .or_insert_with(|| pack_name_hash(&path));
        }
        let Some(filter) = self.filter else {
            self.push_object(oid, path);
            return;
        };
        let first_walk = if self.depth_sensitive {
            match self.tree_depths.get(&oid) {
                Some(&walked_at) if walked_at <= depth => false,
                _ => {
                    self.tree_depths.insert(oid, depth);
                    true
                }
            }
        } else {
            self.visited.insert(oid)
        };
        if !first_walk {
            return;
        }
        if filter.includes_tree(depth) {
            if !self.depth_sensitive || self.visited.insert(oid) {
                self.list(oid);
            }
        } else if !self.depth_sensitive {
            self.unlisted.insert(oid);
        }
        if !filter.prunes_tree_entries(depth) {
            self.queue.push_back((oid, path, depth));
        }
    }

    /// A blob in a tree at `tree_depth`; `name` is `(tree path, entry name)` for regular
    /// files, which get a pack name hash.
    fn push_blob(
        &mut self,
        oid: ObjectId,
        name: Option<(&str, &[u8])>,
        tree_depth: u64,
    ) -> Result<()> {
        if self.boundary.contains(&oid) {
            self.note_boundary(oid);
            return Ok(());
        }
        if self.visited.contains(&oid) {
            return Ok(());
        }
        let (passes, size_checked) = match self.filter {
            None => (true, false),
            Some(filter) => self.blob_passes(filter, &oid, tree_depth)?,
        };
        if passes {
            self.visited.insert(oid);
            self.list(oid);
            if let Some((parent, entry_name)) = name {
                self.name_hashes
                    .insert(oid, pack_name_hash(&child_path(parent, entry_name)));
            }
        } else if size_checked && !self.depth_sensitive {
            self.visited.insert(oid);
            self.unlisted.insert(oid);
        }
        Ok(())
    }

    /// Whether the blob passes `filter`, and whether its size had to be looked up.
    fn blob_passes(
        &self,
        filter: &ObjectFilter,
        oid: &ObjectId,
        tree_depth: u64,
    ) -> Result<(bool, bool)> {
        // A smaller blob never fares worse, so the size only matters between the extremes.
        if !filter.includes_blob_under_tree(0, tree_depth) {
            return Ok((false, false));
        }
        if filter.includes_blob_under_tree(u64::MAX, tree_depth) {
            return Ok((true, false));
        }
        match self.odb.read_info(oid) {
            Ok(info) => Ok((filter.includes_blob_under_tree(info.size, tree_depth), true)),
            Err(_) if self.skip_missing => Ok((false, true)),
            Err(e) => Err(e),
        }
    }
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

    fn walk(repo: &Repository, roots: &[ObjectId], name_trees: NameTrees) -> PackObjectSelection {
        let empty = HashSet::new();
        enumerate_pack_objects_naming(
            repo.odb(),
            roots,
            &[],
            &PackEnumerateOptions {
                filter: None,
                shallow_grafts: &empty,
                have_shallow_grafts: Some(&empty),
                use_bitmaps: false,
                exclude_objects: None,
                prefetch_walk_objects: false,
            },
            name_trees,
        )
        .expect("walk")
    }

    fn write_tree(repo: &Repository, entries: &[(u32, &str, ObjectId)]) -> ObjectId {
        let entries: Vec<crate::objects::TreeEntry> = entries
            .iter()
            .map(|&(mode, name, oid)| crate::objects::TreeEntry {
                mode,
                name: name.as_bytes().to_vec(),
                oid,
            })
            .collect();
        repo.odb()
            .write(ObjectKind::Tree, &crate::objects::serialize_tree(&entries))
            .expect("tree")
    }

    fn write_commit(
        repo: &Repository,
        tree: ObjectId,
        parents: &[ObjectId],
        time: i64,
    ) -> ObjectId {
        let mut body = format!("tree {}\n", tree.to_hex());
        for parent in parents {
            body.push_str(&format!("parent {}\n", parent.to_hex()));
        }
        body.push_str(&format!(
            "author A <a@example.com> {time} +0000\ncommitter A <a@example.com> {time} +0000\n\nc{time}\n"
        ));
        repo.odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("commit")
    }

    #[test]
    fn walk_lists_trees_of_newer_commits_first() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let blob = |s: &str| repo.odb().write(ObjectKind::Blob, s.as_bytes()).unwrap();
        let tree_a = write_tree(&repo, &[(0o100644, "f", blob("a"))]);
        let tree_b = write_tree(&repo, &[(0o100644, "f", blob("b"))]);
        let tree_c = write_tree(&repo, &[(0o100644, "f", blob("c"))]);
        let base = write_commit(&repo, tree_a, &[], 100);
        // Two branches: the older tip is listed as the first root.
        let old_tip = write_commit(&repo, tree_b, &[base], 200);
        let new_tip = write_commit(&repo, tree_c, &[base], 300);

        let selection = walk(&repo, &[old_tip, new_tip], NameTrees::No);
        let pos = |oid: ObjectId| selection.send.iter().position(|o| *o == oid).unwrap();
        assert!(pos(old_tip) < pos(base) && pos(new_tip) < pos(base));
        assert!(pos(tree_c) < pos(tree_b) && pos(tree_b) < pos(tree_a));
    }

    #[test]
    fn walk_names_trees_and_keeps_history_paths_for_path_less_roots() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let file = repo.odb().write(ObjectKind::Blob, b"x\n").unwrap();
        let link = repo.odb().write(ObjectKind::Blob, b"target").unwrap();
        let sub = write_tree(
            &repo,
            &[(0o100644, "file.rs", file), (0o120000, "link", link)],
        );
        let root = write_tree(&repo, &[(0o040000, "src", sub)]);
        let commit = write_commit(&repo, root, &[], 100);

        // Index roots (the subtree and the file) come first and have no path.
        let selection = walk(&repo, &[sub, file, commit], NameTrees::Yes);
        let hash = |oid: ObjectId| selection.name_hashes.get(&oid).copied();
        assert_eq!(hash(sub), Some(pack_name_hash("src")));
        assert_eq!(hash(file), Some(pack_name_hash("src/file.rs")));
        assert_eq!(hash(root), None, "root trees have no path");
        assert_eq!(selection.send.len(), 5);

        let blobs_only = walk(&repo, &[sub, file, commit], NameTrees::No);
        assert_eq!(blobs_only.name_hashes.get(&sub), None);
        assert_eq!(
            blobs_only.name_hashes.get(&file),
            Some(&pack_name_hash("src/file.rs"))
        );
    }

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
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
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

    fn tree_entry(mode: &str, name: &str, oid: ObjectId) -> Vec<u8> {
        let mut bytes = format!("{mode} {name}\0").into_bytes();
        bytes.extend_from_slice(oid.as_bytes());
        bytes
    }

    /// Old tools wrote tree entries with non-canonical modes (`40755` for a
    /// directory, `100664` for a file). Git walks them by their type bits, so the
    /// enumeration must too, or everything below such a directory is left out.
    #[test]
    fn walk_descends_into_trees_with_non_canonical_modes() {
        let tmp = tempfile::tempdir().unwrap();
        Repository::init(tmp.path(), &InitOptions::new().with_bare(true)).unwrap();
        let repo = Repository::open(tmp.path(), &RepositoryOptions::empty()).unwrap();
        let odb = repo.odb();
        let deep = odb
            .write_loose_materialize(ObjectKind::Blob, b"deep\n")
            .unwrap();
        let odd = odb
            .write_loose_materialize(ObjectKind::Blob, b"odd\n")
            .unwrap();
        let inner = odb
            .write_loose_materialize(ObjectKind::Tree, &tree_entry("100644", "deep.txt", deep))
            .unwrap();
        let mut root = tree_entry("100664", "odd.txt", odd);
        root.extend(tree_entry("40755", "sub", inner));
        let root = odb
            .write_loose_materialize(ObjectKind::Tree, &root)
            .unwrap();
        let commit = format!(
            "tree {root}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nodd modes\n"
        );
        let commit = odb
            .write_loose_materialize(ObjectKind::Commit, commit.as_bytes())
            .unwrap();

        let empty = HashSet::new();
        let selection = enumerate_pack_objects(
            odb,
            &[commit],
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
        .unwrap();

        let ours: HashSet<ObjectId> = selection.send.iter().copied().collect();
        let git: HashSet<ObjectId> =
            grit_test_support::git(tmp.path(), &["rev-list", "--objects", &commit.to_hex()])
                .lines()
                .map(|line| ObjectId::from_hex(&line[..40]).unwrap())
                .collect();
        assert_eq!(ours, git, "same objects as git rev-list --objects");
        assert!(ours.contains(&inner) && ours.contains(&deep));
        assert_eq!(selection.send.len(), 5);
        assert_eq!(
            selection.name_hashes.get(&deep),
            Some(&pack_name_hash("sub/deep.txt"))
        );
        assert_eq!(
            selection.name_hashes.get(&odd),
            Some(&pack_name_hash("odd.txt"))
        );
    }
}
