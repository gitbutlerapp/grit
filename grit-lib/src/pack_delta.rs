//! Windowed delta search and parallel pack serialization.
//!
//! [`search_windowed_blob_deltas`] implements Git-style sliding-window delta
//! selection (type/name-hash/size sort, real delta sizes via [`DeltaIndex`], depth
//! and window-memory limits). Segments are fixed from object count and window so
//! parallel worker counts do not change chosen bases.
//!
//! [`serialize_pack_with_deltas`] splits encoding into a parallel preparation stage
//! (delta bytes + deflate per planned entry) and a serial emission stage (headers,
//! OFS vs REF selection, trailer).

use std::collections::{HashMap, HashSet};
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
use crate::objects::{HashAlgo, Object, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack_objects::{
    append_pack_trailer, encode_ofs_delta_distance, encode_pack_object_header, pack_type_code,
    PackBuildOptions, PlannedEntry,
};
use crate::parallel::{
    try_par_map_force, ParallelHashError, ParallelOp, ParallelOptions, ThreadPolicy,
};

/// Try consecutive blob deltas in discovery order so bases precede targets in the pack stream.
pub(crate) fn extend_send_order_blob_deltas(
    send: &[ObjectId],
    objects: &HashMap<ObjectId, Object>,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    islands: &DeltaIslands,
    delta_to_base: &mut HashMap<ObjectId, ObjectId>,
    skip_targets: &HashSet<ObjectId>,
    hash_len: usize,
) {
    let mut recent_same_path: HashMap<u32, Vec<ObjectId>> = HashMap::new();
    for &oid in send {
        let obj = &objects[&oid];
        if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
            continue;
        }
        let path_key = blob_name_hashes.get(&oid).copied().unwrap_or(0);
        if skip_targets.contains(&oid) || delta_to_base.contains_key(&oid) {
            recent_same_path.entry(path_key).or_default().push(oid);
            continue;
        }
        let max_size = obj.data.len().saturating_div(2).saturating_sub(hash_len);
        if max_size > 0 {
            let mut best_base = None;
            let mut best_len = max_size.saturating_add(1);
            if let Some(candidates) = recent_same_path.get(&path_key) {
                for &base in candidates.iter().rev() {
                    if base == oid || !islands.in_same_island(&oid, &base) {
                        continue;
                    }
                    let base_data = &objects[&base].data;
                    let chosen =
                        if obj.data.starts_with(base_data) && obj.data.len() > base_data.len() {
                            encode_prefix_extension_delta(base_data, &obj.data).ok()
                        } else {
                            DeltaIndex::new(base_data).encode(&obj.data, max_size)
                        };
                    if let Some(delta) = chosen {
                        if delta.len() <= max_size && delta.len() < best_len {
                            best_len = delta.len();
                            best_base = Some(base);
                        }
                    }
                }
            }
            if let Some(base) = best_base {
                delta_to_base.insert(oid, base);
            }
        }
        recent_same_path.entry(path_key).or_default().push(oid);
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
    pub objects: &'a HashMap<ObjectId, Object>,
    pub blob_name_hashes: &'a HashMap<ObjectId, u32>,
    pub islands: &'a DeltaIslands,
    pub skip_targets: &'a HashSet<ObjectId>,
    pub skip_as_target: &'a HashSet<ObjectId>,
    /// Send-order / reuse deltas fixed before window search; used for warmup depth only.
    pub warmup_delta_edges: &'a HashMap<ObjectId, ObjectId>,
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
) -> Result<HashMap<ObjectId, ObjectId>> {
    let DeltaSearchInput {
        objects,
        blob_name_hashes,
        islands,
        skip_targets,
        skip_as_target,
        warmup_delta_edges,
        opts,
    } = input;
    if opts.window == 0 || opts.max_depth == 0 {
        return Ok(HashMap::new());
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
        return Ok(HashMap::new());
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
    };

    if workers <= 1 {
        let mut out = HashMap::new();
        for (start, end) in &segments {
            let warmup = segment_warmup(&sorted, *start, opts.window);
            find_deltas_in_segment(
                &sorted[*start..*end],
                &warmup,
                objects,
                islands,
                segment_params,
                &mut out,
                warmup_delta_edges,
            );
        }
        return Ok(out);
    }

    let next_segment = Arc::new(AtomicUsize::new(0));
    let segments = Arc::new(segments);
    let merged: Arc<Mutex<HashMap<ObjectId, ObjectId>>> = Arc::new(Mutex::new(HashMap::new()));

    thread::scope(|scope| {
        for _ in 0..workers {
            let merged = Arc::clone(&merged);
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
                let mut local = HashMap::new();
                find_deltas_in_segment(
                    &sorted[start..end],
                    &warmup,
                    objects,
                    islands,
                    segment_params,
                    &mut local,
                    warmup_delta_edges,
                );
                if let Ok(mut guard) = merged.lock() {
                    guard.extend(local);
                }
            });
        }
    });

    let merged = Arc::try_unwrap(merged)
        .ok()
        .and_then(|m| m.into_inner().ok())
        .unwrap_or_default();
    Ok(merged)
}

fn type_size_sort(
    a: &ObjectId,
    b: &ObjectId,
    objects: &HashMap<ObjectId, Object>,
    name_hashes: &HashMap<ObjectId, u32>,
) -> std::cmp::Ordering {
    let ha = name_hashes.get(a).copied().unwrap_or(0);
    let hb = name_hashes.get(b).copied().unwrap_or(0);
    hb.cmp(&ha)
        .then_with(|| objects[b].data.len().cmp(&objects[a].data.len()))
        .then_with(|| b.as_bytes().cmp(a.as_bytes()))
}

#[derive(Clone, Copy)]
struct SegmentSearchParams {
    window: usize,
    max_depth: usize,
    hash_len: usize,
    window_memory: u64,
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

struct WindowSlot {
    oid: Option<ObjectId>,
    data: Vec<u8>,
    depth: usize,
    base: Option<ObjectId>,
    delta_size: Option<usize>,
}

impl WindowSlot {
    fn clear(&mut self) -> usize {
        let mem = self.data.len();
        self.oid = None;
        self.data.clear();
        self.depth = 0;
        self.base = None;
        self.delta_size = None;
        mem
    }

    fn load(&mut self, oid: ObjectId, data: &[u8]) {
        self.oid = Some(oid);
        self.data = data.to_vec();
        self.depth = 0;
        self.base = None;
        self.delta_size = None;
    }
}

fn delta_chain_depth(edges: &HashMap<ObjectId, ObjectId>, oid: ObjectId) -> usize {
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

fn find_deltas_in_segment(
    list: &[ObjectId],
    warmup: &[ObjectId],
    objects: &HashMap<ObjectId, Object>,
    islands: &DeltaIslands,
    params: SegmentSearchParams,
    out: &mut HashMap<ObjectId, ObjectId>,
    prior_edges: &HashMap<ObjectId, ObjectId>,
) {
    if list.is_empty() {
        return;
    }
    let SegmentSearchParams {
        window,
        max_depth,
        hash_len,
        window_memory,
    } = params;
    let mut array: Vec<WindowSlot> = (0..window)
        .map(|_| WindowSlot {
            oid: None,
            data: Vec::new(),
            depth: 0,
            base: None,
            delta_size: None,
        })
        .collect();
    let mut index_cache: HashMap<ObjectId, Arc<DeltaIndex>> = HashMap::new();
    let mut idx = 0usize;
    let mut count = 0usize;
    let mut mem_usage = 0u64;

    for &warm_oid in warmup {
        mem_usage = mem_usage.saturating_sub(array[idx].clear() as u64);
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
        mem_usage = mem_usage.saturating_sub(array[idx].clear() as u64);
        array[idx].load(entry_oid, &objects[&entry_oid].data);
        mem_usage = mem_usage.saturating_add(array[idx].data.len() as u64);

        while window_memory > 0 && mem_usage > window_memory && count > 1 {
            let tail = (idx + window - count) % window;
            mem_usage = mem_usage.saturating_sub(array[tail].clear() as u64);
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
            let src_depth = array[other_idx].depth;
            let src_data = array[other_idx].data.clone();
            let ret = try_delta_pair(
                &mut array[idx],
                TryDeltaContext {
                    src_oid,
                    src_depth,
                    src_data: &src_data,
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
                out.insert(entry_oid, base_oid);
                continue;
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

fn rotate_best_base(slots: &mut [WindowSlot], idx: usize, best_base: usize, window: usize) {
    let swap = std::mem::replace(
        &mut slots[best_base],
        WindowSlot {
            oid: None,
            data: Vec::new(),
            depth: 0,
            base: None,
            delta_size: None,
        },
    );
    let mut dst = best_base;
    let dist = (window + idx - best_base) % window;
    for _ in 0..dist {
        let src = (dst + 1) % window;
        slots[dst] = std::mem::replace(
            &mut slots[src],
            WindowSlot {
                oid: None,
                data: Vec::new(),
                depth: 0,
                base: None,
                delta_size: None,
            },
        );
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

fn try_delta_pair(
    trg: &mut WindowSlot,
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
        let delta = match encode_prefix_extension_delta(src_data, &trg.data) {
            Ok(d) => d,
            Err(_) => return 0,
        };
        if delta.len() > max_size {
            return 0;
        }
        if let Some(prev) = trg.delta_size {
            if delta.len() == prev && src_depth + 1 >= trg.depth {
                return 0;
            }
        }
        trg.base = Some(src_oid);
        trg.depth = src_depth + 1;
        trg.delta_size = Some(delta.len());
        return 1;
    }

    let index = index_cache.entry(src_oid).or_insert_with(|| {
        *mem_usage = mem_usage.saturating_add((src_data.len() * 2) as u64);
        Arc::new(DeltaIndex::new(src_data))
    });

    let Some(delta) = index.encode(&trg.data, max_size) else {
        return 0;
    };
    if let Some(prev) = trg.delta_size {
        if delta.len() == prev && src_depth + 1 >= trg.depth {
            return 0;
        }
    }

    trg.base = Some(src_oid);
    trg.depth = src_depth + 1;
    trg.delta_size = Some(delta.len());
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

/// Serialize a delta plan into a PACK v2 stream.
pub(crate) fn serialize_pack_with_deltas(
    odb: &Odb,
    entries: &[PlannedEntry],
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
) -> Result<Vec<u8>> {
    let par_opts = pack_write_parallel_options(odb, write_threads)?;
    let prepared = prepare_entries_parallel(odb, entries, opts, &par_opts)?;
    write_prepared_pack(odb, entries, &prepared, opts)
}

fn prepare_entries_parallel(
    odb: &Odb,
    entries: &[PlannedEntry],
    opts: &PackBuildOptions,
    par_opts: &ParallelOptions,
) -> Result<Vec<PreparedEntry>> {
    let payloads: HashMap<ObjectId, &[u8]> =
        entries.iter().map(|e| (e.oid, e.data.as_slice())).collect();
    let delta_indices = Arc::new(build_delta_indices(odb, entries, &payloads)?);

    let prepare = |entry: &PlannedEntry| -> Result<PreparedEntry> {
        prepare_one_entry(odb, entry, &payloads, &delta_indices, opts)
    };

    if par_opts.threads.get() <= 1 {
        entries.iter().map(prepare).collect()
    } else {
        try_par_map_force(entries, *par_opts, prepare).map_err(|e| match e {
            ParallelHashError::Task(err) => err,
        })
    }
}

fn build_delta_indices(
    odb: &Odb,
    entries: &[PlannedEntry],
    payloads: &HashMap<ObjectId, &[u8]>,
) -> Result<HashMap<ObjectId, Arc<DeltaIndex>>> {
    let mut map: HashMap<ObjectId, Arc<DeltaIndex>> = HashMap::new();
    for entry in entries {
        let Some(base_oid) = entry.base else {
            continue;
        };
        if entry.reused_delta_zlib.is_some() || entry.reused_delta.is_some() {
            continue;
        }
        let base_data: Vec<u8> = if let Some(d) = payloads.get(&base_oid) {
            (*d).to_vec()
        } else {
            odb.read(&base_oid)?.data
        };
        if entry.data.starts_with(&base_data) && entry.data.len() > base_data.len() {
            continue;
        }
        map.entry(base_oid).or_insert_with(|| {
            let base_ref = payloads
                .get(&base_oid)
                .copied()
                .unwrap_or(base_data.as_slice());
            Arc::new(DeltaIndex::new(base_ref))
        });
    }
    Ok(map)
}

fn prepare_one_entry(
    odb: &Odb,
    entry: &PlannedEntry,
    payloads: &HashMap<ObjectId, &[u8]>,
    delta_indices: &HashMap<ObjectId, Arc<DeltaIndex>>,
    opts: &PackBuildOptions,
) -> Result<PreparedEntry> {
    match entry.base {
        None => prepare_whole(odb, entry, opts),
        Some(base_oid) => prepare_delta(odb, entry, base_oid, payloads, delta_indices, opts),
    }
}

fn prepare_whole(
    odb: &Odb,
    entry: &PlannedEntry,
    opts: &PackBuildOptions,
) -> Result<PreparedEntry> {
    if opts.reuse_objects && odb.hash_algo() == HashAlgo::Sha1 {
        if let Some(raw) = crate::pack::packed_full_object_slice(odb.objects_dir(), &entry.oid)? {
            return Ok(PreparedEntry {
                body: PreparedBody::Raw(raw.to_vec()),
                payload_len: 0,
                whole_kind: None,
            });
        }
    }
    let zlib = zlib_compress(&entry.data)?;
    Ok(PreparedEntry {
        body: PreparedBody::Zlib(zlib),
        payload_len: entry.data.len(),
        whole_kind: Some(entry.kind),
    })
}

fn prepare_delta(
    odb: &Odb,
    entry: &PlannedEntry,
    base_oid: ObjectId,
    payloads: &HashMap<ObjectId, &[u8]>,
    delta_indices: &HashMap<ObjectId, Arc<DeltaIndex>>,
    _opts: &PackBuildOptions,
) -> Result<PreparedEntry> {
    if let Some((delta_size, pack_bytes, zlib_start, zlib_end)) = &entry.reused_delta_zlib {
        let delta_len = usize::try_from(*delta_size)
            .map_err(|_| Error::CorruptObject("reused delta size exceeds usize".to_owned()))?;
        let zlib = pack_bytes[*zlib_start..*zlib_end].to_vec();
        return Ok(PreparedEntry {
            body: PreparedBody::Zlib(zlib),
            payload_len: delta_len,
            whole_kind: None,
        });
    }

    let delta = if let Some(reused) = &entry.reused_delta {
        reused.clone()
    } else {
        let base_data: Vec<u8> = if let Some(d) = payloads.get(&base_oid) {
            d.to_vec()
        } else {
            odb.read(&base_oid)?.data
        };
        if entry.data.starts_with(&base_data) && entry.data.len() > base_data.len() {
            encode_prefix_extension_delta(&base_data, &entry.data)?
        } else {
            let index = delta_indices
                .get(&base_oid)
                .ok_or_else(|| Error::CorruptObject("missing delta index for pack base".into()))?;
            index.encode(&entry.data, 0).ok_or_else(|| {
                Error::CorruptObject("failed to encode pack delta for object".into())
            })?
        }
    };

    let zlib = zlib_compress(&delta)?;
    Ok(PreparedEntry {
        body: PreparedBody::Zlib(zlib),
        payload_len: delta.len(),
        whole_kind: None,
    })
}

fn write_prepared_pack(
    odb: &Odb,
    entries: &[PlannedEntry],
    prepared: &[PreparedEntry],
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    let algo = odb.hash_algo();
    debug_assert_eq!(entries.len(), prepared.len());

    let mut buf = Vec::new();
    buf.extend_from_slice(b"PACK");
    buf.extend_from_slice(&2u32.to_be_bytes());
    let count = u32::try_from(entries.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    buf.extend_from_slice(&count.to_be_bytes());

    let mut oid_to_offset: HashMap<ObjectId, u64> = HashMap::new();

    for (entry, prep) in entries.iter().zip(prepared) {
        let start = buf.len() as u64;
        match entry.base {
            None => match &prep.body {
                PreparedBody::Raw(raw) => buf.extend_from_slice(raw),
                PreparedBody::Zlib(zlib) => {
                    let kind = prep.whole_kind.unwrap_or(entry.kind);
                    encode_pack_object_header(&mut buf, pack_type_code(kind), prep.payload_len);
                    buf.extend_from_slice(zlib);
                }
            },
            Some(base_oid) => {
                let in_pack_offset = oid_to_offset.get(&base_oid).copied();
                if let Some(base_off) = in_pack_offset.filter(|_| opts.use_ofs_delta) {
                    let dist = start.checked_sub(base_off).ok_or_else(|| {
                        Error::CorruptObject("ofs-delta distance underflow".to_owned())
                    })?;
                    encode_pack_object_header(&mut buf, 6, prep.payload_len);
                    encode_ofs_delta_distance(&mut buf, dist);
                } else {
                    encode_pack_object_header(&mut buf, 7, prep.payload_len);
                    if base_oid.as_bytes().len() != algo.len() {
                        return Err(Error::CorruptObject(
                            "ref-delta base oid width mismatch".to_owned(),
                        ));
                    }
                    buf.extend_from_slice(base_oid.as_bytes());
                }
                match &prep.body {
                    PreparedBody::Raw(raw) => buf.extend_from_slice(raw),
                    PreparedBody::Zlib(zlib) => buf.extend_from_slice(zlib),
                }
            }
        }
        oid_to_offset.insert(entry.oid, start);
    }

    append_pack_trailer(&mut buf, algo);
    Ok(buf)
}

fn zlib_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).map_err(Error::Io)?;
    enc.finish().map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use std::num::NonZeroUsize;

    #[test]
    #[ignore = "perf smoke: cargo test -p grit-lib --release --lib parallel_segment_search_scales -- --ignored --nocapture"]
    fn parallel_segment_search_scales_on_large_blob_set() {
        use std::time::Instant;

        let mut objects = HashMap::new();
        let mut blob_name_hashes = HashMap::new();
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
        let skip = HashSet::new();
        let warmup_edges = HashMap::new();
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
                    opts: &opts,
                },
                Some(threads),
            )
            .expect("search")
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
        let mut objects = HashMap::new();
        objects.insert(grand_oid, Object::new(ObjectKind::Blob, shared));
        objects.insert(base_oid, Object::new(ObjectKind::Blob, base_data));
        objects.insert(target_oid, Object::new(ObjectKind::Blob, target_data));
        let list = vec![target_oid];
        let warmup = vec![base_oid];
        let mut prior = HashMap::new();
        prior.insert(base_oid, grand_oid);
        let mut out = HashMap::new();
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
            },
            &mut out,
            &prior,
        );
        assert_eq!(out.get(&target_oid), Some(&base_oid));
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
        let par = ParallelOptions {
            threads: NonZeroUsize::MIN,
            min_items: 0,
            min_total_bytes: None,
            signed_entry_threshold: None,
        };
        let got = prepare_entries_parallel(&odb, &[entry], &opts, &par).expect("prepare");
        assert_eq!(got.len(), 1);
    }
}
