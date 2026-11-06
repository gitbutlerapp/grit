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
    tree_entry_span, CommitRef, Object, ObjectId, ObjectKind, OidHashMap, OidHashSet, TreeEntryRef,
    TreeRefIter,
};
use crate::odb::store::{CompositeStore, ObjectStore};
use crate::odb::Odb;
use crate::pack::SingleLocalPackReader;
use crate::pack_bitmap::BitmapIndex;
use crate::pack_name_hash::{pack_name_hash, pack_name_hash_child};
use crate::pack_tree_read_ahead::{ReadAheadEntry, TreeReadAhead};
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
    crate::trace::trace_span!("grit.pack.enumerate");
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
    walk_enumeration(
        odb,
        wants,
        haves,
        opts,
        name_trees,
        alternate_sources,
        MissingBlobMode::Fail,
        stop,
    )
    .map(|(selection, _)| selection)
}

/// What the object walk does with a blob the repository does not store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MissingBlobMode {
    /// List it anyway; the pack writer fails when it reads it (the plain walk).
    Fail,
    /// Check that every blob it would list is stored, and set the missing ones
    /// aside as [`MissingBlob`]s instead of listing them.
    Probe,
}

/// A blob the walk would have sent but the repository does not store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MissingBlob {
    /// The missing blob.
    pub(crate) oid: ObjectId,
    /// The tree the walk found it in, when known.
    pub(crate) referrer: Option<ObjectId>,
}

/// Like [`enumerate_pack_objects_naming`] with [`MissingBlobMode::Probe`]: the object
/// walk always runs (bitmaps cannot see missing objects), and blobs the repository
/// lacks come back separately instead of being listed.
///
/// Missing commits, trees and tags still fail the walk.
pub(crate) fn enumerate_pack_objects_probing(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
    name_trees: NameTrees,
    stop: &CancelSignal<'_>,
) -> Result<(PackObjectSelection, Vec<MissingBlob>)> {
    stop.poll()?;
    let opts = PackEnumerateOptions {
        use_bitmaps: false,
        ..*opts
    };
    walk_enumeration(
        odb,
        wants,
        haves,
        &opts,
        name_trees,
        None,
        MissingBlobMode::Probe,
        stop,
    )
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

#[allow(clippy::too_many_arguments)]
fn walk_enumeration(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackEnumerateOptions<'_>,
    name_trees: NameTrees,
    alternate_sources: Option<Arc<CompositeStore>>,
    missing_mode: MissingBlobMode,
    stop: &CancelSignal<'_>,
) -> Result<(PackObjectSelection, Vec<MissingBlob>)> {
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
    let mut missing = Vec::new();
    let (send, name_hashes, edges) = match opts.filter {
        Some(filter) if filter.uses_paths() => {
            let (mut send, name_hashes) = rev_list_filtered_objects(
                odb,
                wants,
                haves,
                filter,
                &have_commits,
                missing_mode,
                stop,
            )?;
            if missing_mode == MissingBlobMode::Probe {
                missing = take_unstored_blobs(odb, &mut send)?;
            }
            (send, name_hashes, None)
        }
        filter => {
            let walked = collect_reachable_excluding_with_name_hashes(
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
                missing_mode,
                stop,
            )?;
            missing = walked.missing;
            (walked.send, walked.name_hashes, Some(walked.edges))
        }
    };
    // Thin bases come only from the edge commits where the walk met the peer's
    // history, as Git's `--thin` does, not from every advertised tip.
    let have_closure = match edges {
        Some(edges) => edges,
        None => haves.iter().copied().collect(),
    };
    Ok((
        PackObjectSelection {
            send,
            have_closure,
            peer_held,
            name_hashes,
        },
        missing,
    ))
}

/// Remove the objects `send` lists that the repository does not store and return
/// them as missing blobs. Path-filtered walks (`sparse:oid`) only ever list
/// commits and trees the walk has read, so anything absent is a blob.
fn take_unstored_blobs(odb: &Odb, send: &mut Vec<ObjectId>) -> Result<Vec<MissingBlob>> {
    let present = odb.contains_many(send)?;
    let mut missing = Vec::new();
    let mut present = present.into_iter();
    send.retain(|oid| {
        let keep = present.next().unwrap_or(true);
        if !keep {
            missing.push(MissingBlob {
                oid: *oid,
                referrer: None,
            });
        }
        keep
    });
    Ok(missing)
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
    // Only a have makes a commit uninteresting; without one the walk below would read
    // every commit reachable from the wants to find none.
    if haves.is_empty() {
        return Ok(HashSet::new());
    }
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
        header_buf: Vec<u8>,
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
            // Only the headers matter here, and without a commit-graph this walk can read
            // most of the history (new commits dated before the peer's tips), so skip
            // inflating messages wherever the pack stores the commit whole.
            let mut header = std::mem::take(&mut self.header_buf);
            let queued = if matches!(
                self.odb
                    .read_packed_commit_header_block_into(&oid, &mut header),
                Ok(true)
            ) {
                self.queue_commit_data(oid, &header)
            } else {
                match self.odb.read(&oid) {
                    Ok(obj) => self.queue_commit(oid, &obj),
                    Err(_) => Ok(()),
                }
            };
            self.header_buf = header;
            queued
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
                // A want is often a blob (a lazy fetch names thousands): the header
                // says so without inflating it.
                match self.odb.read_info(&oid) {
                    Ok(info) if matches!(info.kind, ObjectKind::Commit | ObjectKind::Tag) => {}
                    _ => return Ok(()),
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
            self.queue_commit_data(oid, &obj.data)
        }

        /// Queue commit `oid` from its payload, or from just its header block.
        fn queue_commit_data(&mut self, oid: ObjectId, data: &[u8]) -> Result<()> {
            let parsed = CommitRef::parse(data)?;
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
        header_buf: Vec::new(),
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
    missing_mode: MissingBlobMode,
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
        no_object_names: false,
        quiet: true,
        filter: Some(filter.clone()),
        cancel: Some(crate::progress::CancelToken::from_shared_flag(
            stop.watch_arc(),
        )),
        missing_action: match missing_mode {
            MissingBlobMode::Fail => MissingAction::Error,
            MissingBlobMode::Probe => MissingAction::Allow,
        },
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
    let mut name_hashes = HashMap::new();
    for (oid, path) in &result.objects {
        if !path.is_empty() {
            name_hashes
                .entry(*oid)
                .or_insert_with(|| pack_name_hash(path));
        }
    }
    Ok((ordered, name_hashes))
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
/// With [`MissingBlobMode::Probe`], blobs the repository does not store are not
/// listed but returned in [`WalkedObjects::missing`].
#[allow(clippy::too_many_arguments)]
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
    missing_mode: MissingBlobMode,
    stop: &CancelSignal<'_>,
) -> Result<WalkedObjects> {
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
        missing: (missing_mode == MissingBlobMode::Probe).then(Vec::new),
        session_pack: odb.open_session_pack_reader().ok().flatten(),
        expanded: filter.is_none().then(HashMap::new),
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
    // The walk's thread parses trees; the others read the queued ones ahead of it.
    let read_ahead_workers = crate::pack_delta::pack_delta_search_parallel_options(odb, None)
        .map_or(0, |par| par.threads.get() - 1);

    std::thread::scope(|scope| -> Result<()> {
        let mut read_ahead = TreeReadAhead::new(scope, odb, read_ahead_workers);
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
                walk.push_tree(commit.tree, None, 0);
                read_ahead.top_up(&walk.queue);
                continue;
            }
            let Some(QueuedObject {
                oid,
                path_hash,
                depth,
                found_as_tree,
            }) = walk.queue.pop_front()
            else {
                match path_less_roots.next() {
                    Some(root) => {
                        walk.push_object(root, None, false);
                        continue;
                    }
                    None => break,
                }
            };
            let read_ahead_result = read_ahead.take_front();
            read_ahead.top_up(&walk.queue);
            // Queued blobs are wanted directly (a lazy fetch names thousands); the
            // header says there is nothing to walk without inflating them. Objects
            // found as trees are read straight away; their own kind still decides below.
            if !found_as_tree {
                match odb.read_info(&oid) {
                    Ok(info) if info.kind == ObjectKind::Blob => continue,
                    Ok(_) => {}
                    Err(_) if skip_missing => continue,
                    Err(e) => return Err(e),
                }
            }
            let read = match read_ahead_result {
                Some(obj) => Ok(obj),
                None => walk.read_object(&oid),
            };
            let obj = match read {
                Ok(o) => o,
                Err(_) if skip_missing => continue,
                Err(e) => return Err(e),
            };
            walk.expand(oid, obj, path_hash, depth, oid_len)?;
        }
        Ok(())
    })?;
    // The delta bases cached while reading trees are not read again by what follows a
    // walk: delta planning reads entry headers and the writer copies stored entries.
    odb.clear_delta_base_cache();

    if let Some(dropped) = dropped_root_trees.filter(|d| !d.is_empty()) {
        walk.ordered.retain(|oid| !dropped.contains(oid));
    }
    let mut send = walk.ordered;
    if let Some(peer) = peer_odb {
        if !peer.uses_files_primary() && !peer.custom_primary_is_empty()? {
            send = peer.missing_materialized_many(&send)?;
        }
    }
    Ok(WalkedObjects {
        send,
        name_hashes: walk.name_hashes,
        edges: walk.edges,
        missing: walk.missing.unwrap_or_default(),
    })
}

/// Result of [`collect_reachable_excluding_with_name_hashes`].
struct WalkedObjects {
    /// Objects to send, in pack order.
    send: Vec<ObjectId>,
    /// Pack name hash of each named object.
    name_hashes: HashMap<ObjectId, u32>,
    /// Have-side commits the walk stopped at.
    edges: HashSet<ObjectId>,
    /// Blobs left out because they are not stored ([`MissingBlobMode::Probe`] only).
    missing: Vec<MissingBlob>,
}

/// An object waiting in [`EnumerationWalk::queue`].
struct QueuedObject {
    oid: ObjectId,
    /// Pack name hash of the path the object was found at; `None` for a root tree or
    /// an object reached without a path (a root or a tag target).
    path_hash: Option<u32>,
    /// Tree depth, for depth-sensitive filters.
    depth: u64,
    /// Named as a tree by a commit or a tree entry, so its kind is not looked up
    /// before it is read.
    found_as_tree: bool,
}

impl ReadAheadEntry for QueuedObject {
    fn read_ahead_oid(&self) -> Option<ObjectId> {
        self.found_as_tree.then_some(self.oid)
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
    /// Objects to read. Drained only while [`Self::commits`] is empty.
    queue: VecDeque<QueuedObject>,
    name_hashes: HashMap<ObjectId, u32>,
    /// Have-side commits the walk stopped at (Git's edge commits).
    edges: HashSet<ObjectId>,
    /// With [`MissingBlobMode::Probe`], blobs that would be listed but are not stored.
    missing: Option<Vec<MissingBlob>>,
    /// Reader over the single local pack, kept for the whole walk instead of the one
    /// [`Odb::read`] opens per object (see [`Odb::open_session_pack_reader`]).
    session_pack: Option<SingleLocalPackReader>,
    /// Without a filter, the last tree expanded at each path, keyed by the path's
    /// name hash (see [`ExpandedTree`]).
    expanded: Option<HashMap<Option<u32>, ExpandedTree>>,
}

/// A tree whose every entry the walk has pushed.
///
/// Without a filter, pushing an entry always leaves the object it names in
/// [`EnumerationWalk::visited`], so an entry that another tree repeats byte for byte
/// has nothing left to do. Successive versions of one directory usually differ in a
/// few entries: a new version is compared with the last one expanded at its path, and
/// only the entries between their shared leading and trailing bytes are parsed. Of
/// those, an entry the earlier version also has, byte for byte, is skipped as well.
///
/// Which tree a version is compared with only decides how much is skipped, never what
/// is listed, so paths whose name hashes collide may share an entry.
struct ExpandedTree {
    data: Vec<u8>,
    /// Byte offset of every entry, ascending, then `data.len()`.
    entry_starts: Vec<usize>,
}

impl ExpandedTree {
    /// Entry offsets from `pos` on, when `pos` is where an entry starts.
    fn starts_from(&self, pos: usize) -> Option<&[usize]> {
        let at = self.entry_starts.binary_search(&pos).ok()?;
        Some(&self.entry_starts[at..])
    }

    /// The raw bytes of entry `index`, if there is one.
    fn entry(&self, index: usize) -> Option<&[u8]> {
        let start = *self.entry_starts.get(index)?;
        let end = *self.entry_starts.get(index + 1)?;
        self.data.get(start..end)
    }

    /// Whether `bytes`, a whole entry named `name` of a newer version, is also an entry
    /// here, looked for from entry `*cursor` on.
    ///
    /// Both versions list entries in tree order, so the cursor only moves forward: past
    /// entries whose names sort before `name`, and past the one entry matched. Names
    /// compare as plain bytes, which can differ from tree order where a directory name
    /// is a prefix of another name; that only costs a missed match, since a match
    /// always means equal bytes.
    fn repeats_entry(&self, cursor: &mut usize, bytes: &[u8], name: &[u8], oid_len: usize) -> bool {
        while let Some(old) = self.entry(*cursor) {
            if old == bytes {
                *cursor += 1;
                return true;
            }
            let old_name = memchr::memchr(b' ', old)
                .and_then(|space| old.get(space + 1..old.len().checked_sub(oid_len + 1)?));
            match old_name.map(|old_name| old_name.cmp(name)) {
                Some(std::cmp::Ordering::Less) => *cursor += 1,
                Some(std::cmp::Ordering::Equal) => {
                    *cursor += 1;
                    return false;
                }
                _ => return false,
            }
        }
        false
    }
}

/// Length of the longest common prefix of `a` and `b`.
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let (a_chunks, _) = a.as_chunks::<16>();
    let (b_chunks, _) = b.as_chunks::<16>();
    let whole = a_chunks
        .iter()
        .zip(b_chunks)
        .take_while(|(x, y)| x == y)
        .count()
        * 16;
    whole
        + a[whole..]
            .iter()
            .zip(&b[whole..])
            .take_while(|(x, y)| x == y)
            .count()
}

/// Length of the longest common suffix of `a` and `b`.
fn common_suffix_len(a: &[u8], b: &[u8]) -> usize {
    let (_, a_chunks) = a.as_rchunks::<16>();
    let (_, b_chunks) = b.as_rchunks::<16>();
    let whole = a_chunks
        .iter()
        .rev()
        .zip(b_chunks.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
        * 16;
    whole
        + a[..a.len() - whole]
            .iter()
            .rev()
            .zip(b[..b.len() - whole].iter().rev())
            .take_while(|(x, y)| x == y)
            .count()
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
        if self.boundary.contains(&oid) {
            return true;
        }
        self.peer_odb.is_some_and(|peer| {
            peer.uses_files_primary() && peer.exists_materialized_local_without_rescan(&oid)
        })
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
    fn push_object(&mut self, oid: ObjectId, path_hash: Option<u32>, found_as_tree: bool) {
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
            self.queue.push_back(QueuedObject {
                oid,
                path_hash,
                depth: 0,
                found_as_tree,
            });
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
        // Only a commit is read now; anything else (a blob want, say) is read, if at
        // all, when the queue reaches it.
        match self.odb.read_info(&oid) {
            Ok(info) if info.kind != ObjectKind::Commit => {
                self.queue_unnamed(oid, info.kind == ObjectKind::Tree);
                return Ok(());
            }
            Ok(_) => {}
            Err(_) if self.skip_missing => return Ok(()),
            Err(e) => return Err(e),
        }
        match self.read(&oid)? {
            Some(obj) if obj.kind == ObjectKind::Commit => self.queue_commit(oid, obj),
            Some(obj) => {
                self.queue_unnamed(oid, obj.kind == ObjectKind::Tree);
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
            Some(obj) => {
                self.queue_unnamed(oid, obj.kind == ObjectKind::Tree);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Queue an object reached without a path (a root, or a parent that is not a commit).
    fn queue_unnamed(&mut self, oid: ObjectId, found_as_tree: bool) {
        self.queue.push_back(QueuedObject {
            oid,
            path_hash: None,
            depth: 0,
            found_as_tree,
        });
    }

    /// Expand an object read from [`Self::queue`]: queue a commit, list and queue the
    /// entries of a tree, or follow a tag.
    fn expand(
        &mut self,
        oid: ObjectId,
        obj: Object,
        path_hash: Option<u32>,
        depth: u64,
        oid_len: usize,
    ) -> Result<()> {
        match obj.kind {
            ObjectKind::Commit => {
                self.queue_commit(oid, obj)?;
            }
            ObjectKind::Tree => self.expand_tree(oid, obj.data, path_hash, depth, oid_len)?,
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                self.push_object(tag.object, path_hash, false);
            }
            ObjectKind::Blob => {}
        }
        Ok(())
    }

    /// Push every entry of tree `oid` (payload `data`, found at `path_hash` and `depth`).
    fn expand_tree(
        &mut self,
        oid: ObjectId,
        data: Vec<u8>,
        path_hash: Option<u32>,
        depth: u64,
        oid_len: usize,
    ) -> Result<()> {
        let prev = self
            .expanded
            .as_mut()
            .and_then(|by_path| by_path.remove(&path_hash));
        let mut entry_starts = Vec::new();
        let mut pos = 0;
        // Entries before `pos` are byte-identical to `prev`'s, and from `shared_tail`
        // on the two trees end in the same bytes.
        let mut shared_tail = data.len();
        // Index in `prev.entry_starts` of the earlier version's next unmatched entry.
        let mut prev_entry = 0;
        if let Some(prev) = &prev {
            let prefix = common_prefix_len(&data, &prev.data);
            let shared = prev.entry_starts.partition_point(|&start| start <= prefix);
            if let Some(&last) = shared.checked_sub(1).and_then(|i| prev.entry_starts.get(i)) {
                pos = last;
                prev_entry = shared - 1;
                entry_starts.extend_from_slice(&prev.entry_starts[..shared - 1]);
            }
            shared_tail = data.len() - common_suffix_len(&data[pos..], &prev.data[pos..]);
        }
        loop {
            let at = pos;
            if let Some(prev) = prev.as_ref().filter(|_| at >= shared_tail) {
                // `at` is an entry start here; when it is one in `prev` as well, both
                // trees parse the same from there to the end.
                let prev_at = at + prev.data.len() - data.len();
                if let Some(rest) = prev.starts_from(prev_at) {
                    entry_starts.extend(rest.iter().map(|&s| s + data.len() - prev.data.len()));
                    break;
                }
            }
            entry_starts.push(at);
            if at >= data.len() {
                break;
            }
            let entry = match tree_entry_span(&data, at, oid_len) {
                Some(span) => {
                    pos = span.end;
                    if let Some(prev) = &prev {
                        let name = &data[span.name.clone()];
                        if prev.repeats_entry(&mut prev_entry, &data[at..pos], name, oid_len) {
                            continue;
                        }
                    }
                    TreeEntryRef {
                        mode: span.mode,
                        name: &data[span.name],
                        oid: ObjectId::from_bytes(&data[pos - oid_len..pos])?,
                    }
                }
                None => {
                    let mut entries = TreeRefIter::with_oid_len_at(&data, oid_len, at);
                    let Some(entry) = entries.next().transpose()? else {
                        break;
                    };
                    pos = entries.position();
                    entry
                }
            };
            self.push_tree_entry(entry, oid, path_hash, depth)?;
        }
        if let Some(by_path) = self.expanded.as_mut() {
            by_path.insert(path_hash, ExpandedTree { data, entry_starts });
        }
        Ok(())
    }

    fn push_tree_entry(
        &mut self,
        entry: TreeEntryRef<'_>,
        tree: ObjectId,
        path_hash: Option<u32>,
        depth: u64,
    ) -> Result<()> {
        if entry.mode == 0o160000 {
            return Ok(());
        }
        // Only a filter leaves objects unlisted, so without one an object already
        // visited has nothing left to record.
        if self.filter.is_none() && self.visited.contains(&entry.oid) {
            return Ok(());
        }
        // Regular files and symlinks have no children: record them without inflating
        // their contents. The pack writer reads (or copies verbatim) each one later
        // and fails there if it is missing. Any other mode, including directories
        // written with non-canonical permission bits, is read so the object's own
        // kind decides whether it is walked.
        match entry.mode & 0o170000 {
            0o120000 => self.push_blob(entry.oid, None, tree, depth)?,
            0o100000 => {
                let name = pack_name_hash_child(path_hash, entry.name);
                self.push_blob(entry.oid, Some(name), tree, depth)?;
            }
            file_type => {
                let name = Some(pack_name_hash_child(path_hash, entry.name));
                if file_type == 0o040000 {
                    self.push_tree(entry.oid, name, depth + 1);
                } else {
                    self.push_object(entry.oid, name, false);
                }
            }
        }
        Ok(())
    }

    /// Read `oid`, preferring the walk's pack reader; a miss or error there falls back
    /// to the object database, which reports the authoritative result.
    fn read_object(&mut self, oid: &ObjectId) -> Result<Object> {
        if let Some(Ok(obj)) = self.session_pack.as_mut().map(|r| r.read_object(oid)) {
            return Ok(obj);
        }
        self.odb.read_once(oid)
    }

    fn read(&mut self, oid: &ObjectId) -> Result<Option<Object>> {
        match self.read_object(oid) {
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
    /// `path_hash` is the pack name hash of the tree's path, `None` for a root tree.
    fn push_tree(&mut self, oid: ObjectId, path_hash: Option<u32>, depth: u64) {
        if let Some(hash) = path_hash.filter(|_| self.name_trees == NameTrees::Yes) {
            if !self.visited.contains(&oid) {
                self.name_hashes.entry(oid).or_insert(hash);
            }
        }
        let Some(filter) = self.filter else {
            self.push_object(oid, path_hash, true);
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
            self.queue.push_back(QueuedObject {
                oid,
                path_hash,
                depth,
                found_as_tree: true,
            });
        }
    }

    /// A blob in a tree at `tree_depth`; `name_hash` is the pack name hash of its path,
    /// given for regular files only.
    fn push_blob(
        &mut self,
        oid: ObjectId,
        name_hash: Option<u32>,
        referrer: ObjectId,
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
            // Missing (probe mode only): as in Git's filters, an object whose size
            // cannot be read is not filtered out but reported missing.
            Some(filter) => self
                .blob_passes(filter, &oid, tree_depth)?
                .unwrap_or((true, true)),
        };
        if passes && self.missing.is_some() && !self.odb.contains(&oid)? {
            self.visited.insert(oid);
            if let Some(missing) = self.missing.as_mut() {
                missing.push(MissingBlob {
                    oid,
                    referrer: Some(referrer),
                });
            }
            return Ok(());
        }
        if passes {
            self.visited.insert(oid);
            self.list(oid);
            if let Some(hash) = name_hash {
                self.name_hashes.insert(oid, hash);
            }
        } else if size_checked && !self.depth_sensitive {
            self.visited.insert(oid);
            self.unlisted.insert(oid);
        }
        Ok(())
    }

    /// Whether the blob passes `filter`, and whether its size had to be looked up.
    ///
    /// `None` when the size was needed but the blob is not stored, which only
    /// happens with [`MissingBlobMode::Probe`] (the size is read without a lazy
    /// fetch there); otherwise a missing blob is an error, or with `skip_missing`
    /// filtered out.
    fn blob_passes(
        &self,
        filter: &ObjectFilter,
        oid: &ObjectId,
        tree_depth: u64,
    ) -> Result<Option<(bool, bool)>> {
        // A smaller blob never fares worse, so the size only matters between the extremes.
        if !filter.includes_blob_under_tree(0, tree_depth) {
            return Ok(Some((false, false)));
        }
        if filter.includes_blob_under_tree(u64::MAX, tree_depth) {
            return Ok(Some((true, false)));
        }
        let info = if self.missing.is_some() {
            match self.odb.read_info_without_fetch(oid) {
                Err(crate::error::Error::ObjectNotFound(_)) => return Ok(None),
                other => other,
            }
        } else {
            self.odb.read_info(oid)
        };
        match info {
            Ok(info) => Ok(Some((
                filter.includes_blob_under_tree(info.size, tree_depth),
                true,
            ))),
            Err(_) if self.skip_missing => Ok(Some((false, true))),
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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
    fn tree_read_ahead_leaves_the_walk_output_unchanged() {
        let dir = TempDir::new().unwrap();
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let blob = |s: &str| repo.odb().write(ObjectKind::Blob, s.as_bytes()).unwrap();
        let mut tip: Option<ObjectId> = None;
        // Two trees per commit, so the queue holds far more than one read-ahead batch.
        for i in 0..400i64 {
            let sub = write_tree(&repo, &[(0o100644, "f", blob(&format!("sub {i}")))]);
            let tree = write_tree(
                &repo,
                &[
                    (0o040000, "dir", sub),
                    (0o100644, "top", blob(&format!("top {}", i / 3))),
                ],
            );
            tip = Some(write_commit(&repo, tree, tip.as_slice(), 1_000 + i));
        }
        let tip = tip.unwrap();
        let config_path = repo.git_dir().join("config");
        let config = std::fs::read_to_string(&config_path).unwrap();
        let walk_with_threads = |threads: u32| {
            std::fs::write(
                &config_path,
                format!("{config}[pack]\n\tthreads = {threads}\n"),
            )
            .unwrap();
            let repo = Repository::open(repo.git_dir(), &RepositoryOptions::empty()).unwrap();
            walk(&repo, &[tip], NameTrees::Yes)
        };
        let serial = walk_with_threads(1);
        let parallel = walk_with_threads(4);
        assert_eq!(serial.send.len(), 400 * 3 + 400 + 134);
        assert_eq!(parallel.send, serial.send);
        assert_eq!(parallel.name_hashes, serial.name_hashes);
    }

    #[test]
    fn send_pack_walk_without_haves_reads_no_commits() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};
        let dir = TempDir::new().unwrap();
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let tree = write_tree(&repo, &[]);
        let mut tip = write_commit(&repo, tree, &[], 1_300_000_000);
        for i in 1..50 {
            tip = write_commit(&repo, tree, &[tip], 1_300_000_000 + i);
        }

        reset_commit_reads();
        let hidden = send_pack_uninteresting_commits(
            repo.odb(),
            &[tip],
            &[],
            &HashSet::new(),
            &CancelSignal::never(),
        )
        .unwrap();
        assert!(hidden.is_empty());
        assert_eq!(
            commit_reads(),
            0,
            "nothing can be uninteresting without a have"
        );

        let selection = walk(&repo, &[tip], NameTrees::No);
        assert_eq!(
            selection.send.len(),
            51,
            "every commit and the tree are still sent"
        );
    }

    #[test]
    fn push_dated_before_graphed_history_stops_at_the_frontier() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};
        let dir = TempDir::new().unwrap();
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
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
    fn push_dated_before_packed_history_reads_commit_headers() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};
        let dir = TempDir::new().unwrap();
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let tree = write_tree(&repo, &[]);
        let body = "a long message line\n".repeat(100);
        let write = |parents: &[ObjectId], time: i64| {
            let mut text = format!("tree {tree}\n");
            for parent in parents {
                text.push_str(&format!("parent {parent}\n"));
            }
            text.push_str(&format!(
                "author A <a@example.com> {time} +0000\n\
                 committer A <a@example.com> {time} +0000\n\nc{time}\n\n{body}"
            ));
            repo.odb()
                .write(ObjectKind::Commit, text.as_bytes())
                .unwrap()
        };
        // The remote's history, packed and without a commit-graph, with a merge in it.
        let root = write(&[], 1_300_000_000);
        let mut history = vec![root];
        let mut tip = root;
        for i in 1..120 {
            tip = write(&[tip], 1_300_000_000 + i);
            history.push(tip);
        }
        let side = write(&[root], 1_300_000_500);
        tip = write(&[tip, side], 1_300_000_600);
        history.extend([side, tip]);
        // New local work dated before all of that history, so the walk drains it.
        let mut new_tip = tip;
        let mut new_commits = Vec::new();
        for i in 0..3 {
            new_tip = write(&[new_tip], 1_000_000_000 + i);
            new_commits.push(new_tip);
        }
        std::fs::write(
            repo.git_dir().join("refs/heads/main"),
            format!("{new_tip}\n"),
        )
        .unwrap();
        crate::maintenance::repack_all(&repo, &crate::maintenance::RepackOptions::default())
            .unwrap();
        assert!(repo.odb().commit_graph_chain().is_none());
        let mut header = Vec::new();
        assert!(repo
            .odb()
            .read_packed_commit_header_block_into(&tip, &mut header)
            .unwrap());

        reset_commit_reads();
        let hidden = repo
            .odb()
            .with_pack_read_context(|| {
                send_pack_uninteresting_commits(
                    repo.odb(),
                    &[new_tip],
                    &[tip],
                    &HashSet::new(),
                    &CancelSignal::never(),
                )
            })
            .unwrap();
        let expected: HashSet<ObjectId> = history.iter().copied().collect();
        assert_eq!(hidden, expected, "the remote's whole history, nothing new");
        assert!(new_commits.iter().all(|c| !hidden.contains(c)));
        assert_eq!(commit_reads(), history.len() + new_commits.len());
    }

    #[test]
    fn duplicated_wants_already_on_peer_balance_the_frontier() {
        use super::send_pack_walk_test_hooks::{commit_reads, reset_commit_reads};
        let dir = TempDir::new().unwrap();
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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
        Repository::init(tmp.path(), &InitOptions::new().with_bare(true))
            .unwrap()
            .0;
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
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

    #[test]
    fn common_prefix_and_suffix_lengths_cover_chunked_and_tail_bytes() {
        let a: Vec<u8> = (0..100u8).collect();
        assert_eq!(common_prefix_len(&a, &a), 100);
        assert_eq!(common_suffix_len(&a, &a), 100);
        assert_eq!(common_prefix_len(&a, &[]), 0);
        assert_eq!(common_suffix_len(&[], &a), 0);
        for at in [0, 1, 15, 16, 17, 31, 32, 63, 64, 99] {
            let mut b = a.clone();
            b[at] ^= 0xff;
            assert_eq!(common_prefix_len(&a, &b), at, "prefix, mismatch at {at}");
            assert_eq!(
                common_suffix_len(&a, &b),
                99 - at,
                "suffix, mismatch at {at}"
            );
        }
        // Different lengths: the prefix stops at the shorter end, the suffix aligns ends.
        assert_eq!(common_prefix_len(&a[..40], &a), 40);
        assert_eq!(common_suffix_len(&a[37..], &a), 63);
        assert_eq!(common_suffix_len(&a[..50], &a), 0);
    }

    /// Every commit, tree and blob reachable from `tip`, with the paths each blob has as
    /// a regular file.
    fn reachable_with_blob_paths(
        repo: &Repository,
        tip: ObjectId,
    ) -> (HashSet<ObjectId>, HashMap<ObjectId, HashSet<String>>) {
        let mut objects = HashSet::new();
        let mut blob_paths: HashMap<ObjectId, HashSet<String>> = HashMap::new();
        let mut commits = vec![tip];
        let mut trees = Vec::new();
        while let Some(commit) = commits.pop() {
            if !objects.insert(commit) {
                continue;
            }
            let obj = repo.odb().read(&commit).unwrap();
            let parsed = CommitRef::parse(&obj.data).unwrap();
            commits.extend(parsed.parents().iter().copied());
            trees.push((parsed.tree(), String::new()));
        }
        while let Some((tree, path)) = trees.pop() {
            objects.insert(tree);
            let obj = repo.odb().read(&tree).unwrap();
            for entry in TreeRefIter::with_oid_len(&obj.data, 20) {
                let entry = entry.unwrap();
                let child = if path.is_empty() {
                    String::from_utf8(entry.name.to_vec()).unwrap()
                } else {
                    format!("{path}/{}", String::from_utf8_lossy(entry.name))
                };
                match entry.mode {
                    0o160000 => {}
                    0o040000 => trees.push((entry.oid, child)),
                    0o120000 => {
                        objects.insert(entry.oid);
                    }
                    _ => {
                        objects.insert(entry.oid);
                        blob_paths.entry(entry.oid).or_default().insert(child);
                    }
                }
            }
        }
        (objects, blob_paths)
    }

    fn assert_walk_lists_reachable(repo: &Repository, tip: ObjectId) {
        let walked = walk(repo, &[tip], NameTrees::Yes);
        let listed: HashSet<ObjectId> = walked.send.iter().copied().collect();
        assert_eq!(listed.len(), walked.send.len(), "nothing is listed twice");
        let (expected, blob_paths) = reachable_with_blob_paths(repo, tip);
        assert_eq!(listed, expected);
        for (blob, paths) in &blob_paths {
            let hash = walked.name_hashes.get(blob).copied();
            assert!(
                paths.iter().any(|p| Some(pack_name_hash(p)) == hash),
                "blob {blob} is named after one of its paths {paths:?}"
            );
        }
    }

    /// Deterministic xorshift so the edit sequence is the same on every run.
    struct Xorshift(u64);

    impl Xorshift {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    #[test]
    fn walk_lists_every_entry_of_successive_tree_versions() {
        let dir = TempDir::new().unwrap();
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let mut rng = Xorshift(0x2545_f491_4f6c_dd1d);
        let mut next_blob = 0usize;
        let mut new_blob = |repo: &Repository| {
            next_blob += 1;
            repo.odb()
                .write(ObjectKind::Blob, format!("blob {next_blob}\n").as_bytes())
                .unwrap()
        };
        // Names of varying length so edits shift every later entry's offset.
        let mut root: std::collections::BTreeMap<String, ObjectId> = (0..40)
            .map(|i| {
                (
                    format!("f{}", "x".repeat(i % 7) + &i.to_string()),
                    new_blob(&repo),
                )
            })
            .collect();
        let mut sub: std::collections::BTreeMap<String, ObjectId> = (0..12)
            .map(|i| (format!("s{i}"), new_blob(&repo)))
            .collect();
        let gitlink = ObjectId::from_hex("1234567890123456789012345678901234567890").unwrap();
        let link_target = repo.odb().write(ObjectKind::Blob, b"target").unwrap();
        let mut parent: Option<ObjectId> = None;
        for version in 0..300i64 {
            let files = if rng.below(3) == 0 {
                &mut sub
            } else {
                &mut root
            };
            let names: Vec<String> = files.keys().cloned().collect();
            match rng.below(4) {
                0 if names.len() > 2 => {
                    files.remove(&names[rng.below(names.len())]);
                }
                1 => {
                    let name = format!("f{}", "y".repeat(rng.below(5)) + &version.to_string());
                    files.insert(name, new_blob(&repo));
                }
                _ => {
                    let name = names[rng.below(names.len())].clone();
                    // Restoring an earlier blob makes a version repeat older bytes.
                    let blob = if rng.below(4) == 0 {
                        *files.values().next().unwrap()
                    } else {
                        new_blob(&repo)
                    };
                    files.insert(name, blob);
                }
            }
            let sub_entries: Vec<(u32, &str, ObjectId)> = sub
                .iter()
                .map(|(name, &oid)| (0o100644, name.as_str(), oid))
                .collect();
            let sub_tree = write_tree(&repo, &sub_entries);
            let mut entries: Vec<(u32, &str, ObjectId)> = vec![
                (0o160000, "a-module", gitlink),
                (0o120000, "a-link", link_target),
                (0o040000, "d", sub_tree),
            ];
            entries.extend(
                root.iter()
                    .map(|(name, &oid)| (0o100644, name.as_str(), oid)),
            );
            entries.sort_by(|a, b| a.1.cmp(b.1));
            let tree = write_tree(&repo, &entries);
            let parents: Vec<ObjectId> = parent.into_iter().collect();
            parent = Some(write_commit(&repo, tree, &parents, version));
        }
        assert_walk_lists_reachable(&repo, parent.unwrap());
    }

    #[test]
    fn walk_reparses_a_tree_whose_shared_tail_starts_inside_an_entry() {
        let dir = TempDir::new().unwrap();
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let kept = repo.odb().write(ObjectKind::Blob, b"kept").unwrap();
        let older = repo.odb().write(ObjectKind::Blob, b"older").unwrap();
        let newer = repo.odb().write(ObjectKind::Blob, b"newer").unwrap();
        // The newest commit's tree is expanded first. `100644 g\0<kept>` is the whole
        // second entry of the older tree and the tail of the newer tree's entry named
        // `w100644 g`, so the trees share their last bytes from an offset that is no
        // entry start in the tree expanded before.
        let old_tree = write_tree(&repo, &[(0o100644, "a", older), (0o100644, "g", kept)]);
        let new_tree = write_tree(
            &repo,
            &[(0o100644, "a", newer), (0o100644, "w100644 g", kept)],
        );
        let old = write_commit(&repo, old_tree, &[], 1);
        let new = write_commit(&repo, new_tree, &[old], 2);
        assert_walk_lists_reachable(&repo, new);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git; run with --features full-tests"
    )]
    #[test]
    fn walk_matches_git_rev_list_objects_on_edited_history() {
        let dir = TempDir::new().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
        for i in 0..60 {
            let dir_name = ["src", "src/deep", "docs", "."][rng.below(4)];
            std::fs::create_dir_all(dir.path().join(dir_name)).unwrap();
            let file = dir
                .path()
                .join(dir_name)
                .join(format!("f{}", rng.below(15)));
            if rng.below(5) == 0 && file.exists() {
                std::fs::remove_file(&file).unwrap();
            } else {
                std::fs::write(&file, format!("{i}\n")).unwrap();
            }
            git(&["add", "-A"]);
            git(&["commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
        }
        git(&["repack", "-adq"]);
        let expected: HashSet<ObjectId> = git(&["rev-list", "--objects", "--all"])
            .lines()
            .map(|line| ObjectId::from_hex(&line[..40]).unwrap())
            .collect();
        let repo =
            Repository::open(&dir.path().join(".git"), &RepositoryOptions::empty()).expect("open");
        let tip = ObjectId::from_hex(git(&["rev-parse", "HEAD"]).trim()).unwrap();
        let walked = walk(&repo, &[tip], NameTrees::Yes);
        let listed: HashSet<ObjectId> = walked.send.iter().copied().collect();
        assert_eq!(listed.len(), walked.send.len());
        assert_eq!(listed, expected);
        assert_walk_lists_reachable(&repo, tip);
    }
}
