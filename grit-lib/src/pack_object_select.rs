//! Object selection for pack-objects (walk or pack bitmap reachability).

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use crate::alternate_boundary::AlternateSources;
use crate::bitmap_walk::{BitmapWalkError, ReachabilityQuery, ReachableSet};
use crate::commit_graph_file::{CommitGraphChain, UNRANKED_GENERATION};
use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::objects::{
    oid_hash_map, oid_hash_set, oid_hash_set_with_capacity, parse_tag, tag_object_line_oid,
    CommitRef, Object, ObjectId, ObjectKind, OidHashMap, OidHashSet, TreeRefIter,
};
use crate::odb::store::{CompositeStore, ObjectStore};
use crate::odb::Odb;
use crate::pack_bitmap::BitmapIndex;
use crate::pack_name_hash::pack_name_hash;
use crate::progress::CancelSignal;
use crate::repo::Repository;
use crate::rev_list::{MissingAction, ObjectFilter};

/// Objects to emit in a negotiated pack plus peer-held closure for thin deltas.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub struct PackObjectSelection {
    /// Objects reachable from `wants` but not from `haves`, in valid pack order.
    pub send: Vec<ObjectId>,
    /// Peer-held commits whose trees seed thin-pack delta bases, not the full peer
    /// object closure: the edge commits where the object walk reached the peer's
    /// history, or the negotiation `haves` when bitmaps (or a path filter) selected
    /// the objects instead.
    pub have_closure: HashSet<ObjectId>,
    /// Peer-held objects when pack reachability bitmaps are available (thin-pack bases).
    pub peer_held: Option<Arc<ReachableSet>>,
    /// Pack name hash of the path each object was found at, used to pair delta
    /// candidates: blobs from the tree walk, every object the bitmap name-hash
    /// cache covers when bitmaps are used.
    pub name_hashes: HashMap<ObjectId, u32>,
}

impl fmt::Debug for PackObjectSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackObjectSelection")
            .field("send", &self.send)
            .field("have_closure", &self.have_closure)
            .field(
                "peer_held",
                &self
                    .peer_held
                    .as_ref()
                    .map(|held| held.count())
                    .unwrap_or(0),
            )
            .field("name_hashes", &self.name_hashes)
            .finish()
    }
}

/// How client `haves` bound the want-side object walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HaveBoundary {
    /// Pre-walk all commits reachable from every `have` before the want walk.
    #[default]
    CommitClosure,
    /// Git's `--revs` / `^have` walk, used by send-pack and upload-pack: peel have
    /// tips, mark them uninteresting, and interleave a commit-date walk with wants
    /// until the queue is only uninteresting commits (no full have closure pre-walk).
    TipsOnly,
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
    /// closure. Defaults to [`Self::shallow_grafts`] when unset. When non-empty,
    /// bitmaps are not used: a bitmap closure of a `have` runs past the client's
    /// shallow boundary into history the client does not hold.
    pub have_shallow_grafts: Option<&'a HashSet<ObjectId>>,
    /// When false, always use the object walk even if bitmaps exist.
    pub use_bitmaps: bool,
    /// Extra negotiation boundaries merged into the walk (legacy local-fetch hints).
    pub exclude_objects: Option<&'a HashSet<ObjectId>>,
    /// When `wants` are read from another ODB, objects already stored here are boundaries.
    pub peer_odb: Option<&'a Odb>,
    /// Whether `haves` are expanded to a full commit closure before the want walk.
    pub have_boundary: HaveBoundary,
}

impl PackEnumerateOptions<'_> {
    /// Whether any shallow graft bounds either side of the walk, which rules out
    /// bitmap reachability (bitmaps record full, ungrafted closures).
    fn has_shallow_grafts(&self) -> bool {
        !self.shallow_grafts.is_empty() || self.have_shallow_grafts.is_some_and(|g| !g.is_empty())
    }
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
    enumerate_pack_objects_naming(
        odb,
        wants,
        haves,
        opts,
        NameTrees::No,
        false,
        &CancelSignal::never(),
    )
}

/// Whether the object walk records name hashes for trees as well as blobs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameTrees {
    /// Blobs only: the packers that pair only blobs skip the extra entries.
    No,
    /// Blobs and trees, for a delta search that pairs trees too.
    Yes,
}

impl NameTrees {
    /// [`Self::Yes`] when a pack built with `opts` runs a new delta search, or is thin
    /// (receiver-held bases are matched to sent trees by path).
    pub(crate) fn for_delta_search(opts: &crate::pack_objects::PackBuildOptions) -> Self {
        if opts.delta && (opts.thin || (opts.window > 0 && opts.max_depth > 0)) {
            Self::Yes
        } else {
            Self::No
        }
    }
}

/// Like [`enumerate_pack_objects`], choosing whether the walk names trees and
/// stopping with [`crate::error::Error::Cancelled`] once `stop` fires (polled for
/// every commit and object the walk reads).
pub(crate) fn enumerate_pack_objects_naming(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
    name_trees: NameTrees,
    local_only: bool,
    stop: &CancelSignal<'_>,
) -> Result<PackObjectSelection> {
    stop.poll()?;
    if !local_only && opts.use_bitmaps {
        if let Some(sel) = try_bitmap_enumeration(odb, wants, haves, opts)? {
            stop.poll()?;
            return Ok(sel);
        }
    }
    let alternate_sources = if local_only {
        AlternateSources::load(odb)?.composite()
    } else {
        None
    };
    walk_enumeration(odb, wants, haves, opts, name_trees, alternate_sources, stop)
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
    match index.reachability_and_haves(
        &repo,
        ReachabilityQuery {
            wants,
            haves,
            filter,
        },
        MissingAction::Error,
        MissingAction::Allow,
    ) {
        Ok(sets) => Ok(Some(sets)),
        Err(BitmapWalkError::Unsupported(_)) => Ok(None),
        Err(BitmapWalkError::Bitmap(_)) => Ok(None),
        Err(BitmapWalkError::Repository(e)) => Err(e),
    }
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
    if opts.has_shallow_grafts() {
        return Ok(None);
    }
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
    alternate_sources: Option<Arc<CompositeStore>>,
    stop: &CancelSignal<'_>,
) -> Result<PackObjectSelection> {
    let have_shallow = opts.have_shallow_grafts.unwrap_or(opts.shallow_grafts);
    let have_commits = match opts.have_boundary {
        HaveBoundary::CommitClosure => {
            commits_reachable_from_haves(odb, haves, have_shallow, true, stop)?
        }
        HaveBoundary::TipsOnly => {
            send_pack_uninteresting_commits(odb, wants, haves, have_shallow, stop)?
        }
    };
    let mut boundary: HashSet<ObjectId> = HashSet::new();
    if let Some(extra) = opts.exclude_objects {
        boundary.extend(extra.iter().copied());
    }
    let peer_held = try_peer_held_bitmap(odb, haves, opts)?;
    stop.poll()?;
    let (send, name_hashes, edges) = match opts.filter {
        Some(filter) if filter.uses_paths() => {
            let (send, name_hashes) =
                rev_list_filtered_objects(odb, wants, haves, filter, &have_commits, stop)?;
            (send, name_hashes, None)
        }
        filter => {
            let (send, name_hashes, edges) = collect_reachable_excluding_with_name_hashes(
                odb,
                wants,
                &HashSet::new(),
                &mut boundary,
                &have_commits,
                opts.peer_odb,
                alternate_sources,
                false,
                opts.shallow_grafts,
                filter,
                name_trees,
                stop,
            )?;
            (send, name_hashes, Some(edges))
        }
    };
    // Thin bases come only from the edge commits where the walk met the peer's
    // history, as Git's `--thin` does, not from every advertised tip.
    let have_closure = match edges {
        Some(edges) => edges,
        None => haves.iter().copied().collect(),
    };
    Ok(PackObjectSelection {
        send,
        have_closure,
        peer_held,
        name_hashes,
    })
}

/// An annotated tag ref that `include-tag` may add to a fetch pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TagCandidate {
    /// The tag object the ref names.
    pub(crate) tag: ObjectId,
    /// The non-tag object the tag chain ends at (the advertised `^{}` value).
    pub(crate) peeled: ObjectId,
}

/// Append to `send` every candidate tag whose peeled object `send` already holds,
/// with each tag object of its chain, the way `pack-objects --include-tag` does.
///
/// Cost follows the size of `send` and the number of candidates, never the history
/// behind them: `send` is scanned once (a second time only when a chain passes
/// through a tag no candidate names) and only the added tags are read.
///
/// # Parameters
///
/// - `odb`: the database the added tag objects are read from.
/// - `send`: the objects the pack holds; added tags are appended after them.
/// - `candidates`: the annotated tag refs the client may follow.
///
/// # Errors
///
/// [`Error::ObjectNotFound`] or [`Error::CorruptObject`] for an unreadable tag in a
/// chain that is added.
pub(crate) fn append_included_tags(
    odb: &Odb,
    send: &mut Vec<ObjectId>,
    candidates: &[TagCandidate],
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let mut named = oid_hash_set_with_capacity(candidates.len() * 2);
    for c in candidates {
        named.insert(c.tag);
        named.insert(c.peeled);
    }
    let present: OidHashSet = send
        .iter()
        .filter(|o| named.contains(*o))
        .copied()
        .collect();
    let mut added = included_tag_chains(odb, candidates, |oid| present.contains(oid))?;
    let unnamed: OidHashSet = added
        .iter()
        .filter(|o| !named.contains(*o))
        .copied()
        .collect();
    if !unnamed.is_empty() {
        let already_sent: OidHashSet = send
            .iter()
            .filter(|o| unnamed.contains(*o))
            .copied()
            .collect();
        added.retain(|oid| !already_sent.contains(oid));
    }
    send.extend(added);
    Ok(())
}

/// The tag objects `include-tag` adds for `candidates` to a pack whose membership
/// `in_pack` answers.
///
/// A candidate qualifies when its peeled object is in the pack and its tag is not;
/// its whole chain (tag of a tag) is then returned, minus objects already packed.
/// `in_pack` must be exact for candidate tags and peeled ids; for unnamed chain
/// tags a `false` only means the caller has to deduplicate them.
///
/// # Errors
///
/// [`Error::ObjectNotFound`] or [`Error::CorruptObject`] for an unreadable tag in a
/// qualifying chain.
pub(crate) fn included_tag_chains(
    odb: &Odb,
    candidates: &[TagCandidate],
    in_pack: impl Fn(&ObjectId) -> bool,
) -> Result<Vec<ObjectId>> {
    let mut seen = oid_hash_set();
    let mut out = Vec::new();
    for c in candidates {
        if !in_pack(&c.peeled) || in_pack(&c.tag) {
            continue;
        }
        let mut current = c.tag;
        while current != c.peeled && seen.insert(current) {
            if !in_pack(&current) {
                out.push(current);
            }
            let tag = odb.read(&current)?;
            current = tag_object_line_oid(&tag.data).ok_or_else(|| {
                Error::CorruptObject(format!("tag {current} has no valid object header"))
            })?;
        }
    }
    Ok(out)
}

const SEND_PACK_REACH_WANT: u8 = 1;
const SEND_PACK_REACH_HAVE: u8 = 2;

#[cfg(test)]
pub(crate) mod send_pack_walk_test_hooks {
    use std::cell::Cell;

    thread_local! {
        static COMMIT_READS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset_commit_reads() {
        COMMIT_READS.with(|c| c.set(0));
    }

    pub fn commit_reads() -> usize {
        COMMIT_READS.with(|c| c.get())
    }

    pub(crate) fn record_commit_read() {
        COMMIT_READS.with(|c| c.set(c.get().saturating_add(1)));
    }
}

fn send_pack_reach_uninteresting(reach: u8) -> bool {
    reach & SEND_PACK_REACH_HAVE != 0
}

fn send_pack_reach_interesting(reach: u8) -> bool {
    reach & SEND_PACK_REACH_WANT != 0 && !send_pack_reach_uninteresting(reach)
}

/// Commits marked uninteresting for send-pack (`--revs` with `^have` lines).
///
/// Peels annotated tags on haves (skips missing ids and tags that peel to non-commits),
/// seeds wants and haves on a committer-date heap, and walks parents until every queued
/// commit is uninteresting or the heap is empty.
fn send_pack_uninteresting_commits(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    shallow_grafts: &HashSet<ObjectId>,
    stop: &CancelSignal<'_>,
) -> Result<HashSet<ObjectId>> {
    struct SendPackRevWalk<'a> {
        odb: &'a Odb,
        graph: Option<Arc<CommitGraphChain>>,
        shallow_grafts: &'a HashSet<ObjectId>,
        reach: HashMap<ObjectId, u8>,
        heap: BinaryHeap<QueuedCommit>,
        on_heap: HashSet<ObjectId>,
        processed: HashSet<ObjectId>,
        uninteresting: HashSet<ObjectId>,
        interesting_on_heap: usize,
        next_seq: u64,
    }

    impl<'a> SendPackRevWalk<'a> {
        fn note_reach(&mut self, oid: ObjectId, flag: u8) {
            let entry = self.reach.entry(oid).or_insert(0);
            let was_interesting = send_pack_reach_interesting(*entry);
            *entry |= flag;
            let is_interesting = send_pack_reach_interesting(*entry);
            if self.on_heap.contains(&oid) {
                match (was_interesting, is_interesting) {
                    (false, true) => self.interesting_on_heap += 1,
                    (true, false) => self.interesting_on_heap -= 1,
                    _ => {}
                }
            }
            if send_pack_reach_uninteresting(*entry) {
                self.uninteresting.insert(oid);
            }
        }

        /// Stop once no queued commit is still want-only (the interesting frontier).
        ///
        /// Unlike waiting for every uninteresting commit's parents to be marked, this
        /// matches Git's `still_interesting` cutoff and avoids walking all haves to root.
        fn can_stop_after(&self) -> bool {
            // Counting reach transitions avoids revisiting every queued have after each
            // commit, especially when a new commit is dated before advertised history.
            !self.heap.is_empty() && self.interesting_on_heap == 0
        }

        /// Note `oid` as reached with `flag`; true when it still has to be read and queued.
        fn reach_unqueued(&mut self, oid: ObjectId, flag: u8) -> bool {
            self.note_reach(oid, flag);
            !(self.processed.contains(&oid)
                || self.on_heap.contains(&oid)
                || self.shallow_grafts.contains(&oid))
        }

        fn try_enqueue(&mut self, oid: ObjectId, flag: u8) -> Result<()> {
            if !self.reach_unqueued(oid, flag) {
                return Ok(());
            }
            if self.queue_from_graph(oid) {
                return Ok(());
            }
            match self.odb.read(&oid) {
                Ok(obj) => self.queue_commit(oid, &obj),
                Err(_) => Ok(()),
            }
        }

        /// Queue the commit a ref tip names, following annotated tags.
        ///
        /// Each object of the chain is read once; missing objects and tags that end at a
        /// tree or blob are skipped.
        fn enqueue_tip(&mut self, tip: ObjectId, flag: u8) -> Result<()> {
            let mut oid = tip;
            let mut tags_seen: Option<HashSet<ObjectId>> = None;
            loop {
                if self.graph_lists(&oid) {
                    if self.reach_unqueued(oid, flag) {
                        self.queue_from_graph(oid);
                    }
                    return Ok(());
                }
                let Ok(obj) = self.odb.read(&oid) else {
                    return Ok(());
                };
                if obj.kind != ObjectKind::Tag {
                    if obj.kind != ObjectKind::Commit || !self.reach_unqueued(oid, flag) {
                        return Ok(());
                    }
                    return self.queue_commit(oid, &obj);
                }
                if !tags_seen.get_or_insert_with(HashSet::new).insert(oid) {
                    return Ok(());
                }
                let Some(target) = tag_object_line_oid(&obj.data) else {
                    return Ok(());
                };
                if self.processed.contains(&target) || self.on_heap.contains(&target) {
                    self.note_reach(target, flag);
                    return Ok(());
                }
                oid = target;
            }
        }

        fn graph_lists(&self, oid: &ObjectId) -> bool {
            self.graph
                .as_deref()
                .is_some_and(|g| g.find_commit(oid).is_some())
        }

        /// Queue `oid` from the commit-graph without reading the object; false when the
        /// graph does not list it.
        fn queue_from_graph(&mut self, oid: ObjectId) -> bool {
            let Some(entry) = self.graph.as_deref().and_then(|g| g.graph_walk_entry(&oid)) else {
                return false;
            };
            self.push_queued(QueuedCommit {
                generation: entry.generation,
                time: entry.time,
                seq: std::cmp::Reverse(self.next_seq),
                oid,
                tree: entry.tree,
                parents: entry.parents,
            });
            true
        }

        fn queue_commit(&mut self, oid: ObjectId, obj: &Object) -> Result<()> {
            if obj.kind != ObjectKind::Commit {
                return Ok(());
            }
            let parsed = CommitRef::parse(&obj.data)?;
            #[cfg(test)]
            send_pack_walk_test_hooks::record_commit_read();
            self.push_queued(QueuedCommit {
                generation: UNRANKED_GENERATION,
                time: parsed.committer_unix_seconds_for_ordering(),
                seq: std::cmp::Reverse(self.next_seq),
                oid,
                tree: parsed.tree(),
                parents: parsed.parents().to_vec(),
            });
            Ok(())
        }

        fn push_queued(&mut self, queued: QueuedCommit) {
            self.on_heap.insert(queued.oid);
            if send_pack_reach_interesting(*self.reach.get(&queued.oid).unwrap_or(&0)) {
                self.interesting_on_heap += 1;
            }
            self.heap.push(queued);
            self.next_seq += 1;
        }
    }

    // Commits the graph lists come from it without inflating the object, and are ordered by
    // generation: commits outside the graph (new local work) are expanded first, so a push whose
    // commits are dated before the remote's history does not drain that whole history before
    // reaching them. Shallow repositories keep reading commits, since the graph does not know
    // their grafted boundaries.
    let graph = if shallow_grafts.is_empty() {
        odb.commit_graph_chain()
    } else {
        None
    };
    let mut walk = SendPackRevWalk {
        odb,
        graph,
        shallow_grafts,
        reach: HashMap::new(),
        heap: BinaryHeap::new(),
        on_heap: HashSet::new(),
        processed: HashSet::new(),
        uninteresting: HashSet::new(),
        interesting_on_heap: 0,
        next_seq: 0,
    };

    for &want in wants {
        walk.enqueue_tip(want, SEND_PACK_REACH_WANT)?;
    }
    for &have in haves {
        walk.enqueue_tip(have, SEND_PACK_REACH_HAVE)?;
    }

    while let Some(commit) = walk.heap.pop() {
        stop.poll()?;
        walk.on_heap.remove(&commit.oid);
        if send_pack_reach_interesting(*walk.reach.get(&commit.oid).unwrap_or(&0)) {
            walk.interesting_on_heap -= 1;
        }
        if !walk.processed.insert(commit.oid) {
            continue;
        }
        let flags = *walk.reach.get(&commit.oid).unwrap_or(&0);
        if send_pack_reach_uninteresting(flags) {
            walk.uninteresting.insert(commit.oid);
        }
        let parent_flag = if send_pack_reach_uninteresting(flags) {
            SEND_PACK_REACH_HAVE
        } else {
            SEND_PACK_REACH_WANT
        };
        if !walk.shallow_grafts.contains(&commit.oid) {
            for parent in commit.parents {
                walk.try_enqueue(parent, parent_flag)?;
            }
        }
        if walk.can_stop_after() {
            break;
        }
    }
    Ok(walk.uninteresting)
}

/// Commits reachable from client `haves` (commit graph only, no trees).
fn commits_reachable_from_haves(
    odb: &Odb,
    haves: &[ObjectId],
    shallow_grafts: &HashSet<ObjectId>,
    skip_missing: bool,
    stop: &CancelSignal<'_>,
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut queue: VecDeque<ObjectId> = haves.iter().copied().collect();
    while let Some(oid) = queue.pop_front() {
        stop.poll()?;
        if !seen.insert(oid) {
            continue;
        }
        if shallow_grafts.contains(&oid) {
            continue;
        }
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            Err(_) if skip_missing => continue,
            Err(e) => return Err(e),
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let parsed = CommitRef::parse(&obj.data)?;
        for parent in parsed.parents() {
            if !seen.contains(parent) {
                queue.push_back(*parent);
            }
        }
    }
    Ok(seen)
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
    if haves.is_empty() || !opts.use_bitmaps || opts.has_shallow_grafts() {
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
    have_commits: &HashSet<ObjectId>,
    stop: &CancelSignal<'_>,
) -> Result<(Vec<ObjectId>, HashMap<ObjectId, u32>)> {
    let _ = have_commits;
    stop.poll()?;
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
        cancel_watch: Some(stop.watch_arc()),
        ..Default::default()
    };
    let result = crate::rev_list::rev_list(&repo, &wants, &haves, &opts)?;
    stop.poll()?;
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
/// (and, with [`NameTrees::Yes`], every tree) below a root tree, and the
/// `have_commits` the walk stopped at (its edge commits).
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
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn collect_reachable_excluding_with_name_hashes(
    odb: &Odb,
    roots: &[ObjectId],
    exclude: &HashSet<ObjectId>,
    boundary: &mut HashSet<ObjectId>,
    have_commits: &HashSet<ObjectId>,
    peer_odb: Option<&Odb>,
    alternate_sources: Option<Arc<CompositeStore>>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    name_trees: NameTrees,
    stop: &CancelSignal<'_>,
) -> Result<(Vec<ObjectId>, HashMap<ObjectId, u32>, HashSet<ObjectId>)> {
    let oid_len = odb.hash_algo().len();
    let mut walk = EnumerationWalk {
        odb,
        exclude,
        boundary,
        have_commits,
        peer_odb,
        alternate_sources,
        skip_missing,
        filter,
        depth_sensitive: filter.is_some_and(ObjectFilter::is_depth_sensitive),
        name_trees,
        visited: oid_hash_set(),
        tree_depths: oid_hash_map(),
        unlisted: oid_hash_set(),
        ordered: Vec::new(),
        commits: BinaryHeap::new(),
        next_seq: 0,
        queue: VecDeque::new(),
        name_hashes: HashMap::new(),
        edges: HashSet::new(),
    };
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
        stop.poll()?;
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
    }

    if let Some(dropped) = dropped_root_trees.filter(|d| !d.is_empty()) {
        walk.ordered.retain(|oid| !dropped.contains(oid));
    }
    Ok((walk.ordered, walk.name_hashes, walk.edges))
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
    boundary: &'a mut HashSet<ObjectId>,
    /// Uninteresting commits (send-pack) or full have closure (upload-pack).
    have_commits: &'a HashSet<ObjectId>,
    /// Objects already on the peer (local fetch): stop without listing.
    peer_odb: Option<&'a Odb>,
    /// Alternate object directories (`repack -l` / `pack-objects --local`).
    alternate_sources: Option<Arc<CompositeStore>>,
    skip_missing: bool,
    filter: Option<&'a ObjectFilter>,
    /// The filter's verdict for trees and blobs depends on where they are met.
    depth_sensitive: bool,
    name_trees: NameTrees,
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
    /// Have-side commits the walk stopped at (Git's edge commits).
    edges: HashSet<ObjectId>,
}

/// A commit waiting in [`EnumerationWalk::commits`], ordered newest first and,
/// among equal dates, in the order it was found.
struct QueuedCommit {
    /// Commit-graph generation; [`UNRANKED_GENERATION`] outside the graph, so those sort first.
    generation: u64,
    time: i64,
    seq: std::cmp::Reverse<u64>,
    oid: ObjectId,
    tree: ObjectId,
    parents: Vec<ObjectId>,
}

impl PartialEq for QueuedCommit {
    fn eq(&self, other: &Self) -> bool {
        (self.generation, self.time, self.seq) == (other.generation, other.time, other.seq)
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
        (self.generation, self.time, self.seq).cmp(&(other.generation, other.time, other.seq))
    }
}

impl<'a> EnumerationWalk<'a> {
    fn list(&mut self, oid: ObjectId) {
        if !self.exclude.contains(&oid) {
            self.ordered.push(oid);
        }
    }

    fn note_boundary(&mut self, oid: ObjectId) {
        self.visited.insert(oid);
    }

    fn at_object_boundary(&self, oid: ObjectId) -> bool {
        if self
            .alternate_sources
            .as_ref()
            .is_some_and(|s| s.contains(&oid).unwrap_or(false))
        {
            return true;
        }
        self.boundary.contains(&oid)
            || self
                .peer_odb
                .is_some_and(|peer| peer.exists_materialized_local_without_rescan(&oid))
    }

    fn seal_have_reachable_commit(&mut self, oid: ObjectId) -> Result<()> {
        if !self.visited.insert(oid) {
            return Ok(());
        }
        self.edges.insert(oid);
        self.boundary
            .extend(objects_in_commit_snapshot(self.odb, oid)?);
        Ok(())
    }

    fn commit_on_have_side(&mut self, oid: ObjectId) -> Result<bool> {
        Ok(self.have_commits.contains(&oid))
    }

    /// An object that is sent whatever the filter says: a root or a tag target.
    fn push_object(&mut self, oid: ObjectId, path: String) {
        if let Ok(true) = self.commit_on_have_side(oid) {
            let _ = self.seal_have_reachable_commit(oid);
            return;
        }
        if self.at_object_boundary(oid) {
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
        if self.commit_on_have_side(oid)? {
            return self.seal_have_reachable_commit(oid);
        }
        if self.at_object_boundary(oid) {
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
        if self.commit_on_have_side(oid)? {
            return self.seal_have_reachable_commit(oid);
        }
        if self.at_object_boundary(oid) {
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
            generation: UNRANKED_GENERATION,
            time: parsed.committer_unix_seconds_for_ordering(),
            seq: std::cmp::Reverse(self.next_seq),
            oid,
            tree: parsed.tree(),
            parents: parsed.parents().to_vec(),
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
        // A tree in a have commit's snapshot is already on the peer, whatever the filter.
        if self.at_object_boundary(oid) {
            self.note_boundary(oid);
            return;
        }
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
        if self.at_object_boundary(oid) {
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
                peer_odb: None,
                have_boundary: HaveBoundary::CommitClosure,
            },
            name_trees,
            false,
            &CancelSignal::never(),
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
    fn tips_only_boundary_matches_closure_on_linear_incremental_push() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let blob = |s: &str| repo.odb().write(ObjectKind::Blob, s.as_bytes()).unwrap();
        let mut parent = write_commit(
            &repo,
            write_tree(&repo, &[(0o100644, "f", blob("0"))]),
            &[],
            0,
        );
        for i in 1..800 {
            let tree = write_tree(&repo, &[(0o100644, "f", blob(&i.to_string()))]);
            parent = write_commit(&repo, tree, &[parent], i);
        }
        let remote_tip = parent;
        let new_tree = write_tree(&repo, &[(0o100644, "f", blob("new"))]);
        let new_tip = write_commit(&repo, new_tree, &[remote_tip], 900);
        let empty = HashSet::new();
        let closure_opts = PackEnumerateOptions {
            filter: None,
            shallow_grafts: &empty,
            have_shallow_grafts: Some(&empty),
            use_bitmaps: false,
            exclude_objects: None,
            peer_odb: None,
            have_boundary: HaveBoundary::CommitClosure,
        };
        let tips_opts = PackEnumerateOptions {
            have_boundary: HaveBoundary::TipsOnly,
            ..closure_opts
        };
        let closure = enumerate_pack_objects(repo.odb(), &[new_tip], &[remote_tip], &closure_opts)
            .expect("closure");
        let tips = enumerate_pack_objects(repo.odb(), &[new_tip], &[remote_tip], &tips_opts)
            .expect("tips");
        let closure_set: HashSet<ObjectId> = closure.send.iter().copied().collect();
        let tips_set: HashSet<ObjectId> = tips.send.iter().copied().collect();
        assert_eq!(closure_set, tips_set);
        assert!(
            tips.send.len() <= 8,
            "one new commit should enumerate a handful of objects, got {}",
            tips.send.len()
        );
    }

    #[test]
    fn tips_only_send_pack_does_not_prewalk_deep_history() {
        use crate::merge_base::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let blob = |s: &str| repo.odb().write(ObjectKind::Blob, s.as_bytes()).unwrap();
        let mut parent = write_commit(
            &repo,
            write_tree(&repo, &[(0o100644, "f", blob("0"))]),
            &[],
            0,
        );
        for i in 1..800 {
            let tree = write_tree(&repo, &[(0o100644, "f", blob(&i.to_string()))]);
            parent = write_commit(&repo, tree, &[parent], i);
        }
        let remote_tip = parent;
        let new_tree = write_tree(&repo, &[(0o100644, "f", blob("new"))]);
        let new_tip = write_commit(&repo, new_tree, &[remote_tip], 900);
        let haves = vec![remote_tip];
        let empty = HashSet::new();
        reset_commit_parent_lookups();
        let _selection = enumerate_pack_objects(
            repo.odb(),
            &[new_tip],
            &haves,
            &PackEnumerateOptions {
                filter: None,
                shallow_grafts: &empty,
                have_shallow_grafts: Some(&empty),
                use_bitmaps: false,
                exclude_objects: None,
                peer_odb: None,
                have_boundary: HaveBoundary::TipsOnly,
            },
        )
        .expect("enumerate");
        assert!(
            commit_parent_lookups() <= 128,
            "send-pack hide must stay coupled to the want walk ({} parent lookups)",
            commit_parent_lookups()
        );
    }

    #[test]
    fn push_dated_before_graphed_history_stops_at_the_frontier() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let tree = write_tree(&repo, &[]);
        // The remote's history, in the commit-graph, with a tag partway down.
        let mut tip = write_commit(&repo, tree, &[], 1_300_000_000);
        let mut tagged = tip;
        for i in 1..300 {
            tip = write_commit(&repo, tree, &[tip], 1_300_000_000 + i);
            if i == 150 {
                tagged = tip;
            }
        }
        std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).unwrap();
        repo.odb().write_commit_graph().unwrap();
        // New local work outside the graph, dated before all of that history (a skewed
        // clock, or fixed dates in a script).
        let mut new_tip = tip;
        for i in 0..3 {
            new_tip = write_commit(&repo, tree, &[new_tip], 1_000_000_000 + i);
        }

        reset_commit_reads();
        let hidden = send_pack_uninteresting_commits(
            repo.odb(),
            &[new_tip],
            &[tip, tagged],
            &HashSet::new(),
            &CancelSignal::never(),
        )
        .unwrap();
        assert!(hidden.contains(&tip));
        assert!(hidden.contains(&tagged));
        assert!(!hidden.contains(&new_tip));
        assert_eq!(
            commit_reads(),
            3,
            "only the commits outside the graph are read from the object store"
        );
        assert!(
            hidden.len() <= 4,
            "the walk must stop at the frontier, not drain the remote's history ({} marked)",
            hidden.len()
        );
    }

    #[test]
    fn duplicated_wants_already_on_peer_balance_the_frontier() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let tree = write_tree(&repo, &[]);
        let mut tip = write_commit(&repo, tree, &[], 0);
        for time in 1..100 {
            tip = write_commit(&repo, tree, &[tip], time);
        }
        reset_commit_reads();
        let hidden = send_pack_uninteresting_commits(
            repo.odb(),
            &[tip, tip],
            &[tip, tip],
            &HashSet::new(),
            &CancelSignal::never(),
        )
        .unwrap();
        assert!(hidden.contains(&tip));
        assert!(
            commit_reads() <= 2,
            "shared want/have must stop at the frontier"
        );
        let empty = HashSet::new();
        let selection = enumerate_pack_objects(
            repo.odb(),
            &[tip, tip],
            &[tip, tip],
            &PackEnumerateOptions {
                filter: None,
                shallow_grafts: &empty,
                have_shallow_grafts: None,
                use_bitmaps: false,
                exclude_objects: None,
                peer_odb: None,
                have_boundary: HaveBoundary::TipsOnly,
            },
        )
        .unwrap();
        assert!(selection.send.is_empty());
    }

    #[test]
    fn tips_only_many_advertised_haves_does_not_prewalk_deep_history() {
        use crate::merge_base::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let blob = |s: &str| repo.odb().write(ObjectKind::Blob, s.as_bytes()).unwrap();
        let mut chain = Vec::new();
        let mut parent = write_commit(
            &repo,
            write_tree(&repo, &[(0o100644, "f", blob("0"))]),
            &[],
            0,
        );
        chain.push(parent);
        for i in 1..800 {
            let tree = write_tree(&repo, &[(0o100644, "f", blob(&i.to_string()))]);
            parent = write_commit(&repo, tree, &[parent], i);
            chain.push(parent);
        }
        let remote_tip = parent;
        let new_tree = write_tree(&repo, &[(0o100644, "f", blob("new"))]);
        let new_tip = write_commit(&repo, new_tree, &[remote_tip], 900);
        let mut haves = vec![remote_tip];
        for n in (2..400).step_by(2) {
            let idx = chain.len().checked_sub(1 + n);
            if let Some(idx) = idx {
                haves.push(chain[idx]);
            }
        }
        let empty = HashSet::new();
        reset_commit_parent_lookups();
        let selection = enumerate_pack_objects(
            repo.odb(),
            &[new_tip],
            &haves,
            &PackEnumerateOptions {
                filter: None,
                shallow_grafts: &empty,
                have_shallow_grafts: Some(&empty),
                use_bitmaps: false,
                exclude_objects: None,
                peer_odb: None,
                have_boundary: HaveBoundary::TipsOnly,
            },
        )
        .expect("enumerate");
        assert!(
            selection.send.len() <= 8,
            "one new commit should stay small with many haves, got {}",
            selection.send.len()
        );
        assert!(
            commit_parent_lookups() <= 512,
            "many advertised haves must not pre-walk deep history ({} parent lookups)",
            commit_parent_lookups()
        );
    }

    #[test]
    fn send_pack_have_walk_stops_without_scanning_deep_haves() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};

        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let blob = |s: &str| repo.odb().write(ObjectKind::Blob, s.as_bytes()).unwrap();
        let mut parent = write_commit(
            &repo,
            write_tree(&repo, &[(0o100644, "f", blob("0"))]),
            &[],
            0,
        );
        let near_root = parent;
        for i in 1..800 {
            let tree = write_tree(&repo, &[(0o100644, "f", blob(&i.to_string()))]);
            parent = write_commit(&repo, tree, &[parent], i);
        }
        let direct_parent = parent;
        let new_tree = write_tree(&repo, &[(0o100644, "f", blob("new"))]);
        let new_tip = write_commit(&repo, new_tree, &[direct_parent], 900);
        let empty = HashSet::new();
        reset_commit_reads();
        let have_commits = send_pack_uninteresting_commits(
            repo.odb(),
            &[new_tip],
            &[direct_parent, near_root],
            &empty,
            &CancelSignal::never(),
        )
        .expect("send-pack have walk");
        assert!(have_commits.contains(&direct_parent));
        assert!(have_commits.contains(&near_root));
        let reads = commit_reads();
        assert!(
            reads < 20,
            "send-pack have walk should not scan deep history, read {reads} commits"
        );
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

    fn tree_entry(mode: &str, name: &str, oid: ObjectId) -> Vec<u8> {
        let mut bytes = format!("{mode} {name}\0").into_bytes();
        bytes.extend_from_slice(oid.as_bytes());
        bytes
    }

    /// Old tools wrote tree entries with non-canonical modes (`40755` for a
    /// directory, `100664` for a file). Git walks them by their type bits, so the
    /// enumeration must too, or everything below such a directory is left out.
    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
                peer_odb: None,
                have_boundary: HaveBoundary::CommitClosure,
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

    fn tips_only_options(empty: &HashSet<ObjectId>) -> PackEnumerateOptions<'_> {
        PackEnumerateOptions {
            filter: None,
            shallow_grafts: empty,
            have_shallow_grafts: Some(empty),
            use_bitmaps: false,
            exclude_objects: None,
            peer_odb: None,
            have_boundary: HaveBoundary::TipsOnly,
        }
    }

    fn write_tag(repo: &Repository, target: ObjectId, kind: &str, name: &str) -> ObjectId {
        let body = format!(
            "object {}\ntype {kind}\ntag {name}\ntagger A <a@example.com> 0 +0000\n\n{name}\n",
            target.to_hex()
        );
        repo.odb().write(ObjectKind::Tag, body.as_bytes()).unwrap()
    }

    #[test]
    fn tips_only_thin_bases_come_from_edge_commits_not_every_advertised_tip() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let blob = |s: &str| repo.odb().write(ObjectKind::Blob, s.as_bytes()).unwrap();
        let base_tree = write_tree(&repo, &[(0o100644, "f", blob("base"))]);
        let root = write_commit(&repo, base_tree, &[], 0);
        let main = write_commit(
            &repo,
            write_tree(&repo, &[(0o100644, "f", blob("main"))]),
            &[root],
            10,
        );
        let mut haves = vec![main];
        for i in 0..40 {
            let tree = write_tree(&repo, &[(0o100644, "f", blob(&format!("side {i}")))]);
            let side = write_commit(&repo, tree, &[root], 20 + i);
            haves.push(write_tag(&repo, side, "commit", &format!("t{i}")));
            haves.push(side);
        }
        let new_tip = write_commit(
            &repo,
            write_tree(&repo, &[(0o100644, "f", blob("main\nmore"))]),
            &[main],
            100,
        );
        let empty = HashSet::new();
        let selection =
            enumerate_pack_objects(repo.odb(), &[new_tip], &haves, &tips_only_options(&empty))
                .unwrap();
        assert_eq!(selection.send.len(), 3, "commit, tree and the changed blob");
        assert_eq!(
            selection.have_closure,
            HashSet::from([main]),
            "thin bases come from the commit the walk stopped at"
        );

        let closure = enumerate_pack_objects(
            repo.odb(),
            &[new_tip],
            &haves,
            &PackEnumerateOptions {
                have_boundary: HaveBoundary::CommitClosure,
                ..tips_only_options(&empty)
            },
        )
        .unwrap();
        assert_eq!(closure.send, selection.send);
        assert_eq!(
            closure.have_closure,
            HashSet::from([main]),
            "the full have pre-walk seeds thin bases from the same edge commit"
        );
    }

    #[test]
    fn send_pack_walk_peels_tag_haves_and_ignores_non_commit_targets() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let blob = repo.odb().write(ObjectKind::Blob, b"x").unwrap();
        let tree = write_tree(&repo, &[(0o100644, "f", blob)]);
        let old = write_commit(&repo, tree, &[], 0);
        let tip = write_commit(&repo, tree, &[old], 1);
        let tag = write_tag(&repo, tip, "commit", "v1");
        let nested = write_tag(&repo, tag, "tag", "v1-nested");
        let tree_tag = write_tag(&repo, tree, "tree", "tree-tag");
        let blob_tag = write_tag(&repo, blob, "blob", "blob-tag");
        let missing = ObjectId::from_hex("1234567890123456789012345678901234567890").unwrap();
        let new_tip = write_commit(&repo, tree, &[tip], 2);

        reset_commit_reads();
        let hidden = send_pack_uninteresting_commits(
            repo.odb(),
            &[new_tip],
            &[nested, tag, tree_tag, blob_tag, missing],
            &HashSet::new(),
            &CancelSignal::never(),
        )
        .unwrap();
        assert!(hidden.contains(&tip), "tags peel to the commit they name");
        for oid in [
            tag, nested, tree_tag, blob_tag, tree, blob, missing, new_tip,
        ] {
            assert!(!hidden.contains(&oid), "{oid} is not a have-side commit");
        }
        assert_eq!(
            commit_reads(),
            2,
            "the want and the tag-named commit, each queued once"
        );
    }
}
