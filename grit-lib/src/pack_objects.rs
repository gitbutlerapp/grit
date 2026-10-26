//! Negotiation-driven pack-objects: enumerate wants minus haves, delta search, stream PACK v2.
//!
//! Holds object metadata (not payloads) during planning; reads object bytes from the
//! [`Odb`] on demand when encoding.

use crate::environment::RepositoryOptions;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::Path;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::bitmap_walk::ReachableSet;
use crate::delta_encode::{encode_prefix_extension_delta, DeltaIndex};
use crate::delta_islands::DeltaIslands;
use crate::error::{Error, Result};
use crate::objects::{
    empty_object_id_hash_set, oid_hash_map, oid_hash_set, oid_hash_set_from, parse_commit,
    parse_tag, HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind, OidHashMap, OidHashSet,
    TreeRefIter,
};
use crate::odb::Odb;
use crate::pack_bitmap::BitmapIndex;
use crate::pack_name_hash::pack_name_hash;
use crate::pack_object_select::{HaveBoundary, NameTrees};
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

    /// Copy stored pack entries (whole objects and deltas) without a new delta search
    /// (`window` 0), for rebuilding a pack from objects that are already packed.
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

    /// Pack settings for answering a fetch (upload-pack).
    ///
    /// Window and depth follow `pack.window` / `pack.depth`. Whole objects and deltas
    /// already in the repository's packs are copied as they are, and the delta search
    /// only covers blobs that are not already stored as a delta, as Git does when serving.
    /// Like Git, `pack.depth = 0` only disables new deltas: existing on-disk deltas are
    /// still sent.
    ///
    /// # Parameters
    /// - `cfg` — repository config, or `None` for Git's defaults.
    /// - `thin` — build a thin pack; pass `true` only when the client negotiated
    ///   `thin-pack` *and* holds objects (sent `have`s), since a thin pack against
    ///   nothing gains nothing.
    /// - `use_ofs_delta` — whether the client negotiated `ofs-delta`.
    #[must_use]
    pub fn for_serve(
        cfg: Option<&crate::config::ConfigSet>,
        thin: bool,
        use_ofs_delta: bool,
    ) -> Self {
        let (window, max_depth) = Self::window_and_depth_from_config(cfg);
        Self {
            thin,
            delta: true,
            window,
            max_depth,
            use_ofs_delta,
            reuse_deltas: true,
            reuse_objects: true,
            use_bitmaps: true,
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
        let peer_odb = (!std::ptr::eq(self.source_odb, self.odb)).then_some(self.odb);
        let selection = crate::pack_object_select::enumerate_pack_objects_naming(
            self.source_odb,
            &self.wants,
            &self.haves,
            &crate::pack_object_select::PackEnumerateOptions {
                filter: None,
                shallow_grafts: &source_shallow,
                have_shallow_grafts: Some(&have_shallow),
                use_bitmaps: opts.use_bitmaps,
                exclude_objects: None,
                peer_odb,
                have_boundary: HaveBoundary::CommitClosure,
            },
            NameTrees::for_delta_search(&opts),
        )?;
        let send = selection.send;
        self.fire(PackProgress::Enumerated { total: send.len() });
        if !opts.delta {
            let bytes = serialize_pack(self.source_odb, &send, &opts)?;
            w.write_all(&bytes).map_err(Error::Io)?;
            let count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            return Ok(PackStats {
                object_count: count,
                pack_bytes: bytes.len() as u64,
                ..Default::default()
            });
        }
        let plan = plan_deltas(
            self.source_odb,
            &send,
            &selection.have_closure,
            selection.peer_held.as_deref(),
            &opts,
            &selection.name_hashes,
            self.opts.search_threads,
        )?;
        self.fire(PackProgress::PlannedDeltas);
        crate::pack_delta::write_pack_with_deltas(
            self.source_odb,
            &plan.entries,
            &opts,
            self.opts.write_threads,
            w,
            None,
        )
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
    build_pack_with_shallow_filter_and_boundary(
        odb,
        wants,
        haves,
        empty_object_id_hash_set(),
        empty_object_id_hash_set(),
        None,
        opts,
        HaveBoundary::CommitClosure,
    )
}

/// Like [`build_pack`], but uses [`HaveBoundary::TipsOnly`] for send-pack pushes.
///
/// Negotiated have tips bound the want-side walk lazily instead of pre-walking
/// their full commit closure before enumeration.
///
/// # Errors
///
/// Same as [`build_pack`]: missing wants, corrupt objects, or pack write failures.
pub fn build_pack_for_push(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    build_pack_with_shallow_filter_and_boundary(
        odb,
        wants,
        haves,
        empty_object_id_hash_set(),
        empty_object_id_hash_set(),
        None,
        opts,
        HaveBoundary::TipsOnly,
    )
}

/// Build a pack for upload-pack with optional shallow grafts and object filter.
///
/// When `filter` is set, objects are selected the way `pack-objects --filter` does:
/// `wants` and the objects their tags point at are always sent.
pub fn build_pack_with_shallow_and_filter(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    have_shallow: &HashSet<ObjectId>,
    source_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    build_pack_with_shallow_filter_and_boundary(
        odb,
        wants,
        haves,
        have_shallow,
        source_shallow,
        filter,
        opts,
        HaveBoundary::CommitClosure,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_pack_with_shallow_filter_and_boundary(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    have_shallow: &HashSet<ObjectId>,
    source_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
    have_boundary: HaveBoundary,
) -> Result<Vec<u8>> {
    let mut pack = Vec::new();
    write_pack_with_shallow_and_filter(
        odb,
        wants,
        haves,
        have_shallow,
        source_shallow,
        filter,
        opts,
        have_boundary,
        &mut pack,
    )?;
    Ok(pack)
}

/// Streaming form of [`build_pack_with_shallow_and_filter`].
#[allow(clippy::too_many_arguments)]
fn write_pack_with_shallow_and_filter(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    have_shallow: &HashSet<ObjectId>,
    source_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
    have_boundary: HaveBoundary,
    w: &mut dyn Write,
) -> Result<PackStats> {
    let selection = crate::pack_object_select::enumerate_pack_objects_naming(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter,
            shallow_grafts: source_shallow,
            have_shallow_grafts: Some(have_shallow),
            use_bitmaps: opts.use_bitmaps,
            exclude_objects: None,
            peer_odb: None,
            have_boundary,
        },
        NameTrees::for_delta_search(opts),
    )?;
    if !opts.delta {
        let bytes = serialize_pack(odb, &selection.send, opts)?;
        w.write_all(&bytes).map_err(Error::Io)?;
        return Ok(PackStats {
            object_count: u32::try_from(selection.send.len()).unwrap_or(u32::MAX),
            pack_bytes: bytes.len() as u64,
            ..PackStats::default()
        });
    }
    let plan = plan_deltas(
        odb,
        &selection.send,
        &selection.have_closure,
        selection.peer_held.as_deref(),
        opts,
        &selection.name_hashes,
        None,
    )?;
    crate::pack_delta::write_pack_with_deltas(odb, &plan.entries, opts, None, w, None)
}

/// Stream a pack for upload-pack: pack reuse when bitmaps allow, otherwise pack-objects.
///
/// The whole build runs in the odb's pack read context, so planning and the pack-write
/// workers share one set of pack caches. Filtered (partial clone) packs stream too.
///
/// # Errors
///
/// Missing wants, corrupt objects, or I/O failures while writing to `w`.
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
    repo.odb().with_pack_read_context(|| {
        write_upload_pack_stream_in_context(
            repo,
            wants,
            haves,
            source_shallow,
            have_shallow,
            filter,
            opts,
            progress,
            w,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn write_upload_pack_stream_in_context(
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
        return write_pack_with_shallow_and_filter(
            repo.odb(),
            wants,
            haves,
            have_shallow,
            source_shallow,
            filter,
            opts,
            HaveBoundary::CommitClosure,
            w,
        );
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
    let compression = odb.pack_compression()?;
    let mut oid_to_offset: OidHashMap<u64> = oid_hash_map();
    let mut body_written = 0u64;

    for entry in &plan.entries {
        let start = pack_body_start + body_written;
        let mut chunk = Vec::new();
        if entry.base.is_none() {
            if let Some(reuse) = reuse.as_mut() {
                if let Some(bytes) = reuse.try_emit_whole(&entry.oid, opts)? {
                    chunk = bytes;
                }
            }
            if chunk.is_empty() {
                write_whole_pack_object_from_odb(&mut chunk, odb, entry.oid, entry.kind, opts)?;
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
        if let Some((delta_size, pack_bytes, zlib_start, zlib_end)) = &entry.reused_delta_zlib {
            let delta_len = usize::try_from(*delta_size)
                .map_err(|_| Error::CorruptObject("reused delta size exceeds usize".to_owned()))?;
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
            let computed;
            let (delta_len, zlib): (usize, &[u8]) = match &entry.searched_delta {
                Some((len, zlib)) => (*len, zlib),
                None => {
                    let target_data = odb.read(&entry.oid)?.data;
                    let base_data = odb.read(&base_oid)?.data;
                    let delta = if target_data.starts_with(&base_data)
                        && target_data.len() > base_data.len()
                    {
                        encode_prefix_extension_delta(&base_data, &target_data)?
                    } else {
                        DeltaIndex::new(&base_data)
                            .encode(&target_data, 0)
                            .ok_or_else(|| {
                                Error::CorruptObject(
                                    "failed to encode pack delta for object".into(),
                                )
                            })?
                    };
                    let mut buf = Vec::new();
                    write_zlib(&mut buf, &delta, compression)?;
                    computed = buf;
                    (delta.len(), &computed)
                }
            };
            if let Some(base_off) = in_pack_offset.filter(|_| opts.use_ofs_delta) {
                let dist = start.checked_sub(base_off).ok_or_else(|| {
                    Error::CorruptObject("ofs-delta distance underflow".to_owned())
                })?;
                encode_pack_object_header(&mut chunk, 6, delta_len);
                encode_ofs_delta_distance(&mut chunk, dist);
            } else {
                encode_pack_object_header(&mut chunk, 7, delta_len);
                if base_oid.as_bytes().len() != algo.len() {
                    return Err(Error::CorruptObject(
                        "ref-delta base oid width mismatch".to_owned(),
                    ));
                }
                chunk.extend_from_slice(base_oid.as_bytes());
            }
            chunk.extend_from_slice(zlib);
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
    let selection = crate::pack_object_select::enumerate_pack_objects_naming(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter: None,
            shallow_grafts: empty_shallow,
            have_shallow_grafts: Some(empty_shallow),
            use_bitmaps: opts.use_bitmaps,
            exclude_objects: Some(&have_closure),
            peer_odb: None,
            have_boundary: HaveBoundary::CommitClosure,
        },
        NameTrees::for_delta_search(opts),
    )?;
    build_pack_from_send_list_full(
        odb,
        &selection.send,
        &selection.have_closure,
        opts,
        &selection.name_hashes,
        None,
        write_threads,
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
    let selection = crate::pack_object_select::enumerate_pack_objects_naming(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter: None,
            shallow_grafts: empty_shallow,
            have_shallow_grafts: Some(empty_shallow),
            use_bitmaps: opts.use_bitmaps,
            exclude_objects: Some(&have_closure),
            peer_odb: None,
            have_boundary: HaveBoundary::CommitClosure,
        },
        NameTrees::for_delta_search(opts),
    )?;
    build_pack_from_send_list_with_parallel_threads(
        odb,
        &selection.send,
        &selection.have_closure,
        opts,
        &selection.name_hashes,
        search_threads,
        write_threads,
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
    )
}

/// Serialize a precomputed object list with explicit delta-search and write parallelism.
pub fn build_pack_from_send_list_with_parallel_threads(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    search_threads: Option<usize>,
    write_threads: Option<usize>,
) -> Result<Vec<u8>> {
    build_pack_from_send_list_full(
        odb,
        send,
        have_closure,
        opts,
        blob_name_hashes,
        search_threads,
        write_threads,
    )
    .map(|pack| pack.bytes)
}

pub(crate) fn build_pack_from_send_list_full(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    search_threads: Option<usize>,
    write_threads: Option<usize>,
) -> Result<SerializedDeltaPack> {
    if !opts.delta {
        let bytes = serialize_pack(odb, send, opts)?;
        return Ok(SerializedDeltaPack {
            bytes,
            index_rows: Vec::new(),
            staging_path: None,
        });
    }
    let plan = plan_deltas(
        odb,
        send,
        have_closure,
        None,
        opts,
        blob_name_hashes,
        search_threads,
    )?;
    crate::pack_delta::serialize_pack_with_deltas(odb, &plan.entries, opts, write_threads)
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
        let kind = odb.read_info(oid)?.kind;
        write_whole_pack_object_from_odb(&mut buf, odb, *oid, kind, opts)?;
    }

    append_pack_trailer(&mut buf, odb.hash_algo());
    Ok(buf)
}

/// Append one whole object from the ODB, streaming large blobs when needed.
fn write_whole_pack_object_from_odb(
    buf: &mut Vec<u8>,
    odb: &Odb,
    oid: ObjectId,
    kind: ObjectKind,
    opts: &PackBuildOptions,
) -> Result<()> {
    if opts.reuse_objects && odb.hash_algo() == HashAlgo::Sha1 {
        if let Some(raw) = crate::pack::packed_full_object_slice(odb.objects_dir(), &oid)? {
            buf.extend_from_slice(&raw);
            return Ok(());
        }
    }
    let size = odb.read_info(&oid)?.size;
    let stream_from = odb.big_file_threshold()?;
    if kind == ObjectKind::Blob && size >= stream_from {
        return odb
            .with_stream(&oid, |mut stream| {
                let payload_len = usize::try_from(stream.size).map_err(|_| {
                    Error::CorruptObject(format!("object size overflow for {}", oid.to_hex()))
                })?;
                encode_pack_object_header(buf, pack_type_code(kind), payload_len);
                write_zlib_from_reader(buf, &mut stream.reader, odb.pack_compression()?)
            })?
            .ok_or_else(|| Error::ObjectNotFound(oid.to_hex()));
    }
    let obj = odb.read(&oid)?;
    write_whole_pack_object(buf, odb, oid, kind, &obj.data, opts)
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
    let compression = odb.pack_compression()?;
    write_zlib(buf, data, compression)
}

/// Append the trailing pack checksum: the hash of everything written so far, at
/// the repository's hash width (SHA-1 → 20 bytes, SHA-256 → 32 bytes).
pub(crate) fn append_pack_trailer(buf: &mut Vec<u8>, algo: HashAlgo) {
    buf.extend_from_slice(&algo.checksum_digest(&*buf));
}

/// Append `oid` from `odb` to an in-progress pack as one whole object (header + zlib body).
///
/// Returns the object's kind.
pub(crate) fn append_whole_object_from_odb(
    buf: &mut Vec<u8>,
    odb: &Odb,
    oid: ObjectId,
) -> Result<ObjectKind> {
    let obj = odb.read(&oid)?;
    write_whole_pack_object(
        buf,
        odb,
        oid,
        obj.kind,
        &obj.data,
        &PackBuildOptions::default(),
    )?;
    Ok(obj.kind)
}

/// A single object to write, either whole or as a delta against a chosen base.
pub(crate) struct PlannedEntry {
    pub(crate) oid: ObjectId,
    pub(crate) kind: ObjectKind,
    /// `Some(base_oid)` when this entry is a delta against `base_oid`. The base may be
    /// an in-pack object or — for thin packs — an external base present only on the peer.
    pub(crate) base: Option<ObjectId>,
    /// Delta chosen by the delta search: uncompressed length and zlib-compressed
    /// instructions, ready to copy into the pack. When neither this nor
    /// [`Self::reused_delta_zlib`] is set for a delta, the writer computes it.
    pub(crate) searched_delta: Option<(usize, Vec<u8>)>,
    /// On-disk reuse: uncompressed size from the source pack header plus zlib
    /// bytes copied verbatim from a cached pack buffer (Git's `reuse_delta`).
    pub(crate) reused_delta_zlib:
        Option<(u64, std::sync::Arc<crate::pack::PackData>, usize, usize)>,
}

/// The full delta plan: the ordered entries to emit. Payloads stay in the object
/// database and are read again when the pack is written.
pub(crate) struct DeltaPlan {
    pub(crate) entries: Vec<PlannedEntry>,
}

/// A delta pack built in-process, with index rows that match the emitted stream.
pub(crate) struct SerializedDeltaPack {
    pub bytes: Vec<u8>,
    pub index_rows: Vec<(ObjectId, u64, u32)>,
    /// Pack bytes on disk when set ([`Self::bytes`] may be empty).
    pub staging_path: Option<std::path::PathBuf>,
}

/// Kind and size of each of `oids`, in the same order, without inflating payloads.
fn read_object_infos(
    odb: &Odb,
    oids: &[ObjectId],
    search_threads: Option<usize>,
) -> Result<Vec<ObjectInfo>> {
    let par = crate::pack_delta::pack_delta_search_parallel_options(odb, search_threads)?;
    let read = |oid: &ObjectId| odb.read_info(oid);
    if par.threads.get() <= 1 || oids.len() < 32 {
        return oids.iter().map(read).collect();
    }
    try_par_map_force(oids, par, read).map_err(|e| match e {
        ParallelHashError::Task(err) => err,
    })
}

/// Reorder `entries` so every delta base that is in the pack precedes its targets.
///
/// Entries keep their relative order except that a not-yet-emitted base chain is
/// emitted, deepest base first, right before the first entry that needs it. Bases
/// outside the pack (thin-pack bases) are left alone. The entries are moved in place.
fn order_bases_first(entries: &mut [PlannedEntry]) {
    let n = entries.len();
    let bases: Vec<Option<usize>> = {
        let position: OidHashMap<usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.oid, i))
            .collect();
        entries
            .iter()
            .map(|e| e.base.and_then(|b| position.get(&b).copied()))
            .collect()
    };
    // `dest[i]`: the position entry `i` moves to.
    let mut dest = vec![usize::MAX; n];
    let mut next = 0usize;
    let mut chain = Vec::new();
    for i in 0..n {
        let mut cur = Some(i);
        while let Some(j) = cur {
            if dest[j] != usize::MAX || chain.len() > n {
                break;
            }
            chain.push(j);
            cur = bases[j];
        }
        while let Some(j) = chain.pop() {
            if dest[j] == usize::MAX {
                dest[j] = next;
                next += 1;
            }
        }
    }
    for i in 0..n {
        while dest[i] != i {
            let d = dest[i];
            entries.swap(i, d);
            dest.swap(i, d);
        }
    }
}

/// Whether `objects_dir/pack` holds at least one `.pack` file.
pub(crate) fn objects_dir_has_local_packs(objects_dir: &Path) -> bool {
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

/// Select deltas for `send` and produce an ordered emit plan.
///
/// Only object kinds and sizes are read up front. New deltas come from the streaming
/// window search ([`crate::delta_search::search_deltas`]), which covers every object
/// kind, groups objects by kind, path name hash (`name_hashes`, for blobs and trees)
/// and size as Git does, and reads payloads from `odb` only while they are in the
/// window, so memory does not grow with the number or size of the objects packed.
/// Chains are depth-limited via [`crate::pack::apply_delta_depth_limit`].
/// Correctness (re-indexability) is preserved because every chosen base is acyclic
/// and either in-pack or, for thin packs, peer-held.
///
/// Two optional refinements:
///
/// * **Delta islands** (`opts.respect_islands`): when `pack.island` config marks
///   any ref, a target only deltas against a base in a compatible (superset)
///   island ([`crate::delta_islands::DeltaIslands::in_same_island`]). Islands
///   default to inactive (the prior behavior).
/// * **On-disk delta reuse** (`opts.reuse_deltas`): an existing
///   `REF_DELTA`/`OFS_DELTA` edge whose base is also in this pack is reused
///   verbatim ([`crate::pack::packed_ref_delta_reuse_slice`]) rather than
///   recomputed, still subject to island rules. Such objects and their bases are
///   neither targets nor bases of the search.
///
/// When `opts.thin`, a blob may also delta against a base reachable from the
/// peer's negotiation `haves` even though that base is not emitted; such a base
/// is referenced via REF_DELTA.
pub(crate) fn plan_deltas(
    odb: &Odb,
    send: &[ObjectId],
    have_tips: &HashSet<ObjectId>,
    peer_held: Option<&ReachableSet>,
    opts: &PackBuildOptions,
    name_hashes: &HashMap<ObjectId, u32>,
    search_threads: Option<usize>,
) -> Result<DeltaPlan> {
    let has_local_packs = objects_dir_has_local_packs(odb.objects_dir());
    let reuse_on_disk = opts.reuse_deltas && odb.hash_algo() == HashAlgo::Sha1 && has_local_packs;
    let search = opts.window > 0 && opts.max_depth > 0;
    let thin = opts.thin && search;
    let infos = read_object_infos(odb, send, search_threads)?;
    let in_pack: HashSet<ObjectId> = if reuse_on_disk || thin || opts.respect_islands {
        send.iter().copied().collect()
    } else {
        HashSet::new()
    };

    // Delta islands (`--delta-islands`): only loaded when requested AND a git dir
    // is attached to the odb. An inactive island set (no `pack.island` config, or
    // no matched ref) imposes no restriction, so the common case is unaffected.
    let islands = load_islands_for_pack(odb, &in_pack, opts);

    // target oid -> base oid (the object `target` deltas against).
    let mut delta_to_base: OidHashMap<ObjectId> = oid_hash_map();
    // Deltas whose instruction stream is reused verbatim from an existing pack.
    let mut reused_zlib: HashMap<
        ObjectId,
        (u64, std::sync::Arc<crate::pack::PackData>, usize, usize),
    > = HashMap::new();

    // On-disk delta reuse runs even with `--window=0` (no new deltas), matching Git pack-objects.
    if reuse_on_disk {
        // A thin pack keeps an on-disk delta whose base the peer already holds; it is
        // written as `REF_DELTA` and completed by the receiver.
        let mut reusable_bases = in_pack.clone();
        if opts.thin && !have_tips.is_empty() {
            for &tip in have_tips {
                if odb.read_info(&tip)?.kind == ObjectKind::Commit {
                    reusable_bases.extend(blob_oids_in_commit(odb, tip)?);
                }
            }
        }
        let reusable_bases = Cow::Owned(reusable_bases);
        let empty_resolved: HashMap<ObjectId, Object> = HashMap::new();
        let candidates = crate::pack::collect_packed_delta_reuse_for_send(
            odb.objects_dir(),
            &in_pack,
            &reusable_bases,
            &empty_resolved,
        )?;
        for (t, (base, size, pack_bytes, zlib_start, zlib_end)) in candidates {
            if base != t && reusable_bases.contains(&base) && islands.in_same_island(&t, &base) {
                delta_to_base.insert(t, base);
                reused_zlib.insert(t, (size, pack_bytes, zlib_start, zlib_end));
            }
        }
        // An empty blob is always stored whole.
        for (oid, info) in send.iter().zip(&infos) {
            if info.kind == ObjectKind::Blob && info.size == 0 && reused_zlib.remove(oid).is_some()
            {
                delta_to_base.remove(oid);
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

    let mut searched: OidHashMap<(usize, Vec<u8>)> = oid_hash_map();
    if search {
        let found = search_new_deltas(
            odb,
            send,
            &infos,
            |oid| delta_to_base.contains_key(oid) || reuse_delta_bases.contains(oid),
            name_hashes,
            &islands,
            opts,
            search_threads,
            has_local_packs,
        )?;
        for (target, base, delta) in found {
            delta_to_base.insert(target, base);
            searched.insert(target, delta);
        }
    }

    if thin {
        let hash_len = odb.hash_algo().len();
        let thin_targets: Vec<ObjectId> = send
            .iter()
            .zip(&infos)
            .filter(|(t, info)| {
                info.kind == ObjectKind::Blob
                    && (info.size as usize)
                        .saturating_div(2)
                        .saturating_sub(hash_len)
                        > 0
                    && !delta_to_base.contains_key(*t)
                    && !reuse_delta_bases.contains(*t)
            })
            .map(|(t, _)| *t)
            .collect();
        let in_pack_oid: OidHashSet = in_pack.iter().copied().collect();
        let window = opts.window.max(opts.max_depth).max(1);
        let bitmap_index = peer_held.map(ReachableSet::bitmap_index);
        for t in thin_targets {
            let t_data = odb.read(&t)?.data;
            let path_key = name_hashes.get(&t).copied().unwrap_or(0);
            let Some((base, _base_data)) = find_thin_pack_peer_blob_base(
                odb,
                &t,
                &t_data,
                path_key,
                &in_pack_oid,
                have_tips,
                peer_held,
                bitmap_index,
                &islands,
                window,
                hash_len,
            )?
            else {
                continue;
            };
            delta_to_base.insert(t, base);
        }
    }
    drop(in_pack);

    if opts.max_depth > 0 && !delta_to_base.is_empty() {
        // Cap chain length. After snipping, any removed target reverts to whole.
        let mut delta_std: HashMap<ObjectId, ObjectId> = delta_to_base.drain().collect();
        crate::pack::apply_delta_depth_limit(&mut delta_std, opts.max_depth);
        delta_to_base.extend(delta_std);
    }

    // Emit in discovery order (commits before their trees/blobs), pulling each in-pack
    // delta base ahead of its first target so deltas can use the compact OFS_DELTA.
    // A delta whose target was snipped reverts to a whole object.
    let mut entries: Vec<PlannedEntry> = send
        .iter()
        .zip(&infos)
        .map(|(&oid, info)| {
            let base = delta_to_base.remove(&oid);
            let (searched_delta, reused_delta_zlib) = match base {
                Some(_) => (searched.remove(&oid), reused_zlib.remove(&oid)),
                None => (None, None),
            };
            PlannedEntry {
                oid,
                kind: info.kind,
                base,
                searched_delta,
                reused_delta_zlib,
            }
        })
        .collect();
    drop((infos, delta_to_base, searched, reused_zlib));
    order_bases_first(&mut entries);
    Ok(DeltaPlan { entries })
}

/// A delta chosen by the search: target, base, and (delta length, zlib-compressed delta).
type SearchedDelta = (ObjectId, ObjectId, (usize, Vec<u8>));

/// Run the windowed delta search over the objects of `send` (with `infos` in the same
/// order) that `skip` leaves, trying only pairs in compatible delta islands.
#[allow(clippy::too_many_arguments)]
fn search_new_deltas(
    odb: &Odb,
    send: &[ObjectId],
    infos: &[ObjectInfo],
    skip: impl Fn(&ObjectId) -> bool,
    name_hashes: &HashMap<ObjectId, u32>,
    islands: &crate::delta_islands::DeltaIslands,
    opts: &PackBuildOptions,
    search_threads: Option<usize>,
    has_local_packs: bool,
) -> Result<Vec<SearchedDelta>> {
    u32::try_from(send.len())
        .map_err(|_| Error::CorruptObject("too many objects for one pack".to_owned()))?;
    let ids: Vec<u32> = (0..send.len())
        .filter(|&i| infos[i].size > 0 && !skip(&send[i]))
        .map(|i| i as u32)
        .collect();
    // Pairs an earlier pack writer already compared inside one pack are skipped.
    let packed = if opts.reuse_deltas && has_local_packs {
        let oids: OidHashSet = ids.iter().map(|&i| send[i as usize]).collect();
        crate::pack::locate_packed_objects(odb.objects_dir(), &oids).ok()
    } else {
        None
    };
    let candidates: Vec<crate::delta_search::DeltaCandidate> = ids
        .into_iter()
        .map(|id| {
            let oid = send[id as usize];
            let info = infos[id as usize];
            let slot = packed.as_ref().and_then(|p| p.get(&oid));
            crate::delta_search::DeltaCandidate {
                id,
                oid,
                kind: info.kind,
                size: info.size,
                name_hash: name_hashes.get(&oid).copied().unwrap_or(0),
                whole_in_pack: slot.filter(|s| s.whole).map(|s| s.pack),
                in_pack: slot.map(|s| s.pack),
                dependent_depth: 0,
            }
        })
        .collect();
    drop(packed);
    let window_memory = crate::pack_delta::pack_config(odb)?.pack_window_memory_bytes();
    let threads = crate::pack_delta::pack_delta_search_parallel_options(odb, search_threads)?;
    let in_islands = |target: &ObjectId, base: &ObjectId| islands.in_same_island(target, base);
    let found = crate::delta_search::search_deltas(
        odb,
        candidates,
        crate::delta_search::DeltaSearchParams {
            window: opts.window,
            max_depth: opts.max_depth,
            window_memory,
            hash_len: odb.hash_algo().len(),
            compression: odb.pack_compression()?,
            threads: threads.threads.get(),
        },
        &in_islands,
    )?;
    Ok(found
        .into_iter()
        .map(|d| {
            (
                send[d.target as usize],
                send[d.base as usize],
                (d.delta_len, d.zlib),
            )
        })
        .collect())
}

fn peer_side_delta_base(
    base: &ObjectId,
    in_send: &OidHashSet,
    have_tips: &HashSet<ObjectId>,
    peer_held: Option<&ReachableSet>,
) -> bool {
    if in_send.contains(base) {
        return false;
    }
    peer_held.is_some_and(|held| held.contains(base)) || have_tips.contains(base)
}

#[allow(clippy::too_many_arguments)]
fn record_thin_peer_candidate(
    best: &mut Option<(ObjectId, usize, Vec<u8>)>,
    target: &ObjectId,
    candidate: ObjectId,
    base_data: Vec<u8>,
    target_data: &[u8],
    in_send: &OidHashSet,
    have_tips: &HashSet<ObjectId>,
    peer_held: Option<&ReachableSet>,
    islands: &DeltaIslands,
    max_size: usize,
    known_peer: bool,
) {
    if !known_peer && !peer_side_delta_base(&candidate, in_send, have_tips, peer_held) {
        return;
    }
    if !islands.in_same_island(target, &candidate) {
        return;
    }
    let sizediff = if base_data.len() < target_data.len() {
        target_data.len() - base_data.len()
    } else {
        0
    };
    if sizediff >= max_size || target_data.len() < base_data.len() / 32 {
        return;
    }
    let Some(ds) = thin_pack_delta_size(&base_data, target_data, max_size) else {
        return;
    };
    let better = best.as_ref().is_none_or(|(_, prev, _)| ds < *prev);
    if better {
        *best = Some((candidate, ds, base_data));
    }
}

fn thin_pack_delta_size(base_data: &[u8], target_data: &[u8], max_size: usize) -> Option<usize> {
    if target_data.starts_with(base_data) && target_data.len() > base_data.len() {
        encode_prefix_extension_delta(base_data, target_data)
            .ok()
            .map(|d| d.len())
    } else {
        DeltaIndex::new(base_data)
            .encode(target_data, max_size)
            .map(|d| d.len())
    }
}

#[allow(clippy::too_many_arguments)]
fn find_thin_pack_peer_blob_base(
    odb: &Odb,
    target: &ObjectId,
    target_data: &[u8],
    name_hash: u32,
    in_send: &OidHashSet,
    have_tips: &HashSet<ObjectId>,
    peer_held: Option<&ReachableSet>,
    bitmap_index: Option<&std::sync::Arc<BitmapIndex>>,
    islands: &DeltaIslands,
    window: usize,
    hash_len: usize,
) -> Result<Option<(ObjectId, Vec<u8>)>> {
    let max_size = target_data.len().saturating_div(2).saturating_sub(hash_len);
    if max_size == 0 {
        return Ok(None);
    }

    let mut best: Option<(ObjectId, usize, Vec<u8>)> = None;
    let mut tried = 0usize;

    if let (Some(held), Some(index)) = (peer_held, bitmap_index) {
        let peer_blobs: Vec<ObjectId> = held.iter_by_kind(ObjectKind::Blob).collect();
        for oid in peer_blobs.into_iter().rev() {
            if tried >= window {
                break;
            }
            let Some(pos) = index.position_of(&oid) else {
                continue;
            };
            let Some(h) = index.name_hash(pos) else {
                continue;
            };
            if h != name_hash {
                continue;
            }
            if in_send.contains(&oid) {
                continue;
            }
            tried += 1;
            let obj = odb.read(&oid)?;
            if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
                continue;
            }
            record_thin_peer_candidate(
                &mut best,
                target,
                oid,
                obj.data,
                target_data,
                in_send,
                have_tips,
                peer_held,
                islands,
                max_size,
                false,
            );
        }
    }
    if best.is_none() {
        let opened = open_pack_bitmap_index(odb);
        let index = bitmap_index.or(opened.as_ref());
        if let Some(index) = index {
            let count = index.object_count();
            let mut seen_same_path = 0usize;
            for pos in (0..count).rev() {
                if seen_same_path >= window {
                    break;
                }
                let Some(h) = index.name_hash(pos) else {
                    continue;
                };
                if h != name_hash {
                    continue;
                }
                let Ok(oid) = index.oid_at(pos) else {
                    continue;
                };
                if in_send.contains(&oid) {
                    continue;
                }
                seen_same_path += 1;
                let info = match odb.read_info(&oid) {
                    Ok(info) => info,
                    Err(_) => continue,
                };
                if info.kind != ObjectKind::Blob || info.size == 0 {
                    continue;
                }
                if !peer_side_delta_base(&oid, in_send, have_tips, peer_held) {
                    continue;
                }
                let obj = odb.read(&oid)?;
                record_thin_peer_candidate(
                    &mut best,
                    target,
                    oid,
                    obj.data,
                    target_data,
                    in_send,
                    have_tips,
                    peer_held,
                    islands,
                    max_size,
                    false,
                );
            }
        }
    }
    if best.is_none() {
        for (oid, data) in peer_blobs_from_have_commits(odb, have_tips, name_hash)? {
            record_thin_peer_candidate(
                &mut best,
                target,
                oid,
                data,
                target_data,
                in_send,
                have_tips,
                peer_held,
                islands,
                max_size,
                true,
            );
        }
    }

    Ok(best.map(|(oid, _, data)| (oid, data)))
}

fn blob_oids_in_commit(odb: &Odb, commit_oid: ObjectId) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    let commit_obj = odb.read(&commit_oid)?;
    if commit_obj.kind != ObjectKind::Commit {
        return Ok(out);
    }
    let commit = parse_commit(&commit_obj.data)?;
    let oid_len = odb.hash_algo().len();
    let mut queue = VecDeque::from([commit.tree]);
    while let Some(tree_oid) = queue.pop_front() {
        let tree_obj = odb.read(&tree_oid)?;
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

fn peer_blobs_from_have_commits(
    odb: &Odb,
    have_tips: &HashSet<ObjectId>,
    name_hash: u32,
) -> Result<Vec<(ObjectId, Vec<u8>)>> {
    let oid_len = odb.hash_algo().len();
    let mut out = Vec::new();
    for &commit_oid in have_tips {
        let commit_obj = match odb.read(&commit_oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if commit_obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&commit_obj.data)?;
        let mut queue = VecDeque::from([(commit.tree, String::new())]);
        while let Some((tree_oid, path)) = queue.pop_front() {
            let tree_obj = match odb.read(&tree_oid) {
                Ok(o) => o,
                Err(_) => continue,
            };
            for entry in TreeRefIter::with_oid_len(&tree_obj.data, oid_len) {
                let entry = entry?;
                if entry.mode == 0o160000 {
                    continue;
                }
                let child_path = if path.is_empty() {
                    String::from_utf8_lossy(entry.name).into_owned()
                } else {
                    format!("{}/{}", path, String::from_utf8_lossy(entry.name))
                };
                match entry.mode & 0o170000 {
                    0o040000 => queue.push_back((entry.oid, child_path)),
                    0o100000 | 0o120000 => {
                        if pack_name_hash(&child_path) != name_hash {
                            continue;
                        }
                        let blob = odb.read(&entry.oid)?;
                        if blob.kind == ObjectKind::Blob && !blob.data.is_empty() {
                            out.push((entry.oid, blob.data));
                        }
                    }
                    _ => queue.push_back((entry.oid, child_path)),
                }
            }
        }
    }
    Ok(out)
}

fn open_pack_bitmap_index(odb: &Odb) -> Option<std::sync::Arc<BitmapIndex>> {
    let git_dir = odb.config_git_dir()?;
    let repo = Repository::open(git_dir, &RepositoryOptions::empty()).ok()?;
    BitmapIndex::open(&repo).ok().flatten()
}

/// Load delta-island marks for the objects being packed, honoring
/// `opts.respect_islands`. Returns an inactive (no-op) island set when islands
/// are not requested, when the odb has no attached git directory, or when no
/// `pack.island` regex matches a ref — so callers can always consult the result
/// without a flag check.
fn load_islands_for_pack(
    odb: &Odb,
    in_pack: &HashSet<ObjectId>,
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
    crate::delta_islands::load_delta_islands(&repo, &cfg, in_pack)
}

/// zlib-deflate `data` and append it to `buf`.
fn write_zlib(buf: &mut Vec<u8>, data: &[u8], compression: Compression) -> Result<()> {
    let mut enc = ZlibEncoder::new(Vec::new(), compression);
    enc.write_all(data).map_err(Error::Io)?;
    let compressed = enc.finish().map_err(Error::Io)?;
    buf.extend_from_slice(&compressed);
    Ok(())
}

/// zlib-deflate bytes read from `reader` and append them to `buf`.
fn write_zlib_from_reader(
    buf: &mut Vec<u8>,
    reader: &mut dyn std::io::Read,
    compression: Compression,
) -> Result<()> {
    let mut enc = ZlibEncoder::new(Vec::new(), compression);
    std::io::copy(reader, &mut enc).map_err(Error::Io)?;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use crate::pack_object_select::{enumerate_pack_objects, PackEnumerateOptions};

    fn planned(n: u8, base: Option<u8>) -> PlannedEntry {
        let oid = |n: u8| ObjectId::from_bytes(&[n; 20]).unwrap();
        PlannedEntry {
            oid: oid(n),
            kind: ObjectKind::Blob,
            base: base.map(oid),
            searched_delta: None,
            reused_delta_zlib: None,
        }
    }

    fn order_of(mut entries: Vec<PlannedEntry>) -> Vec<u8> {
        order_bases_first(&mut entries);
        entries.iter().map(|e| e.oid.as_bytes()[0]).collect()
    }

    #[test]
    fn bases_are_ordered_before_their_targets() {
        // 1 -> 3 -> 5 chain discovered target-first; 9 is an out-of-pack (thin) base.
        let entries = vec![
            planned(1, Some(3)),
            planned(2, None),
            planned(3, Some(5)),
            planned(4, Some(9)),
            planned(5, None),
            planned(6, Some(2)),
        ];
        assert_eq!(order_of(entries), [5, 3, 1, 2, 4, 6]);
    }

    #[test]
    fn base_cycles_do_not_hang_or_drop_entries() {
        let entries = vec![planned(1, Some(2)), planned(2, Some(1)), planned(3, None)];
        let mut order = order_of(entries);
        order.sort_unstable();
        assert_eq!(order, [1, 2, 3]);
    }

    /// Enumerate everything reachable from `main` in a git-packed fixture.
    pub(crate) fn packed_selection(
        commits: usize,
    ) -> (
        tempfile::TempDir,
        Repository,
        crate::pack_object_select::PackObjectSelection,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let path = grit_test_support::git_packed_history(tmp.path(), commits);
        let tip = grit_test_support::git(&path, &["rev-parse", "main"]);
        let tip = ObjectId::from_hex(tip.trim()).unwrap();
        let repo = Repository::open(&path, &RepositoryOptions::empty()).unwrap();
        let empty = HashSet::new();
        let selection = enumerate_pack_objects(
            repo.odb(),
            &[tip],
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
        (tmp, repo, selection)
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn serve_plan_reuses_on_disk_deltas() {
        let (_tmp, repo, selection) = packed_selection(80);
        let opts = PackBuildOptions::for_serve(None, false, true);
        let plan = plan_deltas(
            repo.odb(),
            &selection.send,
            &selection.have_closure,
            selection.peer_held.as_deref(),
            &opts,
            &selection.name_hashes,
            None,
        )
        .unwrap();

        let reused = plan
            .entries
            .iter()
            .filter(|e| e.reused_delta_zlib.is_some())
            .count();
        let trees_and_blobs = plan
            .entries
            .iter()
            .filter(|e| matches!(e.kind, ObjectKind::Tree | ObjectKind::Blob))
            .count();
        assert!(
            reused * 2 > trees_and_blobs,
            "most trees and blobs keep their on-disk delta ({reused} of {trees_and_blobs})"
        );
    }

    /// A history of loose objects: a subdirectory of stable files plus two that change
    /// in every commit.
    fn loose_history(commits: usize) -> (tempfile::TempDir, Repository, ObjectId) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grit_test_support::git(dir, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for i in 0..20 {
            std::fs::write(
                dir.join(format!("src/stable{i}.txt")),
                format!("stable {i}\n"),
            )
            .unwrap();
        }
        let mut lines: Vec<String> = (0..200).map(|i| format!("line {i} of the file")).collect();
        for c in 0..commits {
            lines[(c * 13) % 200] = format!("changed in commit {c}");
            std::fs::write(dir.join("src/file.txt"), lines.join("\n")).unwrap();
            std::fs::write(dir.join("src/other.txt"), format!("other {c}\n")).unwrap();
            grit_test_support::git(dir, &["add", "."]);
            grit_test_support::git(
                dir,
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@e",
                    "commit",
                    "-qm",
                    "c",
                ],
            );
        }
        let tip = grit_test_support::git(dir, &["rev-parse", "main"]);
        let tip = ObjectId::from_hex(tip.trim()).unwrap();
        let repo = Repository::open(&dir.join(".git"), &RepositoryOptions::empty()).unwrap();
        (tmp, repo, tip)
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn loose_plan_deltifies_trees_and_blobs_with_compressed_search_output() {
        let (tmp, repo, tip) = loose_history(12);
        assert!(!objects_dir_has_local_packs(repo.odb().objects_dir()));
        let empty = HashSet::new();
        let opts = PackBuildOptions::for_local_copy(None);
        let selection = crate::pack_object_select::enumerate_pack_objects_naming(
            repo.odb(),
            &[tip],
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
            NameTrees::for_delta_search(&opts),
        )
        .unwrap();
        let plan = plan_deltas(
            repo.odb(),
            &selection.send,
            &selection.have_closure,
            selection.peer_held.as_deref(),
            &opts,
            &selection.name_hashes,
            Some(1),
        )
        .unwrap();
        let deltas_of = |kind| {
            plan.entries
                .iter()
                .filter(|e| e.kind == kind && e.base.is_some())
                .count()
        };
        assert!(deltas_of(ObjectKind::Blob) >= 10, "blob deltas");
        assert!(deltas_of(ObjectKind::Tree) >= 10, "subtree deltas");
        assert!(plan
            .entries
            .iter()
            .filter(|e| e.base.is_some())
            .all(|e| e.searched_delta.is_some()));

        let pack = crate::pack_delta::serialize_pack_with_deltas(
            repo.odb(),
            &plan.entries,
            &opts,
            Some(2),
        )
        .unwrap();
        std::fs::write(tmp.path().join("plan.pack"), &pack.bytes).unwrap();
        grit_test_support::git(tmp.path(), &["index-pack", "--strict", "plan.pack"]);
    }

    #[test]
    fn serve_options_follow_pack_config() {
        use crate::config::{ConfigFile, ConfigScope, ConfigSet};
        let file = ConfigFile::parse(
            Path::new(".git/config"),
            "[pack]\n\twindow = 4\n\tdepth = 0\n",
            ConfigScope::Local,
        )
        .unwrap();
        let mut cfg = ConfigSet::new();
        cfg.merge(&file);
        let opts = PackBuildOptions::for_serve(Some(&cfg), true, false);
        assert_eq!((opts.window, opts.max_depth), (4, 0));
        assert!(opts.thin && !opts.use_ofs_delta && opts.delta);
        assert!(
            opts.reuse_deltas,
            "like git upload-pack, depth 0 stops new deltas but still sends on-disk deltas"
        );

        let defaults = PackBuildOptions::for_serve(None, false, true);
        assert_eq!(
            (defaults.window, defaults.max_depth),
            (10, crate::pack::DEFAULT_PACK_DEPTH)
        );
        assert!(defaults.reuse_deltas && defaults.reuse_objects && !defaults.thin);
    }
}
