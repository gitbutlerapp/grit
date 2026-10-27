//! Depth-first resolution of a pack's delta trees, shared by index-pack and fsck.
//!
//! A worker inflates a base once, then walks its delta tree applying each delta exactly
//! once. Only the bytes of the chain currently being walked are held: a parent is
//! released as soon as its last child is taken, and a per-worker byte cap evicts
//! ancestors that are rebuilt from the pack if they are needed again. Peak memory
//! therefore follows the deepest chain rather than the number of deltas in the pack.
//!
//! Compressed objects are read through [`PackBytes`]: borrowed from memory, or with
//! positioned reads from the pack file so random access adds no resident pages.
//!
//! What happens to each resolved object is up to a [`DeltaVisitor`]: index-pack hashes
//! it into an index record, fsck checks it against the pack index and inspects it. The
//! visitor sees each object's bytes once and must not keep them.

use crate::error::{Error, Result};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::unpack_objects::{apply_delta_into, type_code_to_kind, PackReader};
use crate::zlib_inflate::ZlibInflateScratch;
use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

/// Soft cap on ancestor bytes one worker keeps for the chain it is walking.
///
/// The base being applied right now is always kept, so a single object larger than
/// the cap still resolves; older ancestors beyond the cap are rebuilt on demand.
pub(crate) const DELTA_CHAIN_BYTES_PER_WORKER: usize = 16 * 1024 * 1024;

/// Where a delta's base lives: a pack offset (ofs-delta) or an object id (ref-delta).
#[derive(Clone, Copy, Debug)]
pub(crate) enum DeltaBase {
    /// Absolute offset of the base's header in the same pack.
    Offset(usize),
    /// Object id of the base, in this pack or in the object database.
    Oid(ObjectId),
}

/// One delta in the pack, located but not yet inflated; its base is recorded in
/// [`DeltaChildren`].
///
/// Kept small because a large pack holds hundreds of thousands of these at once.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingDelta {
    /// Offset of the delta's object header.
    pub offset: usize,
    /// End of the delta's zlib stream (or any later bound inside the pack).
    pub pack_end: usize,
    /// Inflated size of the delta instructions.
    pub uncompressed_size: usize,
    /// Bytes from `offset` to the zlib stream (object header plus base reference).
    pub header_len: u8,
}

impl PendingDelta {
    /// Describe the delta whose header is at `offset` and whose zlib stream spans
    /// `zlib_offset..pack_end`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when the header is longer than 255 bytes.
    pub(crate) fn new(
        offset: usize,
        zlib_offset: usize,
        pack_end: usize,
        uncompressed_size: usize,
    ) -> Result<Self> {
        let header_len = u8::try_from(zlib_offset.saturating_sub(offset))
            .map_err(|_| Error::CorruptObject("pack object header too long".to_owned()))?;
        Ok(Self {
            offset,
            pack_end,
            uncompressed_size,
            header_len,
        })
    }

    fn zlib_offset(&self) -> usize {
        self.offset + usize::from(self.header_len)
    }
}

/// Where delta resolution reads compressed objects from.
#[derive(Clone, Copy)]
pub(crate) enum PackBytes<'a> {
    /// The pack bytes in memory; reads borrow from them.
    Slice(&'a [u8]),
    /// The pack file, read with positioned reads so random access adds no resident pages.
    File(&'a std::fs::File),
}

impl PackBytes<'_> {
    /// Bytes `start..end` of the pack, borrowed or read into `buf`.
    fn read<'s>(&'s self, start: usize, end: usize, buf: &'s mut Vec<u8>) -> Result<&'s [u8]> {
        match self {
            Self::Slice(pack) => pack
                .get(start..end)
                .ok_or_else(|| Error::CorruptObject("pack object out of range".to_owned())),
            Self::File(file) => {
                buf.clear();
                buf.resize(end.saturating_sub(start), 0);
                read_exact_at(file, buf, start as u64).map_err(Error::Io)?;
                Ok(buf)
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn read_exact_at(
    file: &std::fs::File,
    buf: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
pub(crate) fn read_exact_at(
    file: &std::fs::File,
    mut buf: &mut [u8],
    mut offset: u64,
) -> std::io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn read_exact_at(
    _file: &std::fs::File,
    _buf: &mut [u8],
    _offset: u64,
) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// A base object whose delta tree one worker resolves depth-first.
#[derive(Clone, Copy, Debug)]
pub(crate) enum DeltaRoot {
    /// A whole object in this pack: header at `offset`, zlib stream ending by `end`.
    Pack {
        /// Offset of the object header.
        offset: usize,
        /// End of the object's zlib stream (or any later bound inside the pack).
        end: usize,
        /// Object id its ref-delta children name.
        oid: ObjectId,
    },
    /// A ref-delta base that only exists in the object database (thin packs, or a base
    /// stored in another pack).
    Odb(ObjectId),
}

/// Decides what happens to each object a [`DeltaWalk`] produces.
///
/// Methods run concurrently on worker threads; each worker appends to its own
/// [`Self::Out`] and hands it over in batches with [`Self::merge`]. Returning an error
/// stops the whole walk and [`DeltaWalk::run`] returns the first one.
pub(crate) trait DeltaVisitor: Sync {
    /// Per-worker results.
    type Out: Default + Send;

    /// A root's bytes were loaded. Return `false` to leave its delta children unresolved.
    fn root(
        &self,
        root: &DeltaRoot,
        kind: ObjectKind,
        data: &[u8],
        out: &mut Self::Out,
    ) -> Result<bool>;

    /// A [`DeltaRoot::Pack`] root could not be inflated; its children stay unresolved.
    fn root_failed(&self, root: &DeltaRoot, err: Error, out: &mut Self::Out) -> Result<()> {
        let _ = (root, out);
        Err(err)
    }

    /// The delta at `index` resolved to `data`. Return the object id its ref-delta
    /// children name to keep walking, or `None` to leave its children unresolved.
    fn delta(
        &self,
        index: u32,
        kind: ObjectKind,
        data: &[u8],
        out: &mut Self::Out,
    ) -> Result<Option<ObjectId>>;

    /// The delta at `index` could not be applied; its children stay unresolved.
    fn delta_failed(&self, index: u32, err: Error, out: &mut Self::Out) -> Result<()> {
        let _ = (index, out);
        Err(err)
    }

    /// Number of results buffered in `out`, used to hand them over in batches.
    fn len(out: &Self::Out) -> usize;

    /// Move every result in `from` to the end of `into`, leaving `from` empty.
    fn merge(into: &mut Self::Out, from: &mut Self::Out);
}

/// Delta indices grouped by base, sorted (by [`Self::sort`]) so each base's children
/// form one slice.
#[derive(Default)]
pub(crate) struct DeltaChildren {
    by_offset: Vec<(usize, u32)>,
    by_oid: Vec<(ObjectId, u32)>,
}

impl DeltaChildren {
    /// Record that the delta at `index` is built on `base`.
    pub(crate) fn push(&mut self, base: DeltaBase, index: u32) {
        match base {
            DeltaBase::Offset(offset) => self.by_offset.push((offset, index)),
            DeltaBase::Oid(oid) => self.by_oid.push((oid, index)),
        }
    }

    /// Group children by base; call once after the last [`Self::push`].
    pub(crate) fn sort(&mut self) {
        self.by_offset.sort_unstable();
        self.by_oid.sort_unstable();
    }

    fn of_offset(&self, offset: usize) -> &[(usize, u32)] {
        let start = self.by_offset.partition_point(|(base, _)| *base < offset);
        let len = self.by_offset[start..].partition_point(|(base, _)| *base == offset);
        &self.by_offset[start..start + len]
    }

    fn of_oid(&self, oid: &ObjectId) -> &[(ObjectId, u32)] {
        let start = self.by_oid.partition_point(|(base, _)| base < oid);
        let len = self.by_oid[start..].partition_point(|(base, _)| base == oid);
        &self.by_oid[start..start + len]
    }

    /// True when some delta names the object at `offset` (or with id `oid`) as its base.
    pub(crate) fn has_children(&self, offset: usize, oid: &ObjectId) -> bool {
        !self.of_offset(offset).is_empty() || !self.of_oid(oid).is_empty()
    }
}

/// How to rebuild a chain frame whose bytes were evicted.
#[derive(Clone, Copy)]
enum Anchor {
    /// Start from the walk's root object.
    Root,
    /// Start from the frame at this stack index.
    Frame(usize),
}

/// One object on the chain a worker is walking, with the children still to resolve.
struct ChainFrame<'c> {
    kind: ObjectKind,
    data: Option<Vec<u8>>,
    ofs_children: &'c [(usize, u32)],
    ref_children: &'c [(ObjectId, u32)],
    next_child: usize,
    anchor: Anchor,
    /// Deltas applied, in order, to the anchor's bytes to rebuild this frame. Parents
    /// released after their last child leave their delta here instead of a stack slot.
    via: Vec<u32>,
}

impl ChainFrame<'_> {
    fn take_next_child(&mut self) -> Option<u32> {
        let k = self.next_child;
        let child = match self.ofs_children.get(k) {
            Some(&(_, child)) => child,
            None => self.ref_children.get(k - self.ofs_children.len())?.1,
        };
        self.next_child += 1;
        Some(child)
    }

    fn is_exhausted(&self) -> bool {
        self.next_child >= self.ofs_children.len() + self.ref_children.len()
    }

    fn held_bytes(&self) -> usize {
        self.data.as_ref().map_or(0, Vec::capacity)
    }
}

/// Buffers one worker reuses across objects.
#[derive(Default)]
struct DeltaScratch {
    zlib: ZlibInflateScratch,
    /// Compressed bytes of the object being inflated (file reads only).
    packed: Vec<u8>,
    delta: Vec<u8>,
    leaf: Vec<u8>,
}

/// A pack's deltas, ready to be resolved from roots chosen by the caller.
///
/// Every delta is claimed by at most one worker, so a base duplicated in the pack (or
/// reached again as a thin base) never resolves the same delta twice.
pub(crate) struct DeltaWalk<'a, V> {
    source: PackBytes<'a>,
    odb: &'a Odb,
    pending: &'a [PendingDelta],
    children: &'a DeltaChildren,
    claimed: Vec<AtomicBool>,
    resolved: AtomicU64,
    failed: AtomicBool,
    chain_limit: usize,
    visitor: &'a V,
}

impl<'a, V: DeltaVisitor> DeltaWalk<'a, V> {
    /// Prepare to resolve `pending`, whose bases are recorded in the sorted `children`.
    ///
    /// Compressed bytes come from `source`, [`DeltaRoot::Odb`] bases from `odb`.
    /// `chain_limit` caps the ancestor bytes a worker keeps (see
    /// [`DELTA_CHAIN_BYTES_PER_WORKER`]).
    pub(crate) fn new(
        source: PackBytes<'a>,
        odb: &'a Odb,
        pending: &'a [PendingDelta],
        children: &'a DeltaChildren,
        chain_limit: usize,
        visitor: &'a V,
    ) -> Self {
        Self {
            source,
            odb,
            pending,
            children,
            claimed: pending.iter().map(|_| AtomicBool::new(false)).collect(),
            resolved: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            chain_limit,
            visitor,
        }
    }

    /// Number of deltas resolved so far (the visitor accepted the result).
    pub(crate) fn resolved(&self) -> u64 {
        self.resolved.load(Ordering::Relaxed)
    }

    /// One [`DeltaRoot::Odb`] root per ref-delta base that still has unclaimed children,
    /// sorted by object id.
    pub(crate) fn thin_roots(&self) -> Vec<DeltaRoot> {
        self.children
            .by_oid
            .chunk_by(|a, b| a.0 == b.0)
            .filter(|group| {
                group
                    .iter()
                    .any(|(_, child)| !self.claimed[*child as usize].load(Ordering::Relaxed))
            })
            .map(|group| DeltaRoot::Odb(group[0].0))
            .collect()
    }

    /// Indices of deltas no walk has reached (their base never resolved).
    pub(crate) fn unclaimed(&self) -> impl Iterator<Item = u32> + '_ {
        self.claimed
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.load(Ordering::Relaxed))
            .filter_map(|(i, _)| u32::try_from(i).ok())
    }

    /// Walk `roots` on up to `threads` workers, the caller thread included.
    ///
    /// Results are appended to `out`; workers hand theirs over in small batches, so
    /// per-thread buffers stay bounded instead of each growing to its share of the
    /// pack. `after_root` runs on the caller thread after each root it walks, with the
    /// number of deltas resolved so far.
    ///
    /// # Errors
    ///
    /// Returns the first error a visitor returned (which stops every worker), or
    /// [`Error::CorruptObject`] if a worker thread panicked.
    pub(crate) fn run(
        &self,
        roots: &[DeltaRoot],
        threads: NonZeroUsize,
        out: &mut V::Out,
        after_root: &mut dyn FnMut(u64),
    ) -> Result<()> {
        if roots.is_empty() {
            return Ok(());
        }
        const BATCH: usize = 1024;
        let next_root = AtomicUsize::new(0);
        let first_error: Mutex<Option<Error>> = Mutex::new(None);
        let shared = Mutex::new(std::mem::take(out));
        let flush = |batch: &mut V::Out| {
            V::merge(&mut shared.lock().unwrap_or_else(|e| e.into_inner()), batch);
        };
        let helpers = threads.get().min(roots.len()).saturating_sub(1);
        let work = |after_root: &mut dyn FnMut(u64)| {
            let mut scratch = DeltaScratch::default();
            let mut batch = V::Out::default();
            while !self.failed.load(Ordering::Relaxed) {
                let Some(root) = roots.get(next_root.fetch_add(1, Ordering::Relaxed)) else {
                    break;
                };
                if let Err(err) = self.resolve_root(root, &mut scratch, &mut batch) {
                    record_first_error(&first_error, err);
                    self.failed.store(true, Ordering::Relaxed);
                }
                if V::len(&batch) >= BATCH {
                    flush(&mut batch);
                }
                after_root(self.resolved());
            }
            flush(&mut batch);
        };
        thread::scope(|scope| {
            let work = &work;
            let handles: Vec<_> = (0..helpers)
                .map(|_| scope.spawn(move || work(&mut |_| {})))
                .collect();
            work(after_root);
            for handle in handles {
                if handle.join().is_err() {
                    record_first_error(
                        &first_error,
                        Error::CorruptObject("delta worker stopped".to_owned()),
                    );
                }
            }
        });
        *out = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        after_root(self.resolved());
        match first_error.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Resolve every delta reachable from `root`, depth-first.
    fn resolve_root(
        &self,
        root: &DeltaRoot,
        scratch: &mut DeltaScratch,
        out: &mut V::Out,
    ) -> Result<()> {
        let (kind, data) = match self.load_root(root, &mut scratch.zlib, &mut scratch.packed) {
            Ok(Some(loaded)) => loaded,
            Ok(None) => return Ok(()),
            Err(err) => return self.visitor.root_failed(root, err, out),
        };
        if !self.visitor.root(root, kind, &data, out)? {
            return Ok(());
        }
        let (ofs_children, ref_children) = match root {
            DeltaRoot::Pack { offset, oid, .. } => {
                (self.children.of_offset(*offset), self.children.of_oid(oid))
            }
            DeltaRoot::Odb(oid) => (&[][..], self.children.of_oid(oid)),
        };
        if ofs_children.is_empty() && ref_children.is_empty() {
            return Ok(());
        }
        let mut held = data.capacity();
        let mut stack = vec![ChainFrame {
            kind,
            data: Some(data),
            ofs_children,
            ref_children,
            next_child: 0,
            anchor: Anchor::Root,
            via: Vec::new(),
        }];
        while let Some(top) = stack.len().checked_sub(1) {
            if self.failed.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Some(child) = stack[top].take_next_child() else {
                if let Some(done) = stack.pop() {
                    held = held.saturating_sub(done.held_bytes());
                }
                continue;
            };
            if self.claimed[child as usize].swap(true, Ordering::AcqRel) {
                continue;
            }
            if stack[top].data.is_none() {
                match self.rebuild(&stack, top, root, scratch) {
                    Ok(data) => {
                        held += data.capacity();
                        stack[top].data = Some(data);
                    }
                    Err(err) => {
                        self.visitor.delta_failed(child, err, out)?;
                        continue;
                    }
                }
            }
            let delta = &self.pending[child as usize];
            let kind = stack[top].kind;
            let ofs_children = self.children.of_offset(delta.offset);
            // Results that become bases get their own exact allocation; leaves reuse one buffer.
            let mut result = if ofs_children.is_empty() {
                std::mem::take(&mut scratch.leaf)
            } else {
                Vec::new()
            };
            let base = stack[top].data.as_deref().unwrap_or_default();
            let applied = self.apply(
                delta,
                base,
                &mut scratch.zlib,
                &mut scratch.packed,
                &mut scratch.delta,
                &mut result,
            );
            let next = match applied {
                Ok(()) => self.visitor.delta(child, kind, &result, out)?,
                Err(err) => {
                    self.visitor.delta_failed(child, err, out)?;
                    None
                }
            };
            let Some(oid) = next else {
                if ofs_children.is_empty() {
                    scratch.leaf = result;
                }
                continue;
            };
            self.resolved.fetch_add(1, Ordering::Relaxed);
            let ref_children = self.children.of_oid(&oid);
            if ofs_children.is_empty() && ref_children.is_empty() {
                scratch.leaf = result;
                continue;
            }
            result.shrink_to_fit();
            let exhausted = stack[top].is_exhausted();
            let (anchor, via) = match exhausted.then(|| stack.pop()).flatten() {
                Some(parent) => {
                    held = held.saturating_sub(parent.held_bytes());
                    let mut via = parent.via;
                    via.push(child);
                    (parent.anchor, via)
                }
                None => (Anchor::Frame(top), vec![child]),
            };
            held += result.capacity();
            stack.push(ChainFrame {
                kind,
                data: Some(result),
                ofs_children,
                ref_children,
                next_child: 0,
                anchor,
                via,
            });
            held = evict_ancestors(&mut stack, held, self.chain_limit);
        }
        Ok(())
    }

    /// Bytes and kind of `root`, or `None` for an object database base that is absent.
    fn load_root(
        &self,
        root: &DeltaRoot,
        zlib: &mut ZlibInflateScratch,
        packed: &mut Vec<u8>,
    ) -> Result<Option<(ObjectKind, Vec<u8>)>> {
        match root {
            DeltaRoot::Pack { offset, end, .. } => {
                let bytes = self.source.read(*offset, *end, packed)?;
                let mut rd = PackReader::new(bytes);
                let (type_code, size) = rd.read_type_size()?;
                let kind = type_code_to_kind(type_code)?;
                let mut data = Vec::new();
                let size = u64::try_from(size).unwrap_or(u64::MAX);
                zlib.decompress_fixed_into(bytes, &mut rd.pos, size, &mut data)?;
                Ok(Some((kind, data)))
            }
            DeltaRoot::Odb(oid) => Ok(self.odb.read(oid).ok().map(|obj| (obj.kind, obj.data))),
        }
    }

    /// Recompute the bytes of the evicted frame at `index` from its anchor.
    fn rebuild(
        &self,
        stack: &[ChainFrame<'_>],
        index: usize,
        root: &DeltaRoot,
        scratch: &mut DeltaScratch,
    ) -> Result<Vec<u8>> {
        let frame = &stack[index];
        let mut current: Cow<'_, [u8]> = match frame.anchor {
            Anchor::Root => match self.load_root(root, &mut scratch.zlib, &mut scratch.packed)? {
                Some((_, data)) => Cow::Owned(data),
                None => {
                    return Err(Error::CorruptObject("delta base not available".to_owned()));
                }
            },
            Anchor::Frame(parent) => match stack[parent].data.as_deref() {
                Some(data) => Cow::Borrowed(data),
                None => Cow::Owned(self.rebuild(stack, parent, root, scratch)?),
            },
        };
        for &step in &frame.via {
            let mut out = Vec::new();
            self.apply(
                &self.pending[step as usize],
                &current,
                &mut scratch.zlib,
                &mut scratch.packed,
                &mut scratch.delta,
                &mut out,
            )?;
            current = Cow::Owned(out);
        }
        Ok(current.into_owned())
    }

    fn apply(
        &self,
        delta: &PendingDelta,
        base: &[u8],
        zlib: &mut ZlibInflateScratch,
        packed: &mut Vec<u8>,
        delta_buf: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let bytes = self
            .source
            .read(delta.zlib_offset(), delta.pack_end, packed)?;
        let size = u64::try_from(delta.uncompressed_size).unwrap_or(u64::MAX);
        zlib.decompress_fixed_into(bytes, &mut 0, size, delta_buf)?;
        apply_delta_into(out, base, delta_buf)
    }
}

/// Keep `err` in `first_error` unless an earlier error is already there.
pub(crate) fn record_first_error(first_error: &Mutex<Option<Error>>, err: Error) {
    let mut slot = first_error.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(err);
    }
}

/// Drop the bytes of the oldest ancestors until the chain fits in `limit`; the frame on
/// top is always kept. Returns the bytes still held.
fn evict_ancestors(stack: &mut [ChainFrame<'_>], mut held: usize, limit: usize) -> usize {
    let Some(keep) = stack.len().checked_sub(1) else {
        return held;
    };
    for frame in &mut stack[..keep] {
        if held <= limit {
            break;
        }
        if let Some(data) = frame.data.take() {
            held = held.saturating_sub(data.capacity());
        }
    }
    held
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hash::hash_object;
    use crate::objects::HashAlgo;
    use grit_test_support::objects::{
        DeltaOps, HashAlgo as FixtureAlgo, ObjectKind as PK, PackBuilder,
    };
    use std::collections::BTreeSet;

    /// Records outcomes instead of failing, and prunes the delta at `prune`.
    struct Recorder {
        prune: Option<u32>,
    }

    #[derive(Default)]
    struct Seen {
        roots: Vec<ObjectId>,
        resolved: BTreeSet<u32>,
        failed: BTreeSet<u32>,
    }

    impl DeltaVisitor for Recorder {
        type Out = Seen;

        fn root(
            &self,
            root: &DeltaRoot,
            kind: ObjectKind,
            data: &[u8],
            out: &mut Seen,
        ) -> Result<bool> {
            if let DeltaRoot::Pack { .. } = root {
                out.roots.push(hash_object(HashAlgo::Sha1, kind, data)?);
            }
            Ok(true)
        }

        fn root_failed(&self, _root: &DeltaRoot, _err: Error, _out: &mut Seen) -> Result<()> {
            Ok(())
        }

        fn delta(
            &self,
            index: u32,
            kind: ObjectKind,
            data: &[u8],
            out: &mut Seen,
        ) -> Result<Option<ObjectId>> {
            if self.prune == Some(index) {
                return Ok(None);
            }
            out.resolved.insert(index);
            Ok(Some(hash_object(HashAlgo::Sha1, kind, data)?))
        }

        fn delta_failed(&self, index: u32, _err: Error, out: &mut Seen) -> Result<()> {
            out.failed.insert(index);
            Ok(())
        }

        fn len(out: &Seen) -> usize {
            out.roots.len() + out.resolved.len() + out.failed.len()
        }

        fn merge(into: &mut Seen, from: &mut Seen) {
            into.roots.append(&mut from.roots);
            into.resolved.append(&mut from.resolved);
            into.failed.append(&mut from.failed);
        }
    }

    fn append(base: &[u8], suffix: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut ops = DeltaOps::new();
        ops.header(base.len(), base.len() + suffix.len())
            .copy(0, base.len())
            .insert(suffix);
        let mut next = base.to_vec();
        next.extend_from_slice(suffix);
        (ops.finish(), next)
    }

    /// Pending deltas of `pack`, in entry order, parsed from each entry's header, with
    /// each entry bounded by the next entry's offset (or the trailer).
    fn pending_deltas(pack: &[u8], entry_offsets: &[usize]) -> (Vec<PendingDelta>, DeltaChildren) {
        let mut pending = Vec::new();
        let mut children = DeltaChildren::default();
        for (i, &offset) in entry_offsets.iter().enumerate() {
            let end = entry_offsets.get(i + 1).copied().unwrap_or(pack.len() - 20);
            let mut rd = PackReader::new(pack);
            rd.pos = offset;
            let (type_code, size) = rd.read_type_size().unwrap();
            let base = match type_code {
                6 => DeltaBase::Offset(offset - rd.read_ofs_neg_offset().unwrap()),
                7 => DeltaBase::Oid(ObjectId::from_bytes(rd.read_exact(20).unwrap()).unwrap()),
                _ => continue,
            };
            children.push(base, u32::try_from(pending.len()).unwrap());
            pending.push(PendingDelta::new(offset, rd.pos, end, size).unwrap());
        }
        children.sort();
        (pending, children)
    }

    #[test]
    fn failed_and_pruned_deltas_leave_their_subtrees_unclaimed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let mut pb = PackBuilder::new(FixtureAlgo::Sha1);
        let root = b"delta walk root object\n".repeat(8);
        let root_idx = pb.add_full(PK::Blob, &root);
        // Delta 0: root -> a. Delta 1: a -> b. Delta 2: ref-delta on a by oid.
        let (d, a) = append(&root, b"a\n");
        let a_idx = pb.add_ofs_delta(root_idx, &d, d.len());
        let (d, b) = append(&a, b"b\n");
        let b_idx = pb.add_ofs_delta(a_idx, &d, d.len());
        let a_oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &a).expect("hash");
        let (d, _) = append(&a, b"ref\n");
        pb.add_ref_delta(a_oid.as_bytes(), &d, d.len());
        // Delta 3: b -> c, pruned by the visitor below; delta 4 hangs off c.
        let (d, c) = append(&b, b"c\n");
        let c_idx = pb.add_ofs_delta(b_idx, &d, d.len());
        let (d, _) = append(&c, b"under pruned\n");
        pb.add_ofs_delta(c_idx, &d, d.len());
        // Delta 5: instructions copy past the end of the root; delta 6 hangs off it.
        let mut bad = DeltaOps::new();
        bad.header(root.len(), 8).copy(root.len(), 8);
        let bad = bad.finish();
        let bad_idx = pb.add_ofs_delta(root_idx, &bad, bad.len());
        let (d, _) = append(b"anything", b"orphan\n");
        pb.add_ofs_delta(bad_idx, &d, d.len());
        let built = pb.build();
        let (pending, children) = pending_deltas(&built.bytes, &built.entry_offsets);
        assert_eq!(pending.len(), 7);
        let root_oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &root).expect("hash");

        for chain_limit in [0, DELTA_CHAIN_BYTES_PER_WORKER] {
            for threads in [1_usize, 4] {
                let visitor = Recorder { prune: Some(3) };
                let walk = DeltaWalk::new(
                    PackBytes::Slice(&built.bytes),
                    &odb,
                    &pending,
                    &children,
                    chain_limit,
                    &visitor,
                );
                let roots = [DeltaRoot::Pack {
                    offset: built.entry_offsets[root_idx],
                    end: built.entry_offsets[root_idx + 1],
                    oid: root_oid,
                }];
                let mut seen = Seen::default();
                let threads = NonZeroUsize::new(threads).unwrap();
                walk.run(&roots, threads, &mut seen, &mut |_| {}).unwrap();
                walk.run(&walk.thin_roots(), threads, &mut seen, &mut |_| {})
                    .unwrap();
                let case = format!("threads={threads} cap={chain_limit}");
                assert_eq!(seen.roots, vec![root_oid], "{case}");
                assert_eq!(seen.resolved, BTreeSet::from([0, 1, 2]), "{case}");
                assert_eq!(seen.failed, BTreeSet::from([5]), "{case}");
                assert_eq!(walk.unclaimed().collect::<Vec<_>>(), vec![4, 6], "{case}");
                assert_eq!(walk.resolved(), 3, "{case}");
            }
        }
    }

    #[test]
    fn evict_ancestors_keeps_the_frame_being_applied() {
        let frame = |len: usize| ChainFrame {
            kind: ObjectKind::Blob,
            data: Some(vec![0; len]),
            ofs_children: &[],
            ref_children: &[],
            next_child: 0,
            anchor: Anchor::Root,
            via: Vec::new(),
        };
        let mut stack = vec![frame(10), frame(20), frame(30)];
        let held: usize = stack.iter().map(ChainFrame::held_bytes).sum();
        assert_eq!(evict_ancestors(&mut stack, held, 35), 30);
        assert!(stack[0].data.is_none() && stack[1].data.is_none());
        assert!(stack[2].data.is_some());
        let mut stack = vec![frame(10), frame(20), frame(30)];
        assert_eq!(evict_ancestors(&mut stack, held, 50), 50);
        assert!(stack[0].data.is_none() && stack[1].data.is_some());
    }
}
