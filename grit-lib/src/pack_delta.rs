//! Windowed delta search and parallel pack serialization.
//!
//! [`search_windowed_blob_deltas`] implements Git-style sliding-window delta
//! selection (type/name-hash/size sort, real delta sizes via [`DeltaIndex`], depth
//! and window-memory limits). Segments are fixed from object count and window so
//! parallel worker counts do not change chosen bases.
//!
//! [`write_pack_with_deltas`] streams a pack in batches: a parallel preparation stage
//! (delta bytes + deflate per planned entry) and a serial emission stage (headers,
//! OFS vs REF selection, trailer), so only one batch of encoded output is in memory.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::config::ConfigSet;
use crate::delta_encode::{encode_prefix_extension_delta, DeltaIndex};
use crate::delta_islands::DeltaIslands;
use crate::error::{Error, Result};
use crate::hash::HashingWriter;
use crate::objects::{
    oid_hash_map, HashAlgo, Object, ObjectId, ObjectKind, OidHashMap, OidHashSet,
};
use crate::odb::Odb;
use crate::pack::PackedSlot;
use crate::pack_objects::{
    encode_ofs_delta_distance, encode_pack_object_header, pack_type_code, PackBuildOptions,
    PackStats, PlannedEntry,
};
use crate::parallel::{ParallelOp, ParallelOptions, ThreadPolicy};

/// Whether a delta from `base` to `target` was already rejected by an earlier pack write.
///
/// When both objects come from the same existing pack and `target` is stored whole
/// there, the packer that wrote it already considered (and declined) that pair, so
/// Git does not try it again while reusing deltas. `slots` is `None` when deltas are
/// not being reused.
fn already_rejected_in_pack(
    slots: Option<&OidHashMap<PackedSlot>>,
    target: &ObjectId,
    base: &ObjectId,
) -> bool {
    let Some(slots) = slots else {
        return false;
    };
    match (slots.get(target), slots.get(base)) {
        (Some(t), Some(b)) => t.whole && t.pack == b.pack,
        _ => false,
    }
}

/// Inputs for [`extend_send_order_blob_deltas`].
pub(crate) struct SendOrderDeltaInput<'a> {
    pub send: &'a [ObjectId],
    pub objects: &'a OidHashMap<Object>,
    pub blob_name_hashes: &'a OidHashMap<u32>,
    pub islands: &'a DeltaIslands,
    pub skip_targets: &'a OidHashSet,
    /// Pack locations of the objects, when on-disk deltas are reused.
    pub packed: Option<&'a OidHashMap<PackedSlot>>,
    /// How many earlier blobs with the same name hash are tried as bases.
    pub window: usize,
    pub hash_len: usize,
}

/// Uncompressed delta bytes chosen during search, keyed by delta target oid.
pub(crate) type DeltaEncodings = HashMap<ObjectId, Vec<u8>>;

/// Try consecutive blob deltas in discovery order so bases precede targets in the pack stream.
///
/// Each blob is tried against the last `window` blobs that share its name hash, newest first.
pub(crate) fn extend_send_order_blob_deltas(
    input: SendOrderDeltaInput<'_>,
    delta_to_base: &mut OidHashMap<ObjectId>,
    encodings: &mut DeltaEncodings,
) {
    let SendOrderDeltaInput {
        send,
        objects,
        blob_name_hashes,
        islands,
        skip_targets,
        packed,
        window,
        hash_len,
    } = input;
    if window == 0 {
        return;
    }
    let mut blob_path_keys = std::collections::HashSet::new();
    for &oid in send {
        let Some(obj) = objects.get(&oid) else {
            continue;
        };
        if obj.kind == ObjectKind::Blob && !obj.data.is_empty() {
            blob_path_keys.insert(blob_name_hashes.get(&oid).copied().unwrap_or(0));
        }
    }
    // Single-path histories need Git's newest-first `<` tie rule so window search can
    // build deep chains; multi-path repacks (typical gc) use OID tie-break for stable size.
    let tie_break_equal_len_by_oid = blob_path_keys.len() > 1;
    let mut recent_same_path: HashMap<u32, VecDeque<ObjectId>> = HashMap::new();
    let mut index_cache: HashMap<ObjectId, Arc<DeltaIndex>> = HashMap::new();
    let remember =
        |recent_same_path: &mut HashMap<u32, VecDeque<ObjectId>>, path_key: u32, oid: ObjectId| {
            let recent = recent_same_path.entry(path_key).or_default();
            if recent.len() == window {
                recent.pop_front();
            }
            recent.push_back(oid);
        };
    for &oid in send {
        let Some(obj) = objects.get(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
            continue;
        };
        let path_key = blob_name_hashes.get(&oid).copied().unwrap_or(0);
        if skip_targets.contains(&oid) || delta_to_base.contains_key(&oid) {
            remember(&mut recent_same_path, path_key, oid);
            continue;
        }
        let max_size = obj.data.len().saturating_div(2).saturating_sub(hash_len);
        if max_size > 0 {
            let mut best_base: Option<ObjectId> = None;
            let mut best_encoding: Option<Vec<u8>> = None;
            let mut best_len = max_size.saturating_add(1);
            if let Some(candidates) = recent_same_path.get(&path_key) {
                for &base in candidates.iter().rev() {
                    if base == oid
                        || !islands.in_same_island(&oid, &base)
                        || already_rejected_in_pack(packed, &oid, &base)
                    {
                        continue;
                    }
                    let Some(base_obj) = objects.get(&base) else {
                        continue;
                    };
                    let base_data = &base_obj.data;
                    let chosen =
                        if obj.data.starts_with(base_data) && obj.data.len() > base_data.len() {
                            encode_prefix_extension_delta(base_data, &obj.data).ok()
                        } else {
                            let index = index_cache
                                .entry(base)
                                .or_insert_with(|| Arc::new(DeltaIndex::new(base_data)));
                            index.encode(&obj.data, max_size)
                        };
                    if let Some(delta) = chosen {
                        if delta.len() > max_size {
                            continue;
                        }
                        let better = if !tie_break_equal_len_by_oid {
                            delta.len() < best_len
                        } else {
                            match best_base {
                                None => true,
                                Some(_prev) if delta.len() < best_len => true,
                                Some(prev) if delta.len() == best_len => {
                                    base.as_bytes() < prev.as_bytes()
                                }
                                _ => false,
                            }
                        };
                        if better {
                            best_len = delta.len();
                            best_base = Some(base);
                            best_encoding = Some(delta);
                        }
                    }
                }
            }
            if let Some(base) = best_base {
                delta_to_base.insert(oid, base);
                if let Some(bytes) = best_encoding {
                    encodings.insert(oid, bytes);
                }
            }
        }
        remember(&mut recent_same_path, path_key, oid);
    }
}

/// Resolve [`ParallelOptions`] for windowed delta search from repo config.
pub(crate) fn pack_delta_search_parallel_options(
    odb: &Odb,
    search_threads: Option<usize>,
) -> Result<ParallelOptions> {
    let cfg = pack_config(odb)?;
    let policy = ThreadPolicy::with_threads_override(search_threads);
    Ok(policy.try_resolve(ParallelOp::PackDeltaSearch, &cfg)?)
}

/// Inputs for [`search_windowed_blob_deltas`].
pub(crate) struct DeltaSearchInput<'a> {
    pub objects: &'a OidHashMap<Object>,
    pub blob_name_hashes: &'a OidHashMap<u32>,
    pub islands: &'a DeltaIslands,
    pub skip_targets: &'a OidHashSet,
    pub skip_as_target: &'a OidHashSet,
    /// Send-order / reuse deltas fixed before window search; used for warmup depth only.
    pub warmup_delta_edges: &'a OidHashMap<ObjectId>,
    /// Pack locations of the objects, when on-disk deltas are reused.
    pub packed: Option<&'a OidHashMap<PackedSlot>>,
    pub opts: &'a PackBuildOptions,
}

/// Git-style windowed delta search over in-pack blobs.
///
/// Returns target → base edges for newly chosen deltas (not including entries
/// listed in `skip_targets`). Respects delta islands when `islands` is active.
pub(crate) fn search_windowed_blob_deltas(
    odb: &Odb,
    input: DeltaSearchInput<'_>,
    search_threads: Option<usize>,
) -> Result<(OidHashMap<ObjectId>, DeltaEncodings)> {
    let DeltaSearchInput {
        objects,
        blob_name_hashes,
        islands,
        skip_targets,
        skip_as_target,
        warmup_delta_edges,
        packed,
        opts,
    } = input;
    if opts.window == 0 || opts.max_depth == 0 {
        return Ok((oid_hash_map(), HashMap::new()));
    }

    let cfg = pack_config(odb)?;
    let window_memory = cfg.pack_window_memory_bytes();
    let hash_len = odb.hash_algo().len();
    let window = opts.window + 1;

    let mut sorted: Vec<ObjectId> = objects
        .iter()
        .filter_map(|(&oid, obj)| {
            if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
                return None;
            }
            if skip_targets.contains(&oid) || skip_as_target.contains(&oid) {
                return None;
            }
            Some(oid)
        })
        .collect();
    sorted.sort_by(|a, b| type_size_sort(a, b, objects, blob_name_hashes));

    if sorted.is_empty() {
        return Ok((oid_hash_map(), HashMap::new()));
    }

    let par = pack_delta_search_parallel_options(odb, search_threads)?;
    let segments = fixed_delta_segments(sorted.len(), opts.window);
    let workers = par.threads.get().min(segments.len()).max(1);
    let sorted = Arc::new(sorted);

    let segment_params = SegmentSearchParams {
        window,
        max_depth: opts.max_depth,
        hash_len,
        window_memory,
        packed,
    };

    if workers <= 1 {
        let mut out = oid_hash_map();
        let mut encodings = HashMap::new();
        for (start, end) in &segments {
            let warmup = segment_warmup(&sorted, *start, opts.window);
            find_deltas_in_segment(
                &sorted[*start..*end],
                &warmup,
                objects,
                islands,
                segment_params,
                &mut out,
                &mut encodings,
                warmup_delta_edges,
            );
        }
        return Ok((out, encodings));
    }

    let next_segment = Arc::new(AtomicUsize::new(0));
    let segments = Arc::new(segments);
    let merged: Arc<Mutex<OidHashMap<ObjectId>>> = Arc::new(Mutex::new(oid_hash_map()));
    let merged_enc: Arc<Mutex<DeltaEncodings>> = Arc::new(Mutex::new(HashMap::new()));

    thread::scope(|scope| {
        for _ in 0..workers {
            let merged = Arc::clone(&merged);
            let merged_enc = Arc::clone(&merged_enc);
            let sorted = Arc::clone(&sorted);
            let next_segment = Arc::clone(&next_segment);
            let segments = Arc::clone(&segments);
            scope.spawn(move || loop {
                let seg_idx = next_segment.fetch_add(1, Ordering::SeqCst);
                if seg_idx >= segments.len() {
                    break;
                }
                let (start, end) = segments[seg_idx];
                let warmup = segment_warmup(&sorted, start, opts.window);
                let mut local = oid_hash_map();
                let mut local_enc = HashMap::new();
                find_deltas_in_segment(
                    &sorted[start..end],
                    &warmup,
                    objects,
                    islands,
                    segment_params,
                    &mut local,
                    &mut local_enc,
                    warmup_delta_edges,
                );
                if let Ok(mut guard) = merged.lock() {
                    for (k, v) in local {
                        guard.insert(k, v);
                    }
                }
                if let Ok(mut guard) = merged_enc.lock() {
                    for (k, v) in local_enc {
                        guard.insert(k, v);
                    }
                }
            });
        }
    });

    let merged = Arc::try_unwrap(merged)
        .ok()
        .and_then(|m| m.into_inner().ok())
        .unwrap_or_default();
    let encodings = Arc::try_unwrap(merged_enc)
        .ok()
        .and_then(|m| m.into_inner().ok())
        .unwrap_or_default();
    Ok((merged, encodings))
}

fn type_size_sort(
    a: &ObjectId,
    b: &ObjectId,
    objects: &OidHashMap<Object>,
    name_hashes: &OidHashMap<u32>,
) -> std::cmp::Ordering {
    let ha = name_hashes.get(a).copied().unwrap_or(0);
    let hb = name_hashes.get(b).copied().unwrap_or(0);
    hb.cmp(&ha)
        .then_with(|| objects[b].data.len().cmp(&objects[a].data.len()))
        .then_with(|| b.as_bytes().cmp(a.as_bytes()))
}

#[derive(Clone, Copy)]
struct SegmentSearchParams<'a> {
    window: usize,
    max_depth: usize,
    hash_len: usize,
    window_memory: u64,
    packed: Option<&'a OidHashMap<PackedSlot>>,
}

fn segment_warmup(sorted: &[ObjectId], start: usize, window: usize) -> Vec<ObjectId> {
    if start == 0 {
        return Vec::new();
    }
    let warm_start = start.saturating_sub(window);
    sorted[warm_start..start].to_vec()
}

fn fixed_delta_segments(n: usize, window: usize) -> Vec<(usize, usize)> {
    if n == 0 {
        return Vec::new();
    }
    let chunk = (2 * window).max(1);
    if n <= chunk {
        return vec![(0, n)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        let end = (start + chunk).min(n);
        out.push((start, end));
        start = end;
    }
    out
}

struct WindowSlot<'a> {
    oid: Option<ObjectId>,
    data: &'a [u8],
    depth: usize,
    base: Option<ObjectId>,
    delta_size: Option<usize>,
    delta_encoding: Option<Vec<u8>>,
}

impl<'a> WindowSlot<'a> {
    const EMPTY: Self = Self {
        oid: None,
        data: &[],
        depth: 0,
        base: None,
        delta_size: None,
        delta_encoding: None,
    };

    /// Empty the slot, dropping its cached delta index; returns the memory released.
    fn clear(&mut self, index_cache: &mut HashMap<ObjectId, Arc<DeltaIndex>>) -> usize {
        let mut mem = self.data.len();
        if let Some(oid) = self.oid {
            if index_cache.remove(&oid).is_some() {
                mem += self.data.len() * 2;
            }
        }
        *self = Self::EMPTY;
        mem
    }

    fn load(&mut self, oid: ObjectId, data: &'a [u8]) {
        self.oid = Some(oid);
        self.data = data;
        self.depth = 0;
        self.base = None;
        self.delta_size = None;
        self.delta_encoding = None;
    }
}

fn delta_chain_depth(edges: &OidHashMap<ObjectId>, oid: ObjectId) -> usize {
    let mut depth = 0usize;
    let mut cur = Some(oid);
    let mut guard = 0usize;
    while let Some(t) = cur {
        let Some(base) = edges.get(&t) else {
            break;
        };
        depth += 1;
        cur = Some(*base);
        guard += 1;
        if guard > 4096 {
            break;
        }
    }
    depth
}

#[allow(clippy::too_many_arguments)]
fn find_deltas_in_segment(
    list: &[ObjectId],
    warmup: &[ObjectId],
    objects: &OidHashMap<Object>,
    islands: &DeltaIslands,
    params: SegmentSearchParams<'_>,
    out: &mut OidHashMap<ObjectId>,
    encodings: &mut DeltaEncodings,
    prior_edges: &OidHashMap<ObjectId>,
) {
    if list.is_empty() {
        return;
    }
    let SegmentSearchParams {
        window,
        max_depth,
        hash_len,
        window_memory,
        packed,
    } = params;
    let mut array: Vec<WindowSlot<'_>> = (0..window).map(|_| WindowSlot::EMPTY).collect();
    let mut index_cache: HashMap<ObjectId, Arc<DeltaIndex>> = HashMap::new();
    let mut idx = 0usize;
    let mut count = 0usize;
    let mut mem_usage = 0u64;

    for &warm_oid in warmup {
        mem_usage = mem_usage.saturating_sub(array[idx].clear(&mut index_cache) as u64);
        array[idx].load(warm_oid, &objects[&warm_oid].data);
        if let Some(base) = prior_edges.get(&warm_oid) {
            array[idx].base = Some(*base);
            array[idx].depth = delta_chain_depth(prior_edges, warm_oid);
        }
        mem_usage = mem_usage.saturating_add(array[idx].data.len() as u64);
        idx += 1;
        if count + 1 < window {
            count += 1;
        }
        if idx >= window {
            idx = 0;
        }
    }

    for &entry_oid in list {
        mem_usage = mem_usage.saturating_sub(array[idx].clear(&mut index_cache) as u64);
        array[idx].load(entry_oid, &objects[&entry_oid].data);
        mem_usage = mem_usage.saturating_add(array[idx].data.len() as u64);

        while window_memory > 0 && mem_usage > window_memory && count > 1 {
            let tail = (idx + window - count) % window;
            mem_usage = mem_usage.saturating_sub(array[tail].clear(&mut index_cache) as u64);
            count -= 1;
        }

        if max_depth == 0 {
            continue;
        }

        let mut best_base = None::<usize>;
        let mut j = window;
        while j > 1 {
            j -= 1;
            let other_idx = (idx + j) % window;
            let Some(src_oid) = array[other_idx].oid else {
                break;
            };
            if already_rejected_in_pack(packed, &entry_oid, &src_oid) {
                continue;
            }
            let src_depth = array[other_idx].depth;
            let src_data = array[other_idx].data;
            let ret = try_delta_pair(
                &mut array[idx],
                TryDeltaContext {
                    src_oid,
                    src_depth,
                    src_data,
                    trg_oid: entry_oid,
                    max_depth,
                    hash_len,
                    islands,
                },
                &mut index_cache,
                &mut mem_usage,
            );
            if ret < 0 {
                break;
            }
            if ret > 0 {
                best_base = Some(other_idx);
            }
        }

        if let Some(base_oid) = array[idx].base {
            if max_depth <= array[idx].depth {
                if let Some(bytes) = array[idx].delta_encoding.take() {
                    encodings.insert(entry_oid, bytes);
                }
                out.insert(entry_oid, base_oid);
                continue;
            }
            if let Some(bytes) = array[idx].delta_encoding.take() {
                encodings.insert(entry_oid, bytes);
            }
            out.insert(entry_oid, base_oid);
            if let Some(best_idx) = best_base {
                rotate_best_base(&mut array, idx, best_idx, window);
            }
        }

        idx += 1;
        if count + 1 < window {
            count += 1;
        }
        if idx >= window {
            idx = 0;
        }
    }
}

fn rotate_best_base(slots: &mut [WindowSlot<'_>], idx: usize, best_base: usize, window: usize) {
    let swap = std::mem::replace(&mut slots[best_base], WindowSlot::EMPTY);
    let mut dst = best_base;
    let dist = (window + idx - best_base) % window;
    for _ in 0..dist {
        let src = (dst + 1) % window;
        slots[dst] = std::mem::replace(&mut slots[src], WindowSlot::EMPTY);
        dst = src;
    }
    slots[dst] = swap;
}

struct TryDeltaContext<'a> {
    src_oid: ObjectId,
    src_depth: usize,
    src_data: &'a [u8],
    trg_oid: ObjectId,
    max_depth: usize,
    hash_len: usize,
    islands: &'a DeltaIslands,
}

/// Whether a newly found delta should replace the slot's current best (Git-style tie-break).
fn prefer_delta_candidate(
    trg: &WindowSlot<'_>,
    delta_len: usize,
    src_oid: ObjectId,
    src_depth: usize,
) -> bool {
    let Some(prev_len) = trg.delta_size else {
        return true;
    };
    if delta_len < prev_len {
        return true;
    }
    if delta_len > prev_len {
        return false;
    }
    let new_depth = src_depth + 1;
    if new_depth < trg.depth {
        return true;
    }
    if new_depth > trg.depth {
        return false;
    }
    trg.base
        .is_none_or(|cur| src_oid.as_bytes() < cur.as_bytes())
}

fn try_delta_pair(
    trg: &mut WindowSlot<'_>,
    ctx: TryDeltaContext<'_>,
    index_cache: &mut HashMap<ObjectId, Arc<DeltaIndex>>,
    mem_usage: &mut u64,
) -> i32 {
    let TryDeltaContext {
        src_oid,
        src_depth,
        src_data,
        trg_oid,
        max_depth,
        hash_len,
        islands,
    } = ctx;
    if !islands.in_same_island(&trg_oid, &src_oid) {
        return 0;
    }

    if src_depth >= max_depth {
        return 0;
    }

    let trg_size = trg.data.len();
    let src_size = src_data.len();
    let (base_max, ref_depth) = match trg.delta_size {
        None => (trg_size.saturating_div(2).saturating_sub(hash_len), 1usize),
        Some(ds) => (ds, trg.depth),
    };
    let num = max_depth.saturating_sub(src_depth);
    let den = max_depth.saturating_sub(ref_depth) + 1;
    let max_size = if den == 0 {
        0
    } else {
        (base_max as u64 * num as u64 / den as u64) as usize
    };
    if max_size == 0 {
        return 0;
    }
    let sizediff = trg_size.saturating_sub(src_size);
    if sizediff >= max_size {
        return 0;
    }
    if trg_size < src_size / 32 {
        return 0;
    }

    if trg.data.is_empty() {
        return 0;
    }

    if trg.data.starts_with(src_data) && trg.data.len() > src_data.len() {
        let delta = match encode_prefix_extension_delta(src_data, trg.data) {
            Ok(d) => d,
            Err(_) => return 0,
        };
        if delta.len() > max_size {
            return 0;
        }
        if !prefer_delta_candidate(trg, delta.len(), src_oid, src_depth) {
            return 0;
        }
        trg.base = Some(src_oid);
        trg.depth = src_depth + 1;
        trg.delta_size = Some(delta.len());
        trg.delta_encoding = Some(delta);
        return 1;
    }

    let index = index_cache.entry(src_oid).or_insert_with(|| {
        *mem_usage = mem_usage.saturating_add((src_data.len() * 2) as u64);
        Arc::new(DeltaIndex::new(src_data))
    });

    let Some(delta) = index.encode(trg.data, max_size) else {
        return 0;
    };
    if !prefer_delta_candidate(trg, delta.len(), src_oid, src_depth) {
        return 0;
    }

    trg.base = Some(src_oid);
    trg.depth = src_depth + 1;
    trg.delta_size = Some(delta.len());
    trg.delta_encoding = Some(delta);
    1
}

/// Per-entry compressed payload produced by the parallel stage.
enum PreparedBody {
    /// Entire pack object encoding copied verbatim (reused on-disk whole object).
    Raw(Vec<u8>),
    /// zlib-compressed bytes only (header written in the serial stage).
    Zlib(Vec<u8>),
}

struct PreparedEntry {
    body: PreparedBody,
    /// Uncompressed payload size for pack headers (ignored for [`PreparedBody::Raw`]).
    payload_len: usize,
    /// Set for freshly compressed whole objects.
    whole_kind: Option<ObjectKind>,
    /// Body copied verbatim from an existing local pack.
    reused: bool,
}

/// Resolve [`ParallelOptions`] for pack writing from repo config and an optional override.
pub(crate) fn pack_write_parallel_options(
    odb: &Odb,
    write_threads: Option<usize>,
) -> Result<ParallelOptions> {
    let cfg = pack_config(odb)?;
    let policy = ThreadPolicy::with_threads_override(write_threads);
    Ok(policy.try_resolve(ParallelOp::PackWrite, &cfg)?)
}

fn pack_config(odb: &Odb) -> Result<ConfigSet> {
    Ok(match odb.config_git_dir() {
        Some(git_dir) => {
            let env = crate::environment::Environment::empty();
            ConfigSet::load(&env, Some(git_dir), true)?
        }
        None => ConfigSet::new(),
    })
}

/// Serialize a delta plan into an in-memory PACK v2 stream with matching index rows.
pub(crate) fn serialize_pack_with_deltas(
    odb: &Odb,
    entries: &[PlannedEntry],
    plan_payloads: &OidHashMap<Arc<[u8]>>,
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
) -> Result<crate::pack_objects::SerializedDeltaPack> {
    let mut bytes = Vec::new();
    let mut index_rows = Vec::with_capacity(entries.len());
    write_pack_with_deltas(
        odb,
        entries,
        plan_payloads,
        opts,
        write_threads,
        &mut bytes,
        Some(&mut index_rows),
    )?;
    Ok(crate::pack_objects::SerializedDeltaPack {
        bytes,
        index_rows,
        staging_path: None,
    })
}

/// Planned entries encoded per parallel batch while streaming a pack, which bounds
/// how much compressed output is held in memory at once.
const WRITE_BATCH_ENTRIES: usize = 256;

/// Stream a delta plan to `w` as a PACK v2 stream, including the trailing checksum.
///
/// Entries are delta-encoded and deflated in parallel batches, then emitted in plan
/// order (choosing `OFS_DELTA` when the base was already written and
/// `opts.use_ofs_delta` is set, `REF_DELTA` otherwise). When `index_rows` is set it
/// receives `(oid, pack offset, crc32)` for each entry in pack order.
///
/// # Errors
///
/// Returns object read or encoding errors, and [`Error::Io`] when writing to `w` fails.
pub(crate) fn write_pack_with_deltas(
    odb: &Odb,
    entries: &[PlannedEntry],
    plan_payloads: &OidHashMap<Arc<[u8]>>,
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
    w: &mut dyn Write,
    index_rows: Option<&mut Vec<(ObjectId, u64, u32)>>,
) -> Result<PackStats> {
    write_pack_in_batches(
        odb,
        entries,
        plan_payloads,
        opts,
        write_threads,
        w,
        index_rows,
        WRITE_BATCH_ENTRIES,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_pack_in_batches(
    odb: &Odb,
    entries: &[PlannedEntry],
    plan_payloads: &OidHashMap<Arc<[u8]>>,
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
    w: &mut dyn Write,
    mut index_rows: Option<&mut Vec<(ObjectId, u64, u32)>>,
    batch_entries: usize,
) -> Result<PackStats> {
    let par_opts = pack_write_parallel_options(odb, write_threads)?;
    let compression = pack_zlib_compression(odb)?;
    let algo = odb.hash_algo();
    let count = u32::try_from(entries.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    let delta_count = entries.iter().filter(|e| e.base.is_some()).count() as u32;
    let mut verbatim_reuse_objects = 0u32;
    let mut verbatim_reuse_bytes = 0u64;

    let mut out = HashingWriter::new(algo, w);
    let mut header: Vec<u8> = Vec::with_capacity(12 + algo.len());
    header.extend_from_slice(b"PACK");
    header.extend_from_slice(&2u32.to_be_bytes());
    header.extend_from_slice(&count.to_be_bytes());
    out.write_all(&header).map_err(Error::Io)?;
    let mut offset = header.len() as u64;

    let mut oid_to_offset: OidHashMap<u64> = oid_hash_map();
    let mut emit = |entry: &PlannedEntry, prep: &PreparedEntry| -> Result<()> {
        header.clear();
        match (entry.base, &prep.body) {
            (None, PreparedBody::Raw(_)) => {}
            (None, PreparedBody::Zlib(_)) => {
                let kind = prep.whole_kind.unwrap_or(entry.kind);
                encode_pack_object_header(&mut header, pack_type_code(kind), prep.payload_len);
            }
            (Some(base_oid), _) => {
                let in_pack_offset = oid_to_offset.get(&base_oid).copied();
                if let Some(base_off) = in_pack_offset.filter(|_| opts.use_ofs_delta) {
                    let dist = offset.checked_sub(base_off).ok_or_else(|| {
                        Error::CorruptObject("ofs-delta distance underflow".to_owned())
                    })?;
                    encode_pack_object_header(&mut header, 6, prep.payload_len);
                    encode_ofs_delta_distance(&mut header, dist);
                } else {
                    if base_oid.as_bytes().len() != algo.len() {
                        return Err(Error::CorruptObject(
                            "ref-delta base oid width mismatch".to_owned(),
                        ));
                    }
                    encode_pack_object_header(&mut header, 7, prep.payload_len);
                    header.extend_from_slice(base_oid.as_bytes());
                }
            }
        }
        let body = match &prep.body {
            PreparedBody::Raw(bytes) | PreparedBody::Zlib(bytes) => bytes,
        };
        out.write_all(&header).map_err(Error::Io)?;
        out.write_all(body).map_err(Error::Io)?;
        if let Some(rows) = index_rows.as_deref_mut() {
            let mut crc = crc32fast::Hasher::new();
            crc.update(&header);
            crc.update(body);
            rows.push((entry.oid, offset, crc.finalize()));
        }
        if prep.reused {
            verbatim_reuse_objects += 1;
            verbatim_reuse_bytes += body.len() as u64;
        }
        oid_to_offset.insert(entry.oid, offset);
        offset += (header.len() + body.len()) as u64;
        Ok(())
    };
    let prepare =
        |batch: &[PlannedEntry]| prepare_batch(odb, batch, plan_payloads, opts, compression);
    let batches: Vec<&[PlannedEntry]> = entries.chunks(batch_entries.max(1)).collect();
    let workers = par_opts.threads.get();
    if workers > 1 && entries.len() >= MIN_ENTRIES_FOR_PARALLEL_PACK_WRITE && batches.len() > 1 {
        // Long-lived workers keep the odb's pack caches across batches; a pool per
        // batch would reopen every pack index on each new thread.
        crate::parallel::run_ordered_batches(
            &batches,
            1,
            workers,
            || odb.pack_read_context_guard(),
            |_, batch| prepare(batch),
            |done, prepared| {
                for (batch, preps) in done.iter().zip(prepared) {
                    for (entry, prep) in batch.iter().zip(&preps) {
                        emit(entry, prep)?;
                    }
                }
                Ok(())
            },
        )?;
    } else {
        for batch in batches {
            for (entry, prep) in batch.iter().zip(&prepare(batch)?) {
                emit(entry, prep)?;
            }
        }
    }
    out.finish().map_err(Error::Io)?;
    Ok(PackStats {
        object_count: count,
        pack_bytes: offset + algo.len() as u64,
        delta_count,
        verbatim_reuse_bytes,
        verbatim_reuse_objects,
        ..PackStats::default()
    })
}

/// Avoid thread-pool overhead on small repacks (typical loose-object gc).
// Parallel pack preparation pays off on large packs; on ~600-object gc repacks the
// thread pool and index sharing often lose to a single-threaded pass.
const MIN_ENTRIES_FOR_PARALLEL_PACK_WRITE: usize = 2048;

/// Delta-encode and deflate one batch of planned entries, in order.
fn prepare_batch(
    odb: &Odb,
    entries: &[PlannedEntry],
    plan_payloads: &OidHashMap<Arc<[u8]>>,
    opts: &PackBuildOptions,
    compression: Compression,
) -> Result<Vec<PreparedEntry>> {
    let delta_indices = build_delta_indices(odb, entries, plan_payloads)?;
    entries
        .iter()
        .map(|entry| {
            prepare_one_entry(odb, entry, plan_payloads, &delta_indices, opts, compression)
        })
        .collect()
}

fn build_delta_indices(
    odb: &Odb,
    entries: &[PlannedEntry],
    plan_payloads: &OidHashMap<Arc<[u8]>>,
) -> Result<HashMap<ObjectId, Arc<DeltaIndex>>> {
    let mut map: HashMap<ObjectId, Arc<DeltaIndex>> = HashMap::new();
    for entry in entries {
        let Some(base_oid) = entry.base else {
            continue;
        };
        if entry.reused_delta_zlib.is_some() || entry.reused_delta.is_some() {
            continue;
        }
        let base_data = object_payload(odb, base_oid, entry, plan_payloads)?;
        let target_data = object_payload(odb, entry.oid, entry, plan_payloads)?;
        if target_data.starts_with(base_data.as_ref()) && target_data.len() > base_data.len() {
            continue;
        }
        map.entry(base_oid)
            .or_insert_with(|| Arc::new(DeltaIndex::new(base_data.as_ref())));
    }
    Ok(map)
}

fn object_payload<'a>(
    odb: &'a Odb,
    oid: ObjectId,
    entry: &'a PlannedEntry,
    plan_payloads: &'a OidHashMap<Arc<[u8]>>,
) -> Result<Cow<'a, [u8]>> {
    if !entry.data.is_empty() && entry.oid == oid {
        return Ok(Cow::Borrowed(&entry.data));
    }
    if let Some(bytes) = plan_payloads.get(&oid) {
        return Ok(Cow::Borrowed(bytes.as_ref()));
    }
    Ok(Cow::Owned(odb.read(&oid)?.data))
}

fn prepare_one_entry(
    odb: &Odb,
    entry: &PlannedEntry,
    plan_payloads: &OidHashMap<Arc<[u8]>>,
    delta_indices: &HashMap<ObjectId, Arc<DeltaIndex>>,
    opts: &PackBuildOptions,
    compression: Compression,
) -> Result<PreparedEntry> {
    match entry.base {
        None => prepare_whole(odb, entry, plan_payloads, opts, compression),
        Some(base_oid) => prepare_delta(
            odb,
            entry,
            base_oid,
            plan_payloads,
            delta_indices,
            compression,
        ),
    }
}

fn pack_zlib_compression(odb: &Odb) -> Result<Compression> {
    odb.pack_compression()
}

fn prepare_whole(
    odb: &Odb,
    entry: &PlannedEntry,
    plan_payloads: &OidHashMap<Arc<[u8]>>,
    opts: &PackBuildOptions,
    compression: Compression,
) -> Result<PreparedEntry> {
    if opts.reuse_objects && odb.hash_algo() == HashAlgo::Sha1 {
        if let Some(raw) = crate::pack::packed_full_object_slice(odb.objects_dir(), &entry.oid)? {
            return Ok(PreparedEntry {
                body: PreparedBody::Raw(raw),
                payload_len: 0,
                whole_kind: None,
                reused: true,
            });
        }
    }
    let payload = object_payload(odb, entry.oid, entry, plan_payloads)?.into_owned();
    let zlib = zlib_compress(&payload, compression)?;
    Ok(PreparedEntry {
        body: PreparedBody::Zlib(zlib),
        payload_len: payload.len(),
        whole_kind: Some(entry.kind),
        reused: false,
    })
}

fn prepare_delta(
    odb: &Odb,
    entry: &PlannedEntry,
    base_oid: ObjectId,
    plan_payloads: &OidHashMap<Arc<[u8]>>,
    delta_indices: &HashMap<ObjectId, Arc<DeltaIndex>>,
    compression: Compression,
) -> Result<PreparedEntry> {
    if let Some((delta_size, pack_bytes, zlib_start, zlib_end)) = &entry.reused_delta_zlib {
        let delta_len = usize::try_from(*delta_size)
            .map_err(|_| Error::CorruptObject("reused delta size exceeds usize".to_owned()))?;
        let zlib = pack_bytes[*zlib_start..*zlib_end].to_vec();
        return Ok(PreparedEntry {
            body: PreparedBody::Zlib(zlib),
            payload_len: delta_len,
            whole_kind: None,
            reused: true,
        });
    }

    let delta = if let Some(reused) = &entry.reused_delta {
        reused.clone()
    } else {
        let target_data = object_payload(odb, entry.oid, entry, plan_payloads)?;
        let base_data = object_payload(odb, base_oid, entry, plan_payloads)?;
        if target_data.starts_with(base_data.as_ref()) && target_data.len() > base_data.len() {
            encode_prefix_extension_delta(base_data.as_ref(), &target_data)?
        } else {
            let index = delta_indices
                .get(&base_oid)
                .ok_or_else(|| Error::CorruptObject("missing delta index for pack base".into()))?;
            index.encode(&target_data, 0).ok_or_else(|| {
                Error::CorruptObject("failed to encode pack delta for object".into())
            })?
        }
    };

    let zlib = zlib_compress(&delta, compression)?;
    Ok(PreparedEntry {
        body: PreparedBody::Zlib(zlib),
        payload_len: delta.len(),
        whole_kind: None,
        reused: false,
    })
}

fn zlib_compress(data: &[u8], compression: Compression) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 3 + 64), compression);
    enc.write_all(data).map_err(Error::Io)?;
    enc.finish().map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{oid_hash_map, oid_hash_set, Object, ObjectId, ObjectKind};
    use std::num::NonZeroUsize;

    #[test]
    #[ignore = "perf smoke: cargo test -p grit-lib --release --lib parallel_segment_search_scales -- --ignored --nocapture"]
    fn parallel_segment_search_scales_on_large_blob_set() {
        use std::time::Instant;

        let mut objects = oid_hash_map();
        let mut blob_name_hashes = oid_hash_map();
        let mut sorted = Vec::new();
        for i in 0..200 {
            let hex = format!("{:040x}", i + 1);
            let oid = ObjectId::from_hex(&hex).unwrap();
            let mut data = vec![0u8; 16 * 1024];
            data[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            objects.insert(oid, Object::new(ObjectKind::Blob, data));
            blob_name_hashes.insert(oid, i as u32);
            sorted.push(oid);
        }
        sorted.sort_by(|a, b| type_size_sort(a, b, &objects, &blob_name_hashes));
        let dir = tempfile::tempdir().expect("tempdir");
        let objects_dir = dir.path().join("objects");
        std::fs::create_dir_all(&objects_dir).expect("objects");
        let odb = Odb::new(&objects_dir);
        let opts = PackBuildOptions {
            delta: true,
            window: 10,
            max_depth: 50,
            ..PackBuildOptions::default()
        };
        let islands = DeltaIslands::default();
        let skip = oid_hash_set();
        let warmup_edges = oid_hash_map();
        let run = |threads: usize| {
            search_windowed_blob_deltas(
                &odb,
                DeltaSearchInput {
                    objects: &objects,
                    blob_name_hashes: &blob_name_hashes,
                    islands: &islands,
                    skip_targets: &skip,
                    skip_as_target: &skip,
                    warmup_delta_edges: &warmup_edges,
                    packed: None,
                    opts: &opts,
                },
                Some(threads),
            )
            .expect("search")
            .0
        };
        let _ = run(1);
        let t1 = Instant::now();
        let one = run(1);
        let d1 = t1.elapsed();
        let t4 = Instant::now();
        let four = run(4);
        let d4 = t4.elapsed();
        assert_eq!(one, four, "parallel search must match serial edges");
        let speedup = d1.as_secs_f64() / d4.as_secs_f64();
        eprintln!(
            "parallel delta search: 1 thread {:.1}ms, 4 threads {:.1}ms, speedup {:.2}x",
            d1.as_secs_f64() * 1000.0,
            d4.as_secs_f64() * 1000.0,
            speedup
        );
        assert!(
            speedup >= 2.5,
            "expected >=2.5× speedup from 1 to 4 search threads, got {speedup:.2}×"
        );
    }

    #[test]
    fn find_deltas_records_edge_at_max_depth_before_evicting() {
        let grand_oid = ObjectId::from_hex(&"0".repeat(40)).unwrap();
        let base_oid = ObjectId::from_hex(&"1".repeat(40)).unwrap();
        let target_oid = ObjectId::from_hex(&"2".repeat(40)).unwrap();
        let shared = vec![b'x'; 512];
        let mut base_data = shared.clone();
        base_data.push(b'a');
        let mut target_data = base_data.clone();
        target_data.push(b'b');
        let mut objects = oid_hash_map();
        objects.insert(grand_oid, Object::new(ObjectKind::Blob, shared));
        objects.insert(base_oid, Object::new(ObjectKind::Blob, base_data));
        objects.insert(target_oid, Object::new(ObjectKind::Blob, target_data));
        let list = vec![target_oid];
        let warmup = vec![base_oid];
        let mut prior = oid_hash_map();
        prior.insert(base_oid, grand_oid);
        let mut out = oid_hash_map();
        let mut encodings = HashMap::new();
        find_deltas_in_segment(
            &list,
            &warmup,
            &objects,
            &DeltaIslands::default(),
            SegmentSearchParams {
                window: 2,
                max_depth: 2,
                hash_len: 0,
                window_memory: 0,
                packed: None,
            },
            &mut out,
            &mut encodings,
            &prior,
        );
        assert_eq!(out.get(&target_oid), Some(&base_oid));
        assert!(encodings.contains_key(&target_oid));
    }

    fn similar_blobs(count: u8) -> (Vec<ObjectId>, OidHashMap<Object>) {
        let mut oids = Vec::new();
        let mut objects = oid_hash_map();
        for i in 0..count {
            let mut data = vec![b'x'; 2048];
            data.push(b'a' + i);
            let oid = HashAlgo::Sha1
                .hash_object(ObjectKind::Blob, &data)
                .expect("test blob hash");
            objects.insert(oid, Object::new(ObjectKind::Blob, data));
            oids.push(oid);
        }
        (oids, objects)
    }

    fn whole_in(pack: u32) -> PackedSlot {
        PackedSlot { pack, whole: true }
    }

    #[test]
    fn rejected_pairs_need_a_whole_target_in_the_base_pack() {
        let (oids, _) = similar_blobs(3);
        let (t, b, loose) = (oids[0], oids[1], oids[2]);
        let mut slots = oid_hash_map();
        slots.insert(t, whole_in(0));
        slots.insert(
            b,
            PackedSlot {
                pack: 0,
                whole: false,
            },
        );
        assert!(already_rejected_in_pack(Some(&slots), &t, &b));
        assert!(!already_rejected_in_pack(None, &t, &b));
        assert!(!already_rejected_in_pack(Some(&slots), &b, &t));
        assert!(!already_rejected_in_pack(Some(&slots), &t, &loose));
        assert!(!already_rejected_in_pack(Some(&slots), &loose, &t));
        slots.insert(b, whole_in(1));
        assert!(!already_rejected_in_pack(Some(&slots), &t, &b));
    }

    #[test]
    fn window_search_skips_pairs_already_rejected_in_the_same_pack() {
        let (oids, objects) = similar_blobs(2);
        let (base, target) = (oids[0], oids[1]);
        let run = |packed: Option<&OidHashMap<PackedSlot>>| {
            let mut out = oid_hash_map();
            let mut encodings = HashMap::new();
            find_deltas_in_segment(
                &[base, target],
                &[],
                &objects,
                &DeltaIslands::default(),
                SegmentSearchParams {
                    window: 3,
                    max_depth: 10,
                    hash_len: 20,
                    window_memory: 0,
                    packed,
                },
                &mut out,
                &mut encodings,
                &oid_hash_map(),
            );
            out
        };
        assert_eq!(run(None).get(&target), Some(&base));

        let mut same_pack = oid_hash_map();
        same_pack.insert(base, whole_in(0));
        same_pack.insert(target, whole_in(0));
        assert!(run(Some(&same_pack)).is_empty());

        let mut other_pack = same_pack.clone();
        other_pack.insert(target, whole_in(1));
        assert_eq!(run(Some(&other_pack)).get(&target), Some(&base));

        let mut target_loose = same_pack;
        target_loose.remove(&target);
        assert_eq!(run(Some(&target_loose)).get(&target), Some(&base));
    }

    #[test]
    fn send_order_deltas_try_only_the_last_window_same_path_blobs() {
        let (oids, objects) = similar_blobs(4);
        let mut name_hashes = oid_hash_map();
        for oid in &oids {
            name_hashes.insert(*oid, 7);
        }
        let skip = oid_hash_set();
        let run = |window: usize, packed: Option<&OidHashMap<PackedSlot>>| {
            let mut edges = oid_hash_map();
            let mut encodings = HashMap::new();
            extend_send_order_blob_deltas(
                SendOrderDeltaInput {
                    send: &oids,
                    objects: &objects,
                    blob_name_hashes: &name_hashes,
                    islands: &DeltaIslands::default(),
                    skip_targets: &skip,
                    packed,
                    window,
                    hash_len: 20,
                },
                &mut edges,
                &mut encodings,
            );
            let _ = encodings;
            edges
        };
        let edges = run(1, None);
        assert_eq!(edges.len(), 3);
        for pair in oids.windows(2) {
            assert_eq!(edges.get(&pair[1]), Some(&pair[0]));
        }
        assert!(run(0, None).is_empty());

        let mut slots = oid_hash_map();
        for oid in &oids[..3] {
            slots.insert(*oid, whole_in(0));
        }
        let edges = run(10, Some(&slots));
        assert_eq!(edges.len(), 1, "only the loose blob is a target: {edges:?}");
        assert!(edges.contains_key(&oids[3]));
    }

    #[test]
    fn single_thread_prepare_matches_entry_count() {
        let entry = PlannedEntry {
            oid: ObjectId::from_hex(&"0".repeat(40)).unwrap(),
            kind: ObjectKind::Blob,
            data: b"hello".to_vec(),
            base: None,
            reused_delta: None,
            reused_delta_zlib: None,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = Odb::new(&objects);
        let opts = PackBuildOptions::default();
        let payloads = oid_hash_map();
        let got = prepare_batch(&odb, &[entry], &payloads, &opts, Compression::default())
            .expect("prepare");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn streamed_pack_is_identical_across_worker_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = Odb::new(&objects);
        let entries: Vec<PlannedEntry> = (0..3000u32)
            .map(|i| {
                let data = format!("blob number {i}\n").into_bytes();
                PlannedEntry {
                    oid: odb.hash(ObjectKind::Blob, &data).unwrap(),
                    kind: ObjectKind::Blob,
                    data,
                    base: None,
                    reused_delta: None,
                    reused_delta_zlib: None,
                }
            })
            .collect();
        let payloads = oid_hash_map();
        let opts = PackBuildOptions::default();
        let write = |threads: usize| {
            let mut out = Vec::new();
            let mut rows = Vec::new();
            let stats = write_pack_with_deltas(
                &odb,
                &entries,
                &payloads,
                &opts,
                Some(threads),
                &mut out,
                Some(&mut rows),
            )
            .unwrap();
            (out, stats, rows)
        };
        let (serial, serial_stats, serial_rows) = write(1);
        let (parallel, parallel_stats, parallel_rows) = write(4);
        assert_eq!(serial, parallel);
        assert_eq!(serial_stats, parallel_stats);
        assert_eq!(serial_rows, parallel_rows);
        assert_eq!(serial_stats.object_count, 3000);
        assert_eq!(serial_stats.pack_bytes, serial.len() as u64);
        let (body, trailer) = serial.split_at(serial.len() - 20);
        assert_eq!(HashAlgo::Sha1.checksum_digest(body).as_slice(), trailer);
    }

    #[test]
    fn batched_pack_stream_matches_one_batch_and_git_indexes_it() {
        let (tmp, repo, selection) = crate::pack_objects::tests::packed_selection(60);
        let odb = repo.odb();
        // A fresh-delta plan (no reuse) exercises delta indices across batch boundaries.
        let opts = PackBuildOptions {
            delta: true,
            reuse_deltas: false,
            reuse_objects: false,
            ..PackBuildOptions::default()
        };
        let plan = crate::pack_objects::plan_deltas(
            odb,
            &selection.send,
            &selection.have_closure,
            &opts,
            &crate::objects::oid_hash_map_from(&selection.name_hashes),
            None,
            selection.walk_prefetch,
        )
        .unwrap();
        assert!(plan.entries.iter().any(|e| e.base.is_some()));

        let write = |batch: usize| {
            let mut bytes = Vec::new();
            let mut rows = Vec::new();
            let stats = write_pack_in_batches(
                odb,
                &plan.entries,
                &plan.payloads,
                &opts,
                Some(2),
                &mut bytes,
                Some(&mut rows),
                batch,
            )
            .unwrap();
            (bytes, rows, stats)
        };
        let (one, one_rows, one_stats) = write(usize::MAX);
        let (batched, batched_rows, batched_stats) = write(7);
        assert_eq!(one, batched);
        assert_eq!(one_rows, batched_rows);
        assert_eq!(one_stats, batched_stats);
        assert_eq!(batched_stats.pack_bytes, batched.len() as u64);
        assert_eq!(batched_stats.object_count as usize, plan.entries.len());

        let trailer_start = batched.len() - odb.hash_algo().len();
        let mut ends: Vec<u64> = batched_rows.iter().skip(1).map(|r| r.1).collect();
        ends.push(trailer_start as u64);
        for ((oid, start, crc), end) in batched_rows.iter().zip(ends) {
            let slice = &batched[*start as usize..end as usize];
            assert_eq!(crc32fast::hash(slice), *crc, "crc of {oid}");
        }

        let pack = tmp.path().join("streamed.pack");
        std::fs::write(&pack, &batched).unwrap();
        grit_test_support::git(tmp.path(), &["index-pack", "--strict", "streamed.pack"]);
        let listed = grit_test_support::git(tmp.path(), &["verify-pack", "-v", "streamed.idx"]);
        assert!(listed.contains(&selection.send[0].to_hex()));
    }
}
