//! Negotiation-driven pack-objects: enumerate wants minus haves, delta search, stream PACK v2.
//!
//! Holds object metadata (not payloads) during planning; reads object bytes from the
//! [`Odb`] on demand when encoding.

use crate::environment::RepositoryOptions;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::delta_encode::{encode_prefix_extension_delta, DeltaIndex};
use crate::error::{Error, Result};
use crate::objects::{
    empty_object_id_hash_set, oid_hash_map, oid_hash_map_from, oid_hash_map_with_capacity,
    oid_hash_set, oid_hash_set_from, parse_commit, parse_tag, HashAlgo, Object, ObjectId,
    ObjectKind, OidHashMap, OidHashSet, TreeRefIter,
};
use crate::odb::Odb;
use crate::parallel::{try_par_map_force, ParallelHashError};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;

/// Options controlling pack-objects output (also used by [`build_pack`]).
#[derive(Clone, Copy, Debug)]
pub struct PackBuildOptions {
    /// Build a thin pack: allow deltas whose base is reachable from the `haves`
    /// but **not** emitted in the pack (`REF_DELTA` against a peer-held base).
    pub thin: bool,
    /// Emit delta-compressed objects instead of whole objects only.
    pub delta: bool,
    /// Candidate bases per object (Git `--window`). `0` disables new deltas.
    pub window: usize,
    /// Maximum delta chain depth (Git `--depth`). `0` stores whole objects.
    pub max_depth: usize,
    /// Prefer `OFS_DELTA` when the base precedes the target in the pack stream.
    pub use_ofs_delta: bool,
    /// Honor `pack.island` delta-island rules when selecting bases.
    pub respect_islands: bool,
    /// Reuse on-disk delta edges when both target and base are in this pack.
    pub reuse_deltas: bool,
    /// Reuse verbatim packed bytes for whole objects (CRC32-guarded v2 indexes).
    pub reuse_objects: bool,
    /// Use pack reachability bitmaps for object enumeration when present.
    pub use_bitmaps: bool,
    /// When set by maintenance repack, stream verbatim bytes from the sole on-disk pack
    /// (`window == 0`, zero loose objects). Not for general pack-objects callers.
    pub stream_verbatim_single_pack: bool,
}

impl Default for PackBuildOptions {
    fn default() -> Self {
        Self {
            thin: false,
            delta: false,
            window: 10,
            max_depth: 50,
            use_ofs_delta: true,
            respect_islands: false,
            reuse_deltas: true,
            reuse_objects: true,
            use_bitmaps: true,
            stream_verbatim_single_pack: false,
        }
    }
}

impl PackBuildOptions {
    /// `pack.window` and `pack.depth` from config, or Git defaults when unset.
    #[must_use]
    pub fn window_and_depth_from_config(cfg: Option<&crate::config::ConfigSet>) -> (usize, usize) {
        cfg.map(|c| (c.pack_object_window(), c.pack_object_depth()))
            .unwrap_or((10, crate::pack::DEFAULT_PACK_DEPTH))
    }

    /// Production pack settings for a local full copy (clone/fetch from disk).
    #[must_use]
    pub fn for_local_copy(cfg: Option<&crate::config::ConfigSet>) -> Self {
        let (window, max_depth) = Self::window_and_depth_from_config(cfg);
        Self {
            thin: false,
            delta: true,
            window,
            max_depth,
            use_ofs_delta: true,
            reuse_deltas: max_depth > 0,
            reuse_objects: true,
            use_bitmaps: true,
            ..Default::default()
        }
    }

    /// Full repack of an on-disk repository: reuse existing pack bytes, no new delta search.
    #[must_use]
    pub fn for_repack_rewrite(cfg: Option<&crate::config::ConfigSet>) -> Self {
        let max_depth = Self::window_and_depth_from_config(cfg).1;
        Self {
            thin: false,
            delta: true,
            window: 0,
            max_depth,
            use_ofs_delta: true,
            reuse_deltas: max_depth > 0,
            reuse_objects: true,
            use_bitmaps: true,
            ..Default::default()
        }
    }

    /// Repack when pack files and loose objects both exist: reuse on-disk deltas and run the
    /// configured delta window for new loose blobs.
    #[must_use]
    pub fn for_repack_with_loose(cfg: Option<&crate::config::ConfigSet>) -> Self {
        let (window, max_depth) = Self::window_and_depth_from_config(cfg);
        Self {
            thin: false,
            delta: true,
            window,
            max_depth,
            use_ofs_delta: true,
            reuse_deltas: max_depth > 0,
            reuse_objects: true,
            use_bitmaps: true,
            stream_verbatim_single_pack: false,
            ..Default::default()
        }
    }

    /// Production pack settings for an incremental in-process local push.
    ///
    /// Uses a thin pack when the remote already has objects to delta against.
    /// Initial pushes into an empty bare repo should use [`Self::for_local_copy`].
    #[must_use]
    pub fn for_local_push(cfg: Option<&crate::config::ConfigSet>) -> Self {
        let (window, max_depth) = Self::window_and_depth_from_config(cfg);
        Self {
            thin: true,
            delta: true,
            window,
            max_depth,
            use_ofs_delta: true,
            reuse_deltas: true,
            reuse_objects: true,
            use_bitmaps: true,
            ..Default::default()
        }
    }
}

/// Extended options for [`PackObjects`].
#[derive(Clone, Copy, Debug, Default)]
pub struct PackObjectsOptions {
    /// Core pack-objects behaviour flags.
    pub build: PackBuildOptions,
    /// Override parallel delta-search workers (`None` = resolve [`crate::parallel::ParallelOp::PackDeltaSearch`] / `pack.threads`).
    pub search_threads: Option<usize>,
    /// Override parallel pack-write workers (`None` = resolve [`crate::parallel::ParallelOp::PackWrite`] / `pack.threads` from the repo config attached to the [`Odb`]).
    pub write_threads: Option<usize>,
}

impl From<PackBuildOptions> for PackObjectsOptions {
    fn from(build: PackBuildOptions) -> Self {
        Self {
            build,
            search_threads: None,
            write_threads: None,
        }
    }
}

/// Counters reported after a successful pack write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackStats {
    /// Number of objects in the pack header.
    pub object_count: u32,
    /// Total bytes written (including header and trailing hash).
    pub pack_bytes: u64,
    /// Objects written as `OFS_DELTA` or `REF_DELTA`.
    pub delta_count: u32,
    /// Pack body bytes copied verbatim from an existing on-disk pack (reuse path).
    pub verbatim_reuse_bytes: u64,
    /// Objects emitted via verbatim pack reuse (whole or delta copy/rewrite).
    pub verbatim_reuse_objects: u32,
    /// Delta entries whose OFS base distance was rewritten for the output pack.
    pub pack_ofs_fixup_objects: u32,
    /// OFS deltas converted to REF deltas for thin packs.
    pub pack_ref_from_ofs_objects: u32,
}

/// Progress hook for long pack builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackProgress {
    /// Enumeration finished (`total` objects selected).
    Enumerated {
        /// Number of objects selected for packing.
        total: usize,
    },
    /// Delta planning finished.
    PlannedDeltas,
    /// An object was written (`written` of `total`).
    WroteObject {
        /// Objects written so far.
        written: usize,
        /// Total objects to write in this pack.
        total: usize,
    },
}

/// Streaming pack-objects builder over an [`Odb`].
pub struct PackObjects<'a> {
    odb: &'a Odb,
    source_odb: &'a Odb,
    opts: PackObjectsOptions,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    have_shallow: OidHashSet,
    source_shallow: OidHashSet,
    progress: Option<Box<dyn FnMut(PackProgress)>>,
}

impl<'a> PackObjects<'a> {
    /// Create a builder reading objects from `odb`.
    #[must_use]
    pub fn new(odb: &'a Odb, opts: PackObjectsOptions) -> Self {
        Self {
            odb,
            source_odb: odb,
            opts,
            wants: Vec::new(),
            haves: Vec::new(),
            have_shallow: oid_hash_set(),
            source_shallow: oid_hash_set(),
            progress: None,
        }
    }

    /// Read object payloads from a different ODB (local fetch: remote source, local haves).
    #[must_use]
    pub fn with_source_odb(mut self, source: &'a Odb) -> Self {
        self.source_odb = source;
        self
    }

    /// Objects that must be included in the pack.
    #[must_use]
    pub fn wants(mut self, wants: &[ObjectId]) -> Self {
        self.wants.extend_from_slice(wants);
        self
    }

    /// Objects already held by the peer (negotiation boundary).
    #[must_use]
    pub fn haves(mut self, haves: &[ObjectId]) -> Self {
        self.haves.extend_from_slice(haves);
        self
    }

    /// Shallow grafts on the have side (local fetch).
    #[must_use]
    pub fn have_shallow_grafts(mut self, grafts: &HashSet<ObjectId>) -> Self {
        self.have_shallow = grafts.iter().copied().collect();
        self
    }

    /// Shallow grafts on the source side (do not walk past on source).
    #[must_use]
    pub fn source_shallow_grafts(mut self, grafts: &HashSet<ObjectId>) -> Self {
        self.source_shallow = grafts.iter().copied().collect();
        self
    }

    /// Optional progress callback.
    #[must_use]
    pub fn progress<F>(mut self, f: F) -> Self
    where
        F: FnMut(PackProgress) + 'static,
    {
        self.progress = Some(Box::new(f));
        self
    }

    fn fire(&mut self, ev: PackProgress) {
        if let Some(ref mut f) = self.progress {
            f(ev);
        }
    }

    /// Write a PACK v2 stream to `w` and return statistics.
    ///
    /// # Errors
    ///
    /// Missing wants, corrupt objects, or I/O failures while writing.
    pub fn write_to(mut self, w: &mut dyn Write) -> Result<PackStats> {
        let opts = self.opts.build;
        let have_shallow: HashSet<ObjectId> = self.have_shallow.iter().copied().collect();
        let source_shallow: HashSet<ObjectId> = self.source_shallow.iter().copied().collect();
        let have_closure = crate::pack_object_select::reachable_closure_walk(
            self.odb,
            &self.haves,
            &HashSet::new(),
            true,
            &have_shallow,
        )?;
        let selection = crate::pack_object_select::enumerate_pack_objects(
            self.source_odb,
            &self.wants,
            &self.haves,
            &crate::pack_object_select::PackEnumerateOptions {
                filter: None,
                shallow_grafts: &source_shallow,
                have_shallow_grafts: Some(&have_shallow),
                use_bitmaps: opts.use_bitmaps,
                exclude_objects: Some(&have_closure),
            },
        )?;
        let send = selection.send;
        self.fire(PackProgress::Enumerated { total: send.len() });
        let bytes = if !opts.delta {
            serialize_pack(self.source_odb, &send, &opts)?
        } else {
            let plan = plan_deltas(
                self.source_odb,
                &send,
                &have_closure,
                &opts,
                &oid_hash_map_from(&selection.blob_name_hashes),
                self.opts.search_threads,
                selection.walk_prefetch,
            )?;
            self.fire(PackProgress::PlannedDeltas);
            let delta_count = plan.entries.iter().filter(|e| e.base.is_some()).count() as u32;
            let pack = crate::pack_delta::serialize_pack_with_deltas(
                self.source_odb,
                &plan.entries,
                &plan.payloads,
                &opts,
                self.opts.write_threads,
            )?;
            w.write_all(&pack.bytes).map_err(Error::Io)?;
            let count =
                u32::from_be_bytes([pack.bytes[8], pack.bytes[9], pack.bytes[10], pack.bytes[11]]);
            return Ok(PackStats {
                object_count: count,
                pack_bytes: pack.bytes.len() as u64,
                delta_count,
                ..Default::default()
            });
        };
        w.write_all(&bytes).map_err(Error::Io)?;
        let count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Ok(PackStats {
            object_count: count,
            pack_bytes: bytes.len() as u64,
            delta_count: 0,
            ..Default::default()
        })
    }
}

/// Compute the full object closure reachable from `roots`, stopping descent into
/// any object already present in `stop`.
pub fn reachable_closure(
    odb: &Odb,
    roots: &[ObjectId],
    stop: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let stop = oid_hash_set_from(stop);
    let shallow_grafts = oid_hash_set_from(shallow_grafts);
    let mut seen = oid_hash_set();
    let order = collect_reachable_excluding(odb, roots, &stop, skip_missing, &shallow_grafts)?;
    for oid in order {
        seen.insert(oid);
    }
    Ok(seen.into_iter().collect())
}

/// Build a v2 packfile containing exactly the objects reachable from `wants`
/// but **not** reachable from `haves`, de-duplicated.
pub fn build_pack(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    build_pack_with_shallow_and_filter(
        odb,
        wants,
        haves,
        empty_object_id_hash_set(),
        empty_object_id_hash_set(),
        None,
        opts,
    )
}

/// Build a pack for upload-pack with optional shallow grafts and object filter.
///
/// When `filter` is set, object selection follows `rev-list --objects --filter`
/// (same path as filtered bundles).
pub fn build_pack_with_shallow_and_filter(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    have_shallow: &HashSet<ObjectId>,
    source_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    let selection = crate::pack_object_select::enumerate_pack_objects(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter,
            shallow_grafts: source_shallow,
            have_shallow_grafts: Some(have_shallow),
            use_bitmaps: opts.use_bitmaps,
            exclude_objects: None,
        },
    )?;
    build_pack_from_send_list(
        odb,
        &selection.send,
        &selection.have_closure,
        opts,
        &selection.blob_name_hashes,
    )
}

/// Stream a pack for upload-pack: pack reuse when bitmaps allow, otherwise pack-objects.
#[allow(clippy::too_many_arguments)]
pub fn write_upload_pack_stream(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
    progress: &mut dyn FnMut(PackProgress),
    w: &mut dyn Write,
) -> Result<PackStats> {
    if filter.is_some() {
        let pack = build_pack_with_shallow_and_filter(
            repo.odb(),
            wants,
            haves,
            have_shallow,
            source_shallow,
            filter,
            opts,
        )?;
        let len = pack.len() as u64;
        w.write_all(&pack).map_err(Error::Io)?;
        return Ok(PackStats {
            pack_bytes: len,
            ..PackStats::default()
        });
    }
    if opts.use_bitmaps {
        if let Some(stats) = crate::pack_reuse::try_write_streaming_pack(
            repo,
            wants,
            haves,
            source_shallow,
            have_shallow,
            filter,
            opts,
            progress,
            w,
        )? {
            return Ok(stats);
        }
    }
    let _ = progress;
    PackObjects::new(repo.odb(), PackObjectsOptions::from(*opts))
        .wants(wants)
        .haves(haves)
        .source_shallow_grafts(source_shallow)
        .have_shallow_grafts(have_shallow)
        .write_to(w)
}

fn write_pack_header(w: &mut dyn Write, count: u32) -> Result<()> {
    w.write_all(b"PACK").map_err(Error::Io)?;
    w.write_all(&2u32.to_be_bytes()).map_err(Error::Io)?;
    w.write_all(&count.to_be_bytes()).map_err(Error::Io)?;
    Ok(())
}

/// Stream a delta pack from a plan, optionally applying bitmap verbatim reuse.
pub(crate) fn write_delta_pack_stream(
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    w: &mut dyn Write,
    reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
) -> Result<PackStats> {
    let (pack_bytes_len, reuse_stats) = if let Some(reuse) = reuse {
        let pack = serialize_delta_plan_stream(odb, plan, opts, Some(reuse))?;
        let len = pack.bytes.len() as u64;
        w.write_all(&pack.bytes).map_err(Error::Io)?;
        (len, reuse.take_stats())
    } else {
        let pack = serialize_delta_plan_stream(odb, plan, opts, None)?;
        let len = pack.bytes.len() as u64;
        w.write_all(&pack.bytes).map_err(Error::Io)?;
        (len, crate::pack_reuse::ReuseWriteStats::default())
    };
    let delta_count = plan.entries.iter().filter(|e| e.base.is_some()).count() as u32;
    Ok(PackStats {
        object_count: u32::try_from(plan.entries.len())
            .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?,
        pack_bytes: pack_bytes_len,
        delta_count,
        verbatim_reuse_bytes: reuse_stats.verbatim_reuse_bytes,
        verbatim_reuse_objects: reuse_stats.verbatim_reuse_objects,
        pack_ofs_fixup_objects: reuse_stats.pack_ofs_fixup_objects,
        pack_ref_from_ofs_objects: reuse_stats.pack_ref_from_ofs_objects,
    })
}

fn write_delta_plan_stream_body(
    w: &mut dyn Write,
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
) -> Result<Vec<(ObjectId, u64, u32)>> {
    let count = u32::try_from(plan.entries.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    write_pack_header(w, count)?;
    let mut index_rows = Vec::with_capacity(plan.entries.len());
    write_planned_entries(w, odb, plan, opts, 12, reuse, &mut index_rows)?;
    Ok(index_rows)
}

/// Serialize a delta plan to an in-memory PACK v2 stream with a matching `.idx` row set.
pub(crate) fn serialize_delta_plan_stream(
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
) -> Result<SerializedDeltaPack> {
    let mut buf = Vec::new();
    let index_rows = write_delta_plan_stream_body(&mut buf, odb, plan, opts, reuse)?;
    append_pack_trailer(&mut buf, odb.hash_algo());
    Ok(SerializedDeltaPack {
        bytes: buf,
        index_rows,
        staging_path: None,
    })
}

/// Like [`serialize_delta_plan_stream`], but writes the pack to `staging_path` instead of memory.
pub(crate) fn serialize_delta_plan_stream_to_path(
    staging_path: &std::path::Path,
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
) -> Result<SerializedDeltaPack> {
    use std::fs::File;
    use std::io::BufWriter;

    use crate::hash::HashingWriter;

    let file = File::create(staging_path).map_err(Error::Io)?;
    let mut hashed = HashingWriter::new(odb.hash_algo(), BufWriter::new(file));
    let index_rows = write_delta_plan_stream_body(&mut hashed, odb, plan, opts, reuse)?;
    let (mut inner, _digest) = hashed.finish().map_err(Error::Io)?;
    inner.flush().map_err(Error::Io)?;
    Ok(SerializedDeltaPack {
        bytes: Vec::new(),
        index_rows,
        staging_path: Some(staging_path.to_path_buf()),
    })
}

fn write_planned_entries(
    w: &mut dyn Write,
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    pack_body_start: u64,
    mut reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
    index_rows: &mut Vec<(ObjectId, u64, u32)>,
) -> Result<()> {
    let algo = odb.hash_algo();
    let mut oid_to_offset: OidHashMap<u64> = oid_hash_map();
    let mut delta_index_by_base: OidHashMap<DeltaIndex> = oid_hash_map();
    let mut body_written = 0u64;

    for entry in &plan.entries {
        let start = pack_body_start + body_written;
        let mut chunk = Vec::new();
        if entry.base.is_none() {
            if let Some(reuse) = reuse.as_mut() {
                if let Some(bytes) = reuse.try_emit_whole(&entry.oid, start, opts)? {
                    chunk = bytes;
                }
            }
            if chunk.is_empty() {
                let payload = planned_object_payload(odb, entry.oid, entry, &plan.payloads)?;
                write_whole_pack_object(
                    &mut chunk,
                    odb,
                    entry.oid,
                    entry.kind,
                    payload.as_ref(),
                    opts,
                )?;
            }
            w.write_all(&chunk).map_err(Error::Io)?;
            index_rows.push((entry.oid, start, crc32fast::hash(&chunk)));
            body_written += chunk.len() as u64;
            oid_to_offset.insert(entry.oid, start);
            continue;
        }
        let Some(base_oid) = entry.base else {
            continue;
        };
        let in_pack_offset = oid_to_offset.get(&base_oid).copied();
        if let Some(reuse) = reuse.as_mut() {
            if let Some(bytes) =
                reuse.try_emit_delta(&entry.oid, start, in_pack_offset, opts, &oid_to_offset)?
            {
                chunk = bytes;
            }
        }
        if chunk.is_empty() {
            if let Some((delta_size, pack_bytes, zlib_start, zlib_end)) = &entry.reused_delta_zlib {
                let delta_len = usize::try_from(*delta_size).map_err(|_| {
                    Error::CorruptObject("reused delta size exceeds usize".to_owned())
                })?;
                if let Some(base_off) = in_pack_offset.filter(|_| opts.use_ofs_delta) {
                    let dist = start.checked_sub(base_off).ok_or_else(|| {
                        Error::CorruptObject("ofs-delta distance underflow".to_owned())
                    })?;
                    encode_pack_object_header(&mut chunk, 6, delta_len);
                    encode_ofs_delta_distance(&mut chunk, dist);
                    if let Some(reuse) = reuse.as_mut() {
                        reuse.record_ofs_fixup();
                    }
                } else {
                    encode_pack_object_header(&mut chunk, 7, delta_len);
                    chunk.extend_from_slice(base_oid.as_bytes());
                }
                chunk.extend_from_slice(&pack_bytes[*zlib_start..*zlib_end]);
                if let Some(reuse) = reuse.as_mut() {
                    reuse.record_verbatim_zlib((zlib_end - zlib_start) as u64);
                }
            } else {
                let target_data = planned_object_payload(odb, entry.oid, entry, &plan.payloads)?;
                let delta = if let Some(reused) = &entry.reused_delta {
                    reused.clone()
                } else {
                    let base_data = planned_object_payload(odb, base_oid, entry, &plan.payloads)?;
                    if target_data.starts_with(base_data.as_ref())
                        && target_data.len() > base_data.len()
                    {
                        encode_prefix_extension_delta(base_data.as_ref(), &target_data)?
                    } else {
                        delta_index_by_base
                            .entry(base_oid)
                            .or_insert_with(|| DeltaIndex::new(base_data.as_ref()))
                            .encode(&target_data, 0)
                            .ok_or_else(|| {
                                Error::CorruptObject(
                                    "failed to encode pack delta for object".into(),
                                )
                            })?
                    }
                };
                if let Some(base_off) = in_pack_offset.filter(|_| opts.use_ofs_delta) {
                    let dist = start.checked_sub(base_off).ok_or_else(|| {
                        Error::CorruptObject("ofs-delta distance underflow".to_owned())
                    })?;
                    encode_pack_object_header(&mut chunk, 6, delta.len());
                    encode_ofs_delta_distance(&mut chunk, dist);
                } else {
                    encode_pack_object_header(&mut chunk, 7, delta.len());
                    if base_oid.as_bytes().len() != algo.len() {
                        return Err(Error::CorruptObject(
                            "ref-delta base oid width mismatch".to_owned(),
                        ));
                    }
                    chunk.extend_from_slice(base_oid.as_bytes());
                }
                write_zlib(&mut chunk, &delta)?;
            }
        }
        w.write_all(&chunk).map_err(Error::Io)?;
        index_rows.push((entry.oid, start, crc32fast::hash(&chunk)));
        body_written += chunk.len() as u64;
        oid_to_offset.insert(entry.oid, start);
    }
    Ok(())
}

/// Serialize a precomputed object list into a pack (used by [`build_pack`] and filtered bundles).
///
/// # Errors
///
/// Same as [`build_pack`].
pub fn build_pack_from_send_list(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
) -> Result<Vec<u8>> {
    build_pack_from_send_list_with_parallel_threads(
        odb,
        send,
        have_closure,
        opts,
        blob_name_hashes,
        None,
        None,
        HashMap::new(),
    )
}

/// Like [`build_pack`] with an explicit pack-write worker override for tests and embedders.
///
/// `write_threads` replaces config-derived `pack.threads` when set.
pub fn build_pack_with_write_threads(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
) -> Result<Vec<u8>> {
    build_pack_with_write_threads_full(odb, wants, haves, opts, write_threads)
        .map(|pack| pack.bytes)
}

/// Like [`build_pack_with_write_threads`], retaining trusted pack index rows for local install.
pub(crate) fn build_pack_with_write_threads_full(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
) -> Result<SerializedDeltaPack> {
    let empty_shallow = empty_object_id_hash_set();
    let have_closure = reachable_closure(odb, haves, empty_shallow, true, empty_shallow)?;
    let selection = crate::pack_object_select::enumerate_pack_objects(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter: None,
            shallow_grafts: empty_shallow,
            have_shallow_grafts: Some(empty_shallow),
            use_bitmaps: opts.use_bitmaps,
            exclude_objects: Some(&have_closure),
        },
    )?;
    build_pack_from_send_list_full(
        odb,
        &selection.send,
        &selection.have_closure,
        opts,
        &selection.blob_name_hashes,
        None,
        write_threads,
        selection.walk_prefetch,
    )
}

/// Like [`build_pack`] with explicit delta-search and pack-write worker overrides.
pub fn build_pack_with_parallel_threads(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
    search_threads: Option<usize>,
    write_threads: Option<usize>,
) -> Result<Vec<u8>> {
    let empty_shallow = empty_object_id_hash_set();
    let have_closure = reachable_closure(odb, haves, empty_shallow, true, empty_shallow)?;
    let selection = crate::pack_object_select::enumerate_pack_objects(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter: None,
            shallow_grafts: empty_shallow,
            have_shallow_grafts: Some(empty_shallow),
            use_bitmaps: opts.use_bitmaps,
            exclude_objects: Some(&have_closure),
        },
    )?;
    build_pack_from_send_list_with_parallel_threads(
        odb,
        &selection.send,
        &selection.have_closure,
        opts,
        &selection.blob_name_hashes,
        search_threads,
        write_threads,
        selection.walk_prefetch,
    )
}

/// Serialize a precomputed object list with an explicit pack-write worker count.
pub fn build_pack_from_send_list_with_write_threads(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    write_threads: Option<usize>,
) -> Result<Vec<u8>> {
    build_pack_from_send_list_with_parallel_threads(
        odb,
        send,
        have_closure,
        opts,
        blob_name_hashes,
        None,
        write_threads,
        HashMap::new(),
    )
}

/// Serialize a precomputed object list with explicit delta-search and write parallelism.
#[allow(clippy::too_many_arguments)]
pub fn build_pack_from_send_list_with_parallel_threads(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    search_threads: Option<usize>,
    write_threads: Option<usize>,
    walk_prefetch: HashMap<ObjectId, Object>,
) -> Result<Vec<u8>> {
    build_pack_from_send_list_full(
        odb,
        send,
        have_closure,
        opts,
        blob_name_hashes,
        search_threads,
        write_threads,
        walk_prefetch,
    )
    .map(|pack| pack.bytes)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_pack_from_send_list_full(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    search_threads: Option<usize>,
    write_threads: Option<usize>,
    walk_prefetch: HashMap<ObjectId, Object>,
) -> Result<SerializedDeltaPack> {
    if !opts.delta {
        let bytes = serialize_pack(odb, send, opts)?;
        return Ok(SerializedDeltaPack {
            bytes,
            index_rows: Vec::new(),
            staging_path: None,
        });
    }
    let blob_name_hashes_oid = oid_hash_map_from(blob_name_hashes);
    let plan = plan_deltas(
        odb,
        send,
        have_closure,
        opts,
        &blob_name_hashes_oid,
        search_threads,
        walk_prefetch,
    )?;
    let lazy_stream = opts.window == 0 && !opts.thin && opts.reuse_deltas && opts.reuse_objects;
    if lazy_stream && opts.stream_verbatim_single_pack {
        if let Some(mut reuse) = crate::pack_reuse::try_streaming_reuse_for_repack(odb, send)? {
            let objects_dir = odb
                .files_objects_dir()
                .ok_or(Error::UnsupportedObjectStore {
                    operation: "pack_stream_staging",
                })?;
            let pack_dir = objects_dir.join("pack");
            std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
            static STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let staging = pack_dir.join(format!("tmp_repack_stream_{}_{seq}", std::process::id()));
            let pack =
                serialize_delta_plan_stream_to_path(&staging, odb, &plan, opts, Some(&mut reuse))?;
            return Ok(pack);
        }
    }
    crate::pack_delta::serialize_pack_with_deltas(
        odb,
        &plan.entries,
        &plan.payloads,
        opts,
        write_threads,
    )
}

/// Build a pack for local fetch: enumerate haves on `have_odb`, read payloads from `source_odb`.
pub(crate) fn build_pack_for_local_fetch(
    source_odb: &Odb,
    wants: &[ObjectId],
    have_odb: &Odb,
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    PackObjects::new(have_odb, PackObjectsOptions::from(*opts))
        .with_source_odb(source_odb)
        .wants(wants)
        .haves(haves)
        .have_shallow_grafts(have_shallow)
        .source_shallow_grafts(source_shallow)
        .write_to(&mut buf)?;
    Ok(buf)
}

fn collect_reachable_excluding(
    odb: &Odb,
    roots: &[ObjectId],
    exclude: &OidHashSet,
    skip_missing: bool,
    shallow_grafts: &OidHashSet,
) -> Result<Vec<ObjectId>> {
    let mut visited: OidHashSet = oid_hash_set();
    let mut ordered: Vec<ObjectId> = Vec::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    let enqueue = |oid: ObjectId,
                   queue: &mut VecDeque<ObjectId>,
                   visited: &mut OidHashSet,
                   ordered: &mut Vec<ObjectId>|
     -> bool {
        if !visited.insert(oid) {
            return false;
        }
        if !exclude.contains(&oid) {
            ordered.push(oid);
        }
        queue.push_back(oid);
        true
    };

    for &root in roots {
        enqueue(root, &mut queue, &mut visited, &mut ordered);
    }

    let oid_len = odb.hash_algo().len();

    while let Some(oid) = queue.pop_front() {
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            // A root/have absent from this odb cannot be traversed; with
            // `skip_missing` it simply contributes nothing (no descent), instead
            // of failing the whole pack build.
            Err(_) if skip_missing => continue,
            Err(e) => return Err(e),
        };
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                if !shallow_grafts.contains(&oid) {
                    for parent in commit.parents {
                        enqueue(parent, &mut queue, &mut visited, &mut ordered);
                    }
                }
                enqueue(commit.tree, &mut queue, &mut visited, &mut ordered);
            }
            ObjectKind::Tree => {
                for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                    let entry = entry?;
                    // Skip submodule (gitlink) entries: the commit they name
                    // lives in another object store and is not part of this pack.
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    enqueue(entry.oid, &mut queue, &mut visited, &mut ordered);
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                enqueue(tag.object, &mut queue, &mut visited, &mut ordered);
            }
            ObjectKind::Blob => {}
        }
    }

    Ok(ordered)
}

/// The pack object type code for a Git object kind (PACK v2 base types).
pub(crate) fn pack_type_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

/// Append a PACK object header: 3-bit type + variable-length size (little-endian
/// 7-bit groups, MSB = continuation). Lifted from the CLI pack writer.
pub(crate) fn encode_pack_object_header(buf: &mut Vec<u8>, type_code: u8, payload_len: usize) {
    let mut size = payload_len;
    let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
    size >>= 4;
    if size > 0 {
        buf.push(first | 0x80);
        while size > 0 {
            let b = (size & 0x7f) as u8;
            size >>= 7;
            buf.push(if size > 0 { b | 0x80 } else { b });
        }
    } else {
        buf.push(first);
    }
}

/// Serialize `oids` as a PACK v2 stream of whole (non-delta) objects, terminated
/// by the trailing pack checksum at the repository hash width.
fn serialize_pack(odb: &Odb, oids: &[ObjectId], opts: &PackBuildOptions) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"PACK");
    buf.extend_from_slice(&2u32.to_be_bytes());
    let count = u32::try_from(oids.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    buf.extend_from_slice(&count.to_be_bytes());

    for oid in oids {
        let obj = odb.read(oid)?;
        write_whole_pack_object(&mut buf, odb, *oid, obj.kind, &obj.data, opts)?;
    }

    append_pack_trailer(&mut buf, odb.hash_algo());
    Ok(buf)
}

/// Write one whole (non-delta) pack object, reusing on-disk bytes when allowed.
fn write_whole_pack_object(
    buf: &mut Vec<u8>,
    odb: &Odb,
    oid: ObjectId,
    kind: ObjectKind,
    data: &[u8],
    opts: &PackBuildOptions,
) -> Result<()> {
    if opts.reuse_objects && odb.hash_algo() == HashAlgo::Sha1 {
        if let Some(raw) = crate::pack::packed_full_object_slice(odb.objects_dir(), &oid)? {
            buf.extend_from_slice(&raw);
            return Ok(());
        }
    }
    encode_pack_object_header(buf, pack_type_code(kind), data.len());
    write_zlib(buf, data)
}

/// Append the trailing pack checksum: the hash of everything written so far, at
/// the repository's hash width (SHA-1 → 20 bytes, SHA-256 → 32 bytes).
pub(crate) fn append_pack_trailer(buf: &mut Vec<u8>, algo: HashAlgo) {
    buf.extend_from_slice(&algo.checksum_digest(&*buf));
}

/// Append whole objects from `odb` to an in-progress pack (header + body, no trailer).
pub(crate) fn append_whole_objects_from_odb(
    buf: &mut Vec<u8>,
    odb: &Odb,
    oids: &[ObjectId],
) -> Result<()> {
    let opts = PackBuildOptions::default();
    for oid in oids {
        let obj = odb.read(oid)?;
        write_whole_pack_object(buf, odb, *oid, obj.kind, &obj.data, &opts)?;
    }
    Ok(())
}

/// A single object to write, either whole or as a delta against a chosen base.
pub(crate) struct PlannedEntry {
    pub(crate) oid: ObjectId,
    pub(crate) kind: ObjectKind,
    /// Object payload, kept so the serializer can re-hash/compress without a
    /// second odb read.
    pub(crate) data: Vec<u8>,
    /// `Some(base_oid)` when this entry is a (blob) delta against `base_oid`.
    /// The base may be an in-pack object or — for thin packs — an external base
    /// present only on the peer.
    pub(crate) base: Option<ObjectId>,
    /// Uncompressed delta bytes when freshly computed or reused after inflate.
    pub(crate) reused_delta: Option<Vec<u8>>,
    /// On-disk reuse: uncompressed size from the source pack header plus zlib
    /// bytes copied verbatim from a cached pack buffer (Git's `reuse_delta`).
    pub(crate) reused_delta_zlib:
        Option<(u64, std::sync::Arc<crate::pack::PackData>, usize, usize)>,
}

/// The full delta plan: the ordered entries to emit.
pub(crate) struct DeltaPlan {
    pub(crate) entries: Vec<PlannedEntry>,
    /// Blob payloads loaded during planning, shared across entries to avoid clones and rereads.
    pub(crate) payloads: OidHashMap<Arc<[u8]>>,
}

/// A delta pack built in-process, with index rows that match the emitted stream.
pub(crate) struct SerializedDeltaPack {
    pub bytes: Vec<u8>,
    pub index_rows: Vec<(ObjectId, u64, u32)>,
    /// Pack bytes on disk when set ([`Self::bytes`] may be empty).
    pub staging_path: Option<std::path::PathBuf>,
}

/// Resolve an object's uncompressed bytes for pack writing.
pub(crate) fn planned_object_payload<'a>(
    odb: &'a Odb,
    oid: ObjectId,
    entry: &'a PlannedEntry,
    plan_payloads: &'a OidHashMap<Arc<[u8]>>,
) -> Result<Cow<'a, [u8]>> {
    if entry.oid == oid && !entry.data.is_empty() {
        return Ok(Cow::Borrowed(&entry.data));
    }
    if let Some(bytes) = plan_payloads.get(&oid) {
        return Ok(Cow::Borrowed(bytes.as_ref()));
    }
    Ok(Cow::Owned(odb.read(&oid)?.data))
}

fn load_objects_for_plan(
    odb: &Odb,
    send: &[ObjectId],
    blob_only: bool,
    search_threads: Option<usize>,
    walk_prefetch: &mut HashMap<ObjectId, Object>,
) -> Result<OidHashMap<Object>> {
    let mut objects = oid_hash_map_with_capacity(send.len().max(1));
    for &oid in send {
        let Some(obj) = walk_prefetch.remove(&oid) else {
            continue;
        };
        let keep = if blob_only {
            obj.kind == ObjectKind::Blob && !obj.data.is_empty()
        } else {
            true
        };
        if keep {
            objects.insert(oid, obj);
        } else {
            walk_prefetch.insert(oid, obj);
        }
    }
    if blob_only {
        let mut blob_oids = Vec::new();
        for &oid in send {
            if objects.contains_key(&oid) {
                continue;
            }
            let info = odb.read_info(&oid)?;
            if info.kind == ObjectKind::Blob && info.size > 0 {
                blob_oids.push(oid);
            }
        }
        let par = crate::pack_delta::pack_delta_search_parallel_options(odb, search_threads)?;
        if par.threads.get() <= 1 || blob_oids.len() < 32 {
            for oid in blob_oids {
                objects.insert(oid, odb.read(&oid)?);
            }
        } else {
            let pairs =
                try_par_map_force(&blob_oids, par, |oid| odb.read(oid).map(|obj| (*oid, obj)))
                    .map_err(|e| match e {
                        ParallelHashError::Task(err) => err,
                    })?;
            for (oid, obj) in pairs {
                objects.insert(oid, obj);
            }
        }
    } else {
        for &oid in send {
            if objects.contains_key(&oid) {
                continue;
            }
            objects.insert(oid, odb.read(&oid)?);
        }
    }
    Ok(objects)
}

fn objects_dir_has_local_packs(objects_dir: &Path) -> bool {
    let pack_dir = objects_dir.join("pack");
    if !pack_dir.is_dir() {
        return false;
    }
    std::fs::read_dir(&pack_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
}

/// Select blob deltas for `send` and produce an ordered emit plan.
///
/// Windowed delta search via [`crate::pack_delta::search_windowed_blob_deltas`], depth-limited
/// via [`crate::pack::apply_delta_depth_limit`]. Trees and
/// commits are emitted whole (matching the CLI's blob-only delta selection).
/// Correctness (re-indexability) is preserved because every chosen base is acyclic
/// and either in-pack or, for thin packs, peer-held.
///
/// Two optional refinements bring this closer to the CLI packer:
///
/// * **Delta islands** (`opts.respect_islands`): when `pack.island` config marks
///   any ref, a target only deltas against a base in a compatible (superset)
///   island ([`crate::delta_islands::DeltaIslands::in_same_island`]) and ties are
///   broken toward bases in dominating islands
///   ([`crate::delta_islands::DeltaIslands::delta_cmp`]). Islands default to
///   inactive (the prior behavior).
/// * **On-disk delta reuse** (`opts.reuse_deltas`): an existing
///   `REF_DELTA`/`OFS_DELTA` edge whose base is also in this pack is reused
///   verbatim ([`crate::pack::packed_ref_delta_reuse_slice`]) rather than
///   recomputed, still subject to island rules.
///
/// When `opts.thin`, a blob may also delta against a base reachable from the
/// peer's `haves` (`have_closure`) even though that base is not emitted; such a
/// base is referenced via REF_DELTA.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_deltas(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &OidHashMap<u32>,
    search_threads: Option<usize>,
    mut walk_prefetch: HashMap<ObjectId, Object>,
) -> Result<DeltaPlan> {
    // Reuse-only repacks (`window == 0`) stream from disk at serialize time; avoid
    // materializing every payload during planning.
    let lazy_payloads = opts.window == 0 && !opts.thin && opts.reuse_deltas && opts.reuse_objects;
    let has_local_packs = objects_dir_has_local_packs(odb.objects_dir());
    let blob_only_planning =
        !lazy_payloads && opts.window > 0 && opts.delta && !opts.thin && !has_local_packs;
    let send_cap = send.len().max(1);
    let mut objects: OidHashMap<Object> = if lazy_payloads {
        oid_hash_map_with_capacity(send_cap)
    } else {
        load_objects_for_plan(
            odb,
            send,
            blob_only_planning,
            search_threads,
            &mut walk_prefetch,
        )?
    };

    let in_pack: OidHashSet = send.iter().copied().collect();

    // Delta islands (`--delta-islands`): only loaded when requested AND a git dir
    // is attached to the odb. An inactive island set (no `pack.island` config, or
    // no matched ref) imposes no restriction, so the common case is unaffected.
    let islands = load_islands_for_pack(odb, &in_pack, opts);

    // target oid -> base oid (the object `target` deltas against).
    let mut delta_to_base: OidHashMap<ObjectId> = oid_hash_map_with_capacity(send_cap);
    // Deltas whose instruction stream is reused verbatim from an existing pack.
    let mut reused_zlib: HashMap<
        ObjectId,
        (u64, std::sync::Arc<crate::pack::PackData>, usize, usize),
    > = HashMap::new();

    // On-disk delta reuse runs even with `--window=0` (no new deltas), matching Git pack-objects.
    if opts.reuse_deltas && odb.hash_algo() == HashAlgo::Sha1 && has_local_packs {
        let objects_dir = odb.objects_dir();
        let in_pack_std: HashSet<ObjectId> = in_pack.iter().copied().collect();
        let empty_resolved: HashMap<ObjectId, Object> = HashMap::new();
        let candidates = crate::pack::collect_packed_delta_reuse_for_send(
            objects_dir,
            &in_pack_std,
            &in_pack_std,
            &empty_resolved,
        )?;
        for (t, (base, size, pack_bytes, zlib_start, zlib_end)) in candidates {
            if lazy_payloads {
                if let Ok(info) = odb.read_info(&t) {
                    if info.kind == ObjectKind::Blob && info.size == 0 {
                        continue;
                    }
                }
            } else if objects[&t].data.is_empty() {
                continue;
            }
            if base != t && in_pack.contains(&base) && islands.in_same_island(&t, &base) {
                delta_to_base.insert(t, base);
                reused_zlib.insert(t, (size, pack_bytes, zlib_start, zlib_end));
            }
        }
    }

    // Objects referenced as the base of a reused on-disk delta must be stored as
    // whole objects (or reuse their own on-disk delta), not as a freshly computed
    // window delta — otherwise REF bases no longer match `by_oid` during indexing.
    let reuse_delta_bases: OidHashSet = delta_to_base
        .iter()
        .filter(|(t, _)| reused_zlib.contains_key(t))
        .map(|(_, b)| *b)
        .collect();

    let mut delta_encodings: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    if opts.window > 0 && opts.max_depth > 0 {
        // Without a readable pack directory, every pair is simply tried.
        let packed = if opts.reuse_deltas && has_local_packs {
            let blobs: OidHashSet = if lazy_payloads {
                send.iter()
                    .filter_map(|oid| {
                        let info = odb.read_info(oid).ok()?;
                        (info.kind == ObjectKind::Blob && info.size > 0).then_some(*oid)
                    })
                    .collect()
            } else {
                objects
                    .iter()
                    .filter(|(_, obj)| obj.kind == ObjectKind::Blob && !obj.data.is_empty())
                    .map(|(oid, _)| *oid)
                    .collect()
            };
            crate::pack::locate_packed_objects(odb.objects_dir(), &blobs).ok()
        } else {
            None
        };
        crate::pack_delta::extend_send_order_blob_deltas(
            crate::pack_delta::SendOrderDeltaInput {
                send,
                objects: &objects,
                blob_name_hashes,
                islands: &islands,
                skip_targets: &reuse_delta_bases,
                packed: packed.as_ref(),
                // Same-path history chains up to `max_depth` deep, so earlier revisions
                // within that distance stay useful bases; beyond it the pass would be
                // quadratic in a path's revision count.
                window: opts.window.max(opts.max_depth),
                hash_len: odb.hash_algo().len(),
            },
            &mut delta_to_base,
            &mut delta_encodings,
        );
        let skip_targets: OidHashSet = delta_to_base.keys().copied().collect();
        let (window_found, mut window_encodings) = crate::pack_delta::search_windowed_blob_deltas(
            odb,
            crate::pack_delta::DeltaSearchInput {
                objects: &objects,
                blob_name_hashes,
                islands: &islands,
                skip_targets: &skip_targets,
                skip_as_target: &reuse_delta_bases,
                warmup_delta_edges: &delta_to_base,
                packed: packed.as_ref(),
                opts,
            },
            search_threads,
        )?;
        for (target, base) in window_found {
            if delta_to_base.contains_key(&target) {
                continue;
            }
            delta_to_base.insert(target, base);
            if let Some(bytes) = window_encodings.remove(&target) {
                delta_encodings.insert(target, bytes);
            }
        }

        if opts.thin {
            let hash_len = odb.hash_algo().len();
            let mut external_blob_data: OidHashMap<Vec<u8>> = oid_hash_map();
            for &oid in have_closure {
                if in_pack.contains(&oid) {
                    continue;
                }
                if let Ok(obj) = odb.read(&oid) {
                    if obj.kind == ObjectKind::Blob && !obj.data.is_empty() {
                        external_blob_data.insert(oid, obj.data);
                    }
                }
            }
            for &t in send {
                if delta_to_base.contains_key(&t) || reuse_delta_bases.contains(&t) {
                    continue;
                }
                let Some(obj) = objects.get(&t) else {
                    continue;
                };
                if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
                    continue;
                }
                let t_data = &obj.data;
                let max_size = t_data.len().saturating_div(2).saturating_sub(hash_len);
                if max_size == 0 {
                    continue;
                }
                let mut best: Option<(ObjectId, usize)> = None;
                for (&b, b_data) in &external_blob_data {
                    if !islands.in_same_island(&t, &b) {
                        continue;
                    }
                    let sizediff = if b_data.len() < t_data.len() {
                        t_data.len() - b_data.len()
                    } else {
                        0
                    };
                    if sizediff >= max_size || t_data.len() < b_data.len() / 32 {
                        continue;
                    }
                    let delta_size = if t_data.starts_with(b_data) && t_data.len() > b_data.len() {
                        encode_prefix_extension_delta(b_data, t_data)
                            .ok()
                            .map(|d| d.len())
                    } else {
                        crate::delta_encode::DeltaIndex::new(b_data)
                            .encode(t_data, max_size)
                            .map(|d| d.len())
                    };
                    let Some(ds) = delta_size else {
                        continue;
                    };
                    let better = best.is_none_or(|(_, prev)| ds < prev);
                    if better {
                        best = Some((b, ds));
                    }
                }
                if let Some((base, _)) = best {
                    delta_to_base.insert(t, base);
                    if let Some(d) = external_blob_data.get(&base) {
                        objects
                            .entry(base)
                            .or_insert_with(|| Object::new(ObjectKind::Blob, d.clone()));
                    }
                }
            }
        }
    }

    if opts.max_depth > 0 && !delta_to_base.is_empty() {
        // Cap chain length. After snipping, any removed target reverts to whole.
        let mut delta_std: HashMap<ObjectId, ObjectId> =
            delta_to_base.iter().map(|(k, v)| (*k, *v)).collect();
        crate::pack::apply_delta_depth_limit(&mut delta_std, opts.max_depth);
        delta_to_base.clear();
        delta_to_base.extend(delta_std);

        // A reused delta whose target was snipped (or whose base ceased to be the
        // chosen base) reverts to a freshly-computed full/delta object.
        reused_zlib.retain(|t, _| delta_to_base.contains_key(t));
        delta_encodings.retain(|t, _| delta_to_base.contains_key(t));
    }

    let mut payloads: OidHashMap<Arc<[u8]>> = oid_hash_map_with_capacity(objects.len().max(1));
    for (oid, obj) in &objects {
        payloads.insert(*oid, Arc::from(obj.data.as_slice()));
    }
    if !lazy_payloads {
        let mut missing = Vec::new();
        for &oid in send {
            if payloads.contains_key(&oid) {
                continue;
            }
            if let Some(obj) = walk_prefetch.remove(&oid) {
                if !obj.data.is_empty() {
                    payloads.insert(oid, Arc::from(obj.data.into_boxed_slice()));
                    continue;
                }
                walk_prefetch.insert(oid, obj);
            }
            let info = odb.read_info(&oid)?;
            if info.size > 0 {
                missing.push(oid);
            }
        }
        if missing.is_empty() {
            // All send payloads already loaded during planning.
        } else if missing.len() < 32 {
            for oid in missing {
                let obj = odb.read(&oid)?;
                payloads.insert(oid, Arc::from(obj.data.as_slice()));
            }
        } else {
            let par = crate::pack_delta::pack_delta_search_parallel_options(odb, search_threads)?;
            let pairs =
                try_par_map_force(&missing, par, |oid| odb.read(oid).map(|obj| (*oid, obj)))
                    .map_err(|e| match e {
                        ParallelHashError::Task(err) => err,
                    })?;
            for (oid, obj) in pairs {
                payloads.insert(oid, Arc::from(obj.data.as_slice()));
            }
        }
    }

    // Emit in the original discovery order so commits precede their trees/blobs.
    // For OFS_DELTA the serializer needs each base to appear before its target;
    // discovery order already places a larger base blob no earlier than a smaller
    // one only by coincidence, so the serializer falls back to REF_DELTA whenever
    // the base has not yet been written.
    let mut entries: Vec<PlannedEntry> = Vec::with_capacity(send.len());
    for &oid in send {
        let kind = if let Some(obj) = objects.get(&oid) {
            obj.kind
        } else {
            odb.read_info(&oid)?.kind
        };
        entries.push(PlannedEntry {
            oid,
            kind,
            data: Vec::new(),
            base: delta_to_base.get(&oid).copied(),
            reused_delta: delta_encodings.get(&oid).cloned(),
            reused_delta_zlib: reused_zlib.get(&oid).cloned(),
        });
    }

    Ok(DeltaPlan { entries, payloads })
}

/// Load delta-island marks for the objects being packed, honoring
/// `opts.respect_islands`. Returns an inactive (no-op) island set when islands
/// are not requested, when the odb has no attached git directory, or when no
/// `pack.island` regex matches a ref — so callers can always consult the result
/// without a flag check.
fn load_islands_for_pack(
    odb: &Odb,
    in_pack: &OidHashSet,
    opts: &PackBuildOptions,
) -> crate::delta_islands::DeltaIslands {
    if !opts.respect_islands {
        return crate::delta_islands::DeltaIslands::default();
    }
    let Some(git_dir) = odb.config_git_dir() else {
        return crate::delta_islands::DeltaIslands::default();
    };
    let Ok(repo) = crate::repo::Repository::open(git_dir, &RepositoryOptions::empty()) else {
        return crate::delta_islands::DeltaIslands::default();
    };
    let env = crate::environment::Environment::empty();
    let cfg = crate::config::ConfigSet::load(&env, Some(git_dir), true).unwrap_or_default();
    let in_pack_std: HashSet<ObjectId> = in_pack.iter().copied().collect();
    crate::delta_islands::load_delta_islands(&repo, &cfg, &in_pack_std)
}

/// zlib-deflate `data` and append it to `buf`.
fn write_zlib(buf: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).map_err(Error::Io)?;
    let compressed = enc.finish().map_err(Error::Io)?;
    buf.extend_from_slice(&compressed);
    Ok(())
}

/// Encode an `OFS_DELTA` base distance (Git's offset varint). Lifted verbatim
/// from the CLI pack writer's `encode_git_ofs_delta_distance`.
pub(crate) fn encode_ofs_delta_distance(buf: &mut Vec<u8>, mut ofs: u64) {
    let mut dheader = [0u8; 32];
    let mut pos = dheader.len() - 1;
    dheader[pos] = (ofs & 0x7f) as u8;
    while {
        ofs >>= 7;
        ofs != 0
    } {
        pos -= 1;
        ofs -= 1;
        dheader[pos] = 0x80 | ((ofs & 0x7f) as u8);
    }
    buf.extend_from_slice(&dheader[pos..]);
}
