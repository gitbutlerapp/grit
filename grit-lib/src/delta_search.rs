//! Streaming sliding-window delta search for full repacks.
//!
//! Candidates are described by metadata only (kind, size, name hash, recency).
//! They are sorted so that likely delta pairs sit next to each other (same kind,
//! same path hash, larger objects first), then the sorted list is cut into
//! fixed chunks that worker threads search independently. Inside a chunk each
//! target is tried against the previous `window` candidates; object payloads are
//! read from the [`Odb`] only when a pair is actually compared and are dropped
//! as the window slides on, so memory stays bounded by the window rather than
//! by the repository.
//!
//! Chunk boundaries depend only on the candidate list, never on the number of
//! threads, so the chosen deltas are the same on every machine.

use std::cmp::Ordering as CmpOrdering;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::delta_encode::DeltaIndex;
use crate::error::{Error, Result};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;

/// Objects smaller than this are never worth a delta (the header alone eats the gain).
const MIN_DELTA_OBJECT_SIZE: u64 = 50;

/// Objects larger than this are stored whole: indexing them would cost more
/// memory than the delta could save.
const MAX_DELTA_OBJECT_SIZE: u64 = 512 * 1024 * 1024;

/// Preferred number of candidates searched as one unit of work.
const CHUNK_TARGET: usize = 4096;

/// One object that may become a delta target or base.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeltaCandidate {
    /// Caller's identifier for the object (for example its position in the pack's
    /// object list); echoed back in [`FoundDelta`]. Lower ids sort first among
    /// otherwise equal candidates, so callers pass more recent objects lower ids.
    pub id: u32,
    /// Object id used to read the payload.
    pub oid: ObjectId,
    /// Object kind; deltas only pair objects of the same kind.
    pub kind: ObjectKind,
    /// Uncompressed object size in bytes.
    pub size: u64,
    /// Path name hash (see [`crate::pack_name_hash::pack_name_hash`]), `0` when unknown.
    pub name_hash: u32,
    /// Source pack that stores this object whole, when that pack's earlier writer
    /// already compared it against everything else in the same pack.
    pub whole_in_pack: Option<u32>,
    /// Source pack holding this object in any form.
    pub in_pack: Option<u32>,
    /// Length of the longest chain of kept deltas that already depend on this
    /// object; a new delta for it must leave room for them under the depth limit.
    pub dependent_depth: usize,
}

/// Search settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeltaSearchParams {
    /// Number of earlier candidates tried as bases for each target.
    pub window: usize,
    /// Maximum delta chain length.
    pub max_depth: usize,
    /// Soft cap on payload plus index bytes held by one window (`0` = unlimited).
    pub window_memory: u64,
    /// Raw object id length; a delta must save at least this much over a whole object.
    pub hash_len: usize,
    /// Compression level for the returned delta streams.
    pub compression: Compression,
    /// Worker threads.
    pub threads: usize,
}

/// A delta chosen for a target.
#[derive(Debug)]
pub(crate) struct FoundDelta {
    /// [`DeltaCandidate::id`] of the target.
    pub target: u32,
    /// [`DeltaCandidate::id`] of the base.
    pub base: u32,
    /// Uncompressed delta length (the pack entry's size field).
    pub delta_len: usize,
    /// zlib-compressed delta instructions, ready to copy into a pack.
    pub zlib: Vec<u8>,
}

/// Whether `c` can take part in the search at all.
fn searchable(c: &DeltaCandidate) -> bool {
    (MIN_DELTA_OBJECT_SIZE..=MAX_DELTA_OBJECT_SIZE).contains(&c.size)
}

/// Order that puts likely delta pairs next to each other: kind, then path hash,
/// then larger objects first (deltas that remove data are smaller than ones that
/// add it), then more recent objects first.
fn search_order(a: &DeltaCandidate, b: &DeltaCandidate) -> CmpOrdering {
    kind_rank(b.kind)
        .cmp(&kind_rank(a.kind))
        .then_with(|| b.name_hash.cmp(&a.name_hash))
        .then_with(|| b.size.cmp(&a.size))
        .then_with(|| a.id.cmp(&b.id))
}

fn kind_rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

/// Split `sorted` into chunks of about [`CHUNK_TARGET`] candidates, moving each cut
/// forward to the next change of kind or path hash so one file's history is not
/// split (up to twice the target size).
fn chunk_bounds(sorted: &[DeltaCandidate]) -> Vec<(usize, usize)> {
    let n = sorted.len();
    let mut out = Vec::with_capacity(n / CHUNK_TARGET + 1);
    let mut start = 0;
    while start < n {
        let mut end = (start + CHUNK_TARGET).min(n);
        let hard_end = (start + 2 * CHUNK_TARGET).min(n);
        while end < hard_end
            && sorted[end].kind == sorted[end - 1].kind
            && sorted[end].name_hash == sorted[end - 1].name_hash
        {
            end += 1;
        }
        out.push((start, end));
        start = end;
    }
    out
}

/// Pick deltas for `candidates`.
///
/// Returns one [`FoundDelta`] per target that got a delta; every base is another
/// candidate, and chains (counting [`DeltaCandidate::dependent_depth`]) stay within
/// [`DeltaSearchParams::max_depth`]. Bases may come after their targets in any
/// caller order, so the caller must order its output so bases are written first.
///
/// # Errors
///
/// Propagates object read failures and zlib errors.
pub(crate) fn search_deltas(
    odb: &Odb,
    mut candidates: Vec<DeltaCandidate>,
    params: DeltaSearchParams,
) -> Result<Vec<FoundDelta>> {
    if params.window == 0 || params.max_depth == 0 {
        return Ok(Vec::new());
    }
    candidates.retain(searchable);
    candidates.sort_unstable_by(search_order);
    let chunks = chunk_bounds(&candidates);
    let workers = params.threads.clamp(1, chunks.len().max(1));
    if workers == 1 {
        let mut out = Vec::new();
        for &(start, end) in &chunks {
            search_chunk(odb, &candidates[start..end], params, &mut out)?;
        }
        return Ok(out);
    }

    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<Vec<FoundDelta>>)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let _pack_context = odb.pack_read_context_guard();
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(start, end)) = chunks.get(k) else {
                        return;
                    };
                    let mut found = Vec::new();
                    let res = search_chunk(odb, &candidates[start..end], params, &mut found)
                        .map(|()| found);
                    let failed = res.is_err();
                    results
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((k, res));
                    if failed {
                        next.store(chunks.len(), Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_unstable_by_key(|(k, _)| *k);
    let mut out = Vec::new();
    for (_, res) in results {
        out.extend(res?);
    }
    Ok(out)
}

/// One window entry.
#[derive(Default)]
struct Slot {
    /// Index into the chunk, `None` when the slot is empty.
    pos: Option<usize>,
    data: Option<Arc<[u8]>>,
    index: Option<DeltaIndex>,
    /// Chain depth of the delta chosen for this object (0 = stored whole).
    depth: usize,
    /// Best delta found so far while this object is the target.
    best: Option<BestDelta>,
}

struct BestDelta {
    base_pos: usize,
    delta: Vec<u8>,
}

impl Slot {
    fn mem(&self) -> u64 {
        let data = self.data.as_ref().map_or(0, |d| d.len() as u64);
        // A rolling-hash index costs roughly as much again as its base.
        let index = if self.index.is_some() { data } else { 0 };
        data + index
    }

    fn payload(&mut self, odb: &Odb, oid: &ObjectId) -> Result<Arc<[u8]>> {
        if let Some(data) = &self.data {
            return Ok(Arc::clone(data));
        }
        let data: Arc<[u8]> = Arc::from(odb.read(oid)?.data);
        self.data = Some(Arc::clone(&data));
        Ok(data)
    }
}

/// Run the sliding window over one sorted chunk, appending chosen deltas to `out`.
fn search_chunk(
    odb: &Odb,
    list: &[DeltaCandidate],
    params: DeltaSearchParams,
    out: &mut Vec<FoundDelta>,
) -> Result<()> {
    let window = params.window + 1;
    let mut slots: Vec<Slot> = (0..window).map(|_| Slot::default()).collect();
    let mut filled = 0usize;
    let mut idx = 0usize;

    for (pos, target) in list.iter().enumerate() {
        slots[idx] = Slot {
            pos: Some(pos),
            ..Slot::default()
        };

        let max_depth = params.max_depth.saturating_sub(target.dependent_depth);
        let mut best_slot = None;
        if max_depth > 0 {
            for back in 1..window {
                let other = (idx + window - back) % window;
                let Some(src_pos) = slots[other].pos else {
                    break;
                };
                let source = &list[src_pos];
                if source.kind != target.kind {
                    break;
                }
                if try_pair(odb, list, &mut slots, idx, other, max_depth, params)? {
                    best_slot = Some(other);
                }
            }
        }

        if let Some(best) = slots[idx].best.take() {
            let delta_len = best.delta.len();
            out.push(FoundDelta {
                target: target.id,
                base: list[best.base_pos].id,
                delta_len,
                zlib: zlib_compress(&best.delta, params.compression)?,
            });
            // A delta at the depth limit can never serve as a base: let the next
            // target take its slot.
            if slots[idx].depth >= max_depth {
                slots[idx] = Slot::default();
                continue;
            }
            // Keep the chosen base around longer by moving it next to its target.
            if let Some(best) = best_slot {
                rotate_into_place(&mut slots, best, idx, window);
            }
        }

        idx = (idx + 1) % window;
        filled = (filled + 1).min(window - 1);
        if params.window_memory > 0 {
            let mut mem_usage: u64 = slots.iter().map(Slot::mem).sum();
            while mem_usage > params.window_memory && filled > 1 {
                let oldest = (idx + window - filled) % window;
                mem_usage = mem_usage.saturating_sub(slots[oldest].mem());
                slots[oldest] = Slot::default();
                filled -= 1;
            }
        }
    }
    Ok(())
}

/// Try `slots[src]` as a base for `slots[trg]`, keeping the delta when it beats the
/// target's current best. Returns whether it did.
fn try_pair(
    odb: &Odb,
    list: &[DeltaCandidate],
    slots: &mut [Slot],
    trg: usize,
    src: usize,
    max_depth: usize,
    params: DeltaSearchParams,
) -> Result<bool> {
    let (Some(trg_pos), Some(src_pos)) = (slots[trg].pos, slots[src].pos) else {
        return Ok(false);
    };
    let target = &list[trg_pos];
    let source = &list[src_pos];

    // Both stored in the same pack with the target whole: whoever wrote that pack
    // already compared the pair and kept the target whole.
    if target.whole_in_pack.is_some() && target.whole_in_pack == source.in_pack {
        return Ok(false);
    }
    let src_depth = slots[src].depth;
    if src_depth >= max_depth {
        return Ok(false);
    }

    let trg_size = usize::try_from(target.size).unwrap_or(usize::MAX);
    let src_size = usize::try_from(source.size).unwrap_or(usize::MAX);
    let (limit, ref_depth) = match &slots[trg].best {
        None => (trg_size / 2, 1),
        Some(best) => (best.delta.len() + params.hash_len, slots[trg].depth),
    };
    let limit = limit.saturating_sub(params.hash_len);
    // Scale the budget down for deep bases so shallow chains win close calls.
    let max_size =
        limit * (max_depth - src_depth) / (max_depth.saturating_sub(ref_depth) + 1).max(1);
    if max_size == 0 {
        return Ok(false);
    }
    if trg_size.saturating_sub(src_size) >= max_size {
        return Ok(false);
    }
    if trg_size < src_size / 32 {
        return Ok(false);
    }

    let target_data = slots[trg].payload(odb, &target.oid)?;
    let source_data = slots[src].payload(odb, &source.oid)?;
    let index = slots[src]
        .index
        .get_or_insert_with(|| DeltaIndex::new(&source_data));
    let Some(delta) = index.encode(&target_data, max_size) else {
        return Ok(false);
    };
    if let Some(best) = &slots[trg].best {
        let shallower = src_depth + 1 < slots[trg].depth;
        if delta.len() > best.delta.len() || (delta.len() == best.delta.len() && !shallower) {
            return Ok(false);
        }
    }
    slots[trg].depth = src_depth + 1;
    slots[trg].best = Some(BestDelta {
        base_pos: src_pos,
        delta,
    });
    Ok(true)
}

/// Move the slot at `from` into the newest position `target`, shifting the slots
/// after it (the delta target included) one step back, so a useful base is the
/// last to leave the window.
fn rotate_into_place(slots: &mut [Slot], from: usize, target: usize, window: usize) {
    let mut cur = from;
    while cur != target {
        let next = (cur + 1) % window;
        slots.swap(cur, next);
        cur = next;
    }
}

fn zlib_compress(data: &[u8], compression: Compression) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 16), compression);
    enc.write_all(data).map_err(Error::Io)?;
    enc.finish().map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: u32, kind: ObjectKind, name_hash: u32, size: u64) -> DeltaCandidate {
        DeltaCandidate {
            id,
            oid: ObjectId::from_bytes(&[id as u8; 20]).unwrap_or_else(|_| unreachable!()),
            kind,
            size,
            name_hash,
            whole_in_pack: None,
            in_pack: None,
            dependent_depth: 0,
        }
    }

    #[test]
    fn search_order_groups_kind_then_path_then_size_desc_then_recency() {
        let mut v = vec![
            cand(0, ObjectKind::Blob, 5, 100),
            cand(1, ObjectKind::Tree, 9, 100),
            cand(2, ObjectKind::Blob, 5, 300),
            cand(3, ObjectKind::Blob, 7, 50),
            cand(4, ObjectKind::Blob, 5, 300),
        ];
        v.sort_by(search_order);
        let ids: Vec<u32> = v.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![3, 2, 4, 0, 1]);
    }

    #[test]
    fn chunk_bounds_do_not_split_one_path_below_twice_the_target() {
        let mut v: Vec<DeltaCandidate> = (0..CHUNK_TARGET as u32 + 10)
            .map(|i| cand(i, ObjectKind::Blob, 1, 100))
            .collect();
        v.extend((0..5).map(|i| cand(90_000 + i, ObjectKind::Blob, 2, 100)));
        let bounds = chunk_bounds(&v);
        assert_eq!(
            bounds,
            vec![(0, CHUNK_TARGET + 10), (CHUNK_TARGET + 10, v.len())]
        );
    }

    #[test]
    fn chunk_bounds_cut_long_histories_at_twice_the_target() {
        let v: Vec<DeltaCandidate> = (0..5 * CHUNK_TARGET as u32)
            .map(|i| cand(i, ObjectKind::Blob, 1, 100))
            .collect();
        let bounds = chunk_bounds(&v);
        assert_eq!(bounds.first(), Some(&(0, 2 * CHUNK_TARGET)));
        assert_eq!(bounds.last().map(|b| b.1), Some(v.len()));
        assert!(bounds.windows(2).all(|w| w[0].1 == w[1].0));
    }

    #[test]
    fn rotate_into_place_moves_base_behind_target() {
        let mut slots: Vec<Slot> = (0..5)
            .map(|i| Slot {
                pos: Some(i),
                ..Slot::default()
            })
            .collect();
        // Target at 3, best base at 0: order becomes 1, 2, 3, 0, 4.
        rotate_into_place(&mut slots, 0, 3, 5);
        let order: Vec<Option<usize>> = slots.iter().map(|s| s.pos).collect();
        assert_eq!(order, vec![Some(1), Some(2), Some(3), Some(0), Some(4)]);
        // Wrapping around the ring: target at 1, base at 3 in a window of 5.
        rotate_into_place(&mut slots, 3, 1, 5);
        let order: Vec<Option<usize>> = slots.iter().map(|s| s.pos).collect();
        assert_eq!(order, vec![Some(2), Some(0), Some(3), Some(4), Some(1)]);
    }

    #[test]
    fn searchable_skips_tiny_objects() {
        assert!(!searchable(&cand(0, ObjectKind::Blob, 0, 10)));
        assert!(searchable(&cand(0, ObjectKind::Blob, 0, 50)));
    }
}
