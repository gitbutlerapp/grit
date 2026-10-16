//! Negotiation-driven pack-objects: enumerate wants minus haves, delta search, stream PACK v2.
//!
//! Holds object metadata (not payloads) during planning; reads object bytes from the
//! [`Odb`] on demand when encoding.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::delta_encode::{encode_prefix_extension_delta, DeltaIndex};
use crate::error::{Error, Result};
use crate::hash::HashingWriter;
use crate::objects::{parse_commit, parse_tag, parse_tree, HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
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

    /// Production pack settings for an in-process local push (thin delta pack).
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

    /// Whether pack bitmaps may be used (`pack.useBitmaps`, default true).
    #[must_use]
    pub fn use_bitmaps_from_config(cfg: Option<&crate::config::ConfigSet>) -> bool {
        cfg.map(crate::config::ConfigSet::pack_use_bitmaps)
            .unwrap_or(true)
    }

    /// Upload-pack gating: `pack.useBitmaps` and `uploadpack.allowBitmaps`.
    #[must_use]
    pub fn use_bitmaps_for_upload_pack(cfg: Option<&crate::config::ConfigSet>) -> bool {
        let pack_ok = Self::use_bitmaps_from_config(cfg);
        let allow = cfg
            .map(crate::config::ConfigSet::uploadpack_allow_bitmaps)
            .unwrap_or(true);
        pack_ok && allow
    }
}

/// Extended options for [`PackObjects`].
#[derive(Clone, Copy, Debug)]
pub struct PackObjectsOptions {
    /// Core pack-objects behaviour flags.
    pub build: PackBuildOptions,
    /// Parallel delta-search worker threads (`1` = single-threaded, the default).
    pub delta_threads: usize,
}

impl Default for PackObjectsOptions {
    fn default() -> Self {
        Self {
            build: PackBuildOptions::default(),
            delta_threads: 1,
        }
    }
}

impl From<PackBuildOptions> for PackObjectsOptions {
    fn from(build: PackBuildOptions) -> Self {
        Self {
            build,
            delta_threads: 1,
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
    Enumerated { total: usize },
    /// Delta planning finished.
    PlannedDeltas,
    /// An object was written (`written` of `total`).
    WroteObject { written: usize, total: usize },
}

/// Streaming pack-objects builder over an [`Odb`].
pub struct PackObjects<'a> {
    odb: &'a Odb,
    source_odb: &'a Odb,
    opts: PackObjectsOptions,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    have_shallow: HashSet<ObjectId>,
    source_shallow: HashSet<ObjectId>,
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
            have_shallow: HashSet::new(),
            source_shallow: HashSet::new(),
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
        self.have_shallow = grafts.clone();
        self
    }

    /// Shallow grafts on the source side (do not walk past on source).
    #[must_use]
    pub fn source_shallow_grafts(mut self, grafts: &HashSet<ObjectId>) -> Self {
        self.source_shallow = grafts.clone();
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
        let have_closure = crate::pack_object_select::reachable_closure_walk(
            self.odb,
            &self.haves,
            &HashSet::new(),
            true,
            &self.have_shallow,
        )?;
        let selection = crate::pack_object_select::enumerate_pack_objects(
            self.source_odb,
            &self.wants,
            &self.haves,
            &crate::pack_object_select::PackEnumerateOptions {
                filter: None,
                shallow_grafts: &self.source_shallow,
                use_bitmaps: opts.use_bitmaps,
                exclude_objects: Some(&have_closure),
            },
        )?;
        let send = selection.send;
        self.fire(PackProgress::Enumerated { total: send.len() });
        if !opts.delta {
            return write_whole_object_pack_stream(self.source_odb, &send, &opts, w);
        }
        let plan = plan_deltas(
            self.source_odb,
            &send,
            &have_closure,
            &opts,
            &selection.blob_name_hashes,
        )?;
        self.fire(PackProgress::PlannedDeltas);
        write_delta_pack_stream(self.source_odb, &plan, &opts, w, None)
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
    let mut seen = HashSet::new();
    let order = collect_reachable_excluding(odb, roots, stop, skip_missing, shallow_grafts)?;
    for oid in order {
        seen.insert(oid);
    }
    Ok(seen)
}

/// Build a v2 packfile containing exactly the objects reachable from `wants`
/// but **not** reachable from `haves`, de-duplicated.
pub fn build_pack(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    build_pack_with_shallow_and_filter(odb, wants, haves, &HashSet::new(), None, opts)
}

/// Build a pack for upload-pack with optional shallow grafts and object filter.
///
/// When `filter` is set, object selection follows `rev-list --objects --filter`
/// (same path as filtered bundles).
pub fn build_pack_with_shallow_and_filter(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
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

/// Same as [`build_pack_with_shallow_and_filter`] with a live [`Repository`] (for upload-pack).
pub fn build_pack_for_upload(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    build_pack_with_shallow_and_filter(&repo.odb, wants, haves, source_shallow, filter, opts)
}

/// Stream a pack for upload-pack: pack reuse when bitmaps allow, otherwise pack-objects.
///
/// Returns [`None`] when the caller should use another code path (should not happen today).
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
        let mut shallow_grafts = source_shallow.clone();
        shallow_grafts.extend(have_shallow.iter().copied());
        let pack = build_pack_with_shallow_and_filter(
            &repo.odb,
            wants,
            haves,
            &shallow_grafts,
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
            filter,
            opts,
            progress,
            w,
        )? {
            return Ok(stats);
        }
    }
    let _ = progress;
    PackObjects::new(&repo.odb, PackObjectsOptions::from(*opts))
        .wants(wants)
        .haves(haves)
        .source_shallow_grafts(source_shallow)
        .have_shallow_grafts(have_shallow)
        .write_to(w)
}

struct ByteCountWriter<W: Write> {
    inner: W,
    bytes: u64,
}

impl<W: Write> ByteCountWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }
    fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl<W: Write> Write for ByteCountWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn write_pack_header(w: &mut dyn Write, count: u32) -> Result<()> {
    w.write_all(b"PACK").map_err(Error::Io)?;
    w.write_all(&2u32.to_be_bytes()).map_err(Error::Io)?;
    w.write_all(&count.to_be_bytes()).map_err(Error::Io)?;
    Ok(())
}

fn write_whole_object_pack_stream(
    odb: &Odb,
    send: &[ObjectId],
    opts: &PackBuildOptions,
    w: &mut dyn Write,
) -> Result<PackStats> {
    let algo = odb.hash_algo();
    let count = u32::try_from(send.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    let counted = ByteCountWriter::new(w);
    let mut hasher = HashingWriter::new(algo, counted);
    write_pack_header(&mut hasher, count)?;
    for oid in send {
        let obj = odb.read(oid)?;
        let mut chunk = Vec::new();
        write_whole_pack_object(&mut chunk, odb, *oid, obj.kind, &obj.data, opts)?;
        hasher.write_all(&chunk).map_err(Error::Io)?;
    }
    let (counted, _) = hasher.finish().map_err(Error::Io)?;
    Ok(PackStats {
        object_count: count,
        pack_bytes: counted.bytes(),
        delta_count: 0,
        verbatim_reuse_bytes: 0,
        verbatim_reuse_objects: 0,
        pack_ofs_fixup_objects: 0,
        pack_ref_from_ofs_objects: 0,
    })
}

/// Stream a delta pack from a plan, optionally applying bitmap verbatim reuse.
pub(crate) fn write_delta_pack_stream(
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    w: &mut dyn Write,
    reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
) -> Result<PackStats> {
    let algo = odb.hash_algo();
    let count = u32::try_from(plan.entries.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    let delta_count = plan.entries.iter().filter(|e| e.base.is_some()).count() as u32;
    let counted = ByteCountWriter::new(w);
    let mut hasher = HashingWriter::new(algo, counted);
    write_pack_header(&mut hasher, count)?;
    let reuse_stats = write_planned_entries(&mut hasher, odb, plan, opts, 12, reuse)?;
    let (counted, _) = hasher.finish().map_err(Error::Io)?;
    Ok(PackStats {
        object_count: count,
        pack_bytes: counted.bytes(),
        delta_count,
        verbatim_reuse_bytes: reuse_stats.verbatim_reuse_bytes,
        verbatim_reuse_objects: reuse_stats.verbatim_reuse_objects,
        pack_ofs_fixup_objects: reuse_stats.pack_ofs_fixup_objects,
        pack_ref_from_ofs_objects: reuse_stats.pack_ref_from_ofs_objects,
    })
}

/// Append negotiated objects to an in-progress pack body (no header or trailer).
pub(crate) fn write_pack_body_from_send_list(
    w: &mut dyn Write,
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    pack_body_start: u64,
) -> Result<u32> {
    if send.is_empty() {
        return Ok(0);
    }
    if !opts.delta {
        for oid in send {
            let obj = odb.read(oid)?;
            let mut chunk = Vec::new();
            write_whole_pack_object(&mut chunk, odb, *oid, obj.kind, &obj.data, opts)?;
            w.write_all(&chunk).map_err(Error::Io)?;
        }
        return Ok(u32::try_from(send.len()).unwrap_or(u32::MAX));
    }
    let plan = plan_deltas(odb, send, have_closure, opts, blob_name_hashes)?;
    write_planned_entries(w, odb, &plan, opts, pack_body_start, None)?;
    Ok(u32::try_from(plan.entries.len()).unwrap_or(u32::MAX))
}

fn write_planned_entries(
    w: &mut dyn Write,
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    pack_body_start: u64,
    mut reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
) -> Result<crate::pack_reuse::ReuseWriteStats> {
    let algo = odb.hash_algo();
    let payloads: HashMap<ObjectId, &[u8]> = plan
        .entries
        .iter()
        .filter_map(|e| e.data.as_ref().map(|d| (e.oid, d.as_slice())))
        .collect();
    let mut oid_to_offset: HashMap<ObjectId, u64> = HashMap::new();
    let mut delta_index_by_base: HashMap<ObjectId, DeltaIndex> = HashMap::new();
    let mut body_written = 0u64;

    for entry in &plan.entries {
        let start = pack_body_start + body_written;
        if entry.base.is_none() {
            if let Some(reuse) = reuse.as_mut() {
                if let Some(n) = reuse.try_write_whole(w, &entry.oid, start, opts)? {
                    body_written += n;
                    oid_to_offset.insert(entry.oid, start);
                    continue;
                }
            }
            let mut chunk = Vec::new();
            let payload = match &entry.data {
                Some(d) => d.as_slice(),
                None => {
                    let obj = odb.read(&entry.oid)?;
                    write_whole_pack_object(
                        &mut chunk, odb, entry.oid, entry.kind, &obj.data, opts,
                    )?;
                    w.write_all(&chunk).map_err(Error::Io)?;
                    body_written += chunk.len() as u64;
                    oid_to_offset.insert(entry.oid, start);
                    continue;
                }
            };
            write_whole_pack_object(&mut chunk, odb, entry.oid, entry.kind, payload, opts)?;
            w.write_all(&chunk).map_err(Error::Io)?;
            body_written += chunk.len() as u64;
            oid_to_offset.insert(entry.oid, start);
            continue;
        }
        let Some(base_oid) = entry.base else {
            continue;
        };
        let in_pack_offset = oid_to_offset.get(&base_oid).copied();
        if let Some(reuse) = reuse.as_mut() {
            if let Some(n) =
                reuse.try_write_delta(w, &entry.oid, start, in_pack_offset, opts, &oid_to_offset)?
            {
                body_written += n;
                oid_to_offset.insert(entry.oid, start);
                continue;
            }
        }
        let mut chunk = Vec::new();
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
            let target_data = match &entry.data {
                Some(d) => d.as_slice(),
                None => {
                    let obj = odb.read(&entry.oid)?;
                    if obj.data.is_empty() {
                        return Err(Error::CorruptObject(
                            "delta entry missing payload".to_owned(),
                        ));
                    }
                    // Fresh delta against a whole object: read once for encoding.
                    let base_data: Vec<u8> = if let Some(d) = payloads.get(&base_oid) {
                        d.to_vec()
                    } else {
                        odb.read(&base_oid)?.data
                    };
                    let delta =
                        if obj.data.starts_with(&base_data) && obj.data.len() > base_data.len() {
                            encode_prefix_extension_delta(&base_data, &obj.data)?
                        } else {
                            let base_ref = payloads
                                .get(&base_oid)
                                .copied()
                                .unwrap_or(base_data.as_slice());
                            delta_index_by_base
                                .entry(base_oid)
                                .or_insert_with(|| DeltaIndex::new(base_ref))
                                .encode(&obj.data, 0)
                                .ok_or_else(|| {
                                    Error::CorruptObject(
                                        "failed to encode pack delta for object".into(),
                                    )
                                })?
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
                    w.write_all(&chunk).map_err(Error::Io)?;
                    body_written += chunk.len() as u64;
                    oid_to_offset.insert(entry.oid, start);
                    continue;
                }
            };
            let delta = if let Some(reused) = &entry.reused_delta {
                reused.clone()
            } else {
                let base_data: Vec<u8> = if let Some(d) = payloads.get(&base_oid) {
                    d.to_vec()
                } else {
                    odb.read(&base_oid)?.data
                };
                if target_data.starts_with(&base_data) && target_data.len() > base_data.len() {
                    encode_prefix_extension_delta(&base_data, target_data)?
                } else {
                    let base_ref = payloads
                        .get(&base_oid)
                        .copied()
                        .unwrap_or(base_data.as_slice());
                    delta_index_by_base
                        .entry(base_oid)
                        .or_insert_with(|| DeltaIndex::new(base_ref))
                        .encode(target_data, 0)
                        .ok_or_else(|| {
                            Error::CorruptObject("failed to encode pack delta for object".into())
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
        w.write_all(&chunk).map_err(Error::Io)?;
        body_written += chunk.len() as u64;
        oid_to_offset.insert(entry.oid, start);
    }
    Ok(reuse.as_mut().map(|r| r.take_stats()).unwrap_or_default())
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
    if !opts.delta {
        return serialize_pack(odb, send, opts);
    }
    let plan = plan_deltas(odb, send, have_closure, opts, blob_name_hashes)?;
    serialize_pack_with_deltas(odb, &plan, opts)
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
    exclude: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut ordered: Vec<ObjectId> = Vec::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    let enqueue = |oid: ObjectId,
                   queue: &mut VecDeque<ObjectId>,
                   visited: &mut HashSet<ObjectId>,
                   ordered: &mut Vec<ObjectId>|
     -> bool {
        if exclude.contains(&oid) {
            return false;
        }
        if visited.insert(oid) {
            ordered.push(oid);
            queue.push_back(oid);
            true
        } else {
            false
        }
    };

    for &root in roots {
        enqueue(root, &mut queue, &mut visited, &mut ordered);
    }

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
                for entry in parse_tree(&obj.data)? {
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
fn pack_type_code(kind: ObjectKind) -> u8 {
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
    if opts.reuse_objects {
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
    buf.extend_from_slice(algo.digest(&*buf).as_bytes());
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
struct PlannedEntry {
    oid: ObjectId,
    kind: ObjectKind,
    /// Payload when needed for delta planning; `None` for whole objects read at write time.
    data: Option<Vec<u8>>,
    /// `Some(base_oid)` when this entry is a (blob) delta against `base_oid`.
    /// The base may be an in-pack object or — for thin packs — an external base
    /// present only on the peer.
    base: Option<ObjectId>,
    /// Uncompressed delta bytes when freshly computed or reused after inflate.
    reused_delta: Option<Vec<u8>>,
    /// On-disk reuse: uncompressed size from the source pack header plus zlib
    /// bytes copied verbatim from a cached pack buffer (Git's `reuse_delta`).
    reused_delta_zlib: Option<(u64, std::sync::Arc<crate::pack::PackData>, usize, usize)>,
}

/// The full delta plan: the ordered entries to emit plus the set of external
/// (thin) bases that were referenced but deliberately not emitted.
pub(crate) struct DeltaPlan {
    entries: Vec<PlannedEntry>,
    #[allow(dead_code)]
    external_bases: HashSet<ObjectId>,
}

/// Length of the common prefix of `a` and `b`.
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

/// Select blob deltas for `send` and produce an ordered emit plan.
///
/// A lift of the CLI's `optimize_blob_deltas`: a size-sorted prefix/LCP window
/// heuristic over blobs, depth-limited via [`crate::pack::apply_delta_depth_limit`]. Trees and
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
/// peer's `haves` (`have_closure`) even though that base is not emitted; the base
/// oid is recorded in [`DeltaPlan::external_bases`] and referenced via REF_DELTA.
pub(crate) fn plan_deltas(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
) -> Result<DeltaPlan> {
    let mut kinds: HashMap<ObjectId, ObjectKind> = HashMap::new();
    let mut blob_data: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    for &oid in send {
        let obj = odb.read(&oid)?;
        kinds.insert(oid, obj.kind);
        if obj.kind == ObjectKind::Blob && !obj.data.is_empty() {
            blob_data.insert(oid, obj.data);
        }
    }

    let in_pack: HashSet<ObjectId> = send.iter().copied().collect();

    // Delta islands (`--delta-islands`): only loaded when requested AND a git dir
    // is attached to the odb. An inactive island set (no `pack.island` config, or
    // no matched ref) imposes no restriction, so the common case is unaffected.
    let islands = load_islands_for_pack(odb, &in_pack, opts);

    // target oid -> base oid (the object `target` deltas against).
    let mut delta_to_base: HashMap<ObjectId, ObjectId> = HashMap::new();
    // Deltas whose instruction stream is reused verbatim from an existing pack.
    let mut reused_zlib: HashMap<
        ObjectId,
        (u64, std::sync::Arc<crate::pack::PackData>, usize, usize),
    > = HashMap::new();
    let mut external_bases: HashSet<ObjectId> = HashSet::new();

    // On-disk delta reuse runs even with `--window=0` (no new deltas), matching Git pack-objects.
    if opts.reuse_deltas {
        let objects_dir = odb.objects_dir();
        let candidates =
            crate::pack::collect_packed_delta_reuse_for_send(objects_dir, &in_pack, &in_pack)?;
        for (t, (base, size, pack_bytes, zlib_start, zlib_end)) in candidates {
            if blob_data.get(&t).is_some_and(|d| d.is_empty()) {
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
    let reuse_delta_bases: HashSet<ObjectId> = delta_to_base
        .iter()
        .filter(|(t, _)| reused_zlib.contains_key(t))
        .map(|(_, b)| *b)
        .collect();

    if opts.window > 0 && opts.max_depth > 0 {
        // Blobs in the pack, smallest-first (size-sorted window proximity).
        let mut blobs: Vec<ObjectId> = blob_data.keys().copied().collect();
        blobs.sort_by(|a, b| {
            let sa = blob_data[a].len();
            let sb = blob_data[b].len();
            sa.cmp(&sb).then_with(|| {
                blob_name_hashes
                    .get(a)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&blob_name_hashes.get(b).copied().unwrap_or(0))
            })
        });

        // Optional thin bases: blobs present only on the peer that a packed blob
        // could delta against. We load them lazily and cache by oid.
        let mut external_blob_data: HashMap<ObjectId, Vec<u8>> = HashMap::new();
        if opts.thin {
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
        }

        for (i, &t) in blobs.iter().enumerate() {
            // A reused on-disk delta already covers this target.
            if delta_to_base.contains_key(&t) {
                continue;
            }
            // Do not replace a reuse base with a fresh delta encoding.
            if reuse_delta_bases.contains(&t) {
                continue;
            }
            let t_data = &blob_data[&t];

            // (base, common, base_len, external). When islands are active the
            // selection additionally prefers a base in a dominating island via
            // `delta_cmp`, matching the CLI's `island_delta_cmp` bias.
            let mut best: Option<(ObjectId, usize, usize, bool)> = None;

            // Consider larger in-pack blobs within the window (closest in size).
            // `blobs` is ascending by size, so later entries are the larger bases.
            for &b in blobs.iter().skip(i + 1).take(opts.window) {
                // Island rule: never base `t` on a blob in a non-superset island.
                if !islands.in_same_island(&t, &b) {
                    continue;
                }
                let b_data = &blob_data[&b];
                if b_data.len() <= t_data.len() {
                    continue;
                }
                let common = if b_data.starts_with(t_data) {
                    t_data.len()
                } else {
                    common_prefix_len(t_data, b_data)
                };
                if common > 64 && common.saturating_mul(2) >= t_data.len() {
                    let better = best.is_none_or(|(prev_b, bc, bl, _)| {
                        // Prefer a strictly dominating island first (Git's
                        // `island_delta_cmp`), then more common prefix, then the
                        // smaller (closer-in-size) base.
                        if islands.is_active() {
                            let cmp = islands.delta_cmp(&b, &prev_b);
                            if cmp < 0 {
                                return true;
                            }
                            if cmp > 0 {
                                return false;
                            }
                        }
                        common > bc || (common == bc && b_data.len() < bl)
                    });
                    if better {
                        best = Some((b, common, b_data.len(), false));
                    }
                }
            }

            // Thin: also consider peer-held external bases. An external base may
            // be SMALLER than the target (the common "target extends an earlier
            // version" case) — that is still a cheap delta and, because external
            // bases are never emitted, can never form an in-pack chain cycle. We
            // only switch to a thin base when no equally-good in-pack base exists.
            if opts.thin {
                for (&b, b_data) in &external_blob_data {
                    if b == t {
                        continue;
                    }
                    // External (peer-held) bases participate in island rules too.
                    if !islands.in_same_island(&t, &b) {
                        continue;
                    }
                    let common = common_prefix_len(t_data, b_data);
                    if common > 64 && common.saturating_mul(2) >= t_data.len() {
                        let better = best.is_none_or(|(_, bc, bl, ext)| {
                            common > bc || (common == bc && ext && b_data.len() < bl)
                        });
                        if better {
                            best = Some((b, common, b_data.len(), true));
                        }
                    }
                }
            }

            if let Some((base, _, _, external)) = best {
                delta_to_base.insert(t, base);
                if external {
                    external_bases.insert(base);
                    let _ = external_blob_data.get(&base);
                }
            }
        }
    }

    if opts.max_depth > 0 && !delta_to_base.is_empty() {
        // Cap chain length. After snipping, any removed target reverts to whole.
        crate::pack::apply_delta_depth_limit(&mut delta_to_base, opts.max_depth);

        // A reused delta whose target was snipped (or whose base ceased to be the
        // chosen base) reverts to a freshly-computed full/delta object.
        reused_zlib.retain(|t, _| delta_to_base.contains_key(t));

        // A base that is no longer referenced as an external base (because its
        // only dependent was snipped) must not be counted as external.
        external_bases.retain(|b| delta_to_base.values().any(|v| v == b));
    }

    // Emit in the original discovery order so commits precede their trees/blobs.
    // For OFS_DELTA the serializer needs each base to appear before its target;
    // discovery order already places a larger base blob no earlier than a smaller
    // one only by coincidence, so the serializer falls back to REF_DELTA whenever
    // the base has not yet been written.
    let mut entries: Vec<PlannedEntry> = Vec::with_capacity(send.len());
    for &oid in send {
        let kind = kinds[&oid];
        let data = blob_data.get(&oid).cloned();
        entries.push(PlannedEntry {
            oid,
            kind,
            data,
            base: delta_to_base.get(&oid).copied(),
            reused_delta: None,
            reused_delta_zlib: reused_zlib.get(&oid).cloned(),
        });
    }

    Ok(DeltaPlan {
        entries,
        external_bases,
    })
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
    let Ok(repo) = crate::repo::Repository::open(git_dir, None) else {
        return crate::delta_islands::DeltaIslands::default();
    };
    let env = crate::environment::Environment::empty();
    let cfg = crate::config::ConfigSet::load(&env, Some(git_dir), true).unwrap_or_default();
    crate::delta_islands::load_delta_islands(&repo, &cfg, in_pack)
}

/// Serialize a [`DeltaPlan`] into a PACK v2 stream.
///
/// Whole entries are written as base objects; delta entries are written as
/// `OFS_DELTA` when the base is already in the pack at a known offset and
/// `opts.use_ofs_delta` is set, otherwise `REF_DELTA` (which also covers thin /
/// external bases that are never emitted).
fn serialize_pack_with_deltas(
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    write_delta_pack_stream(odb, plan, opts, &mut buf, None)?;
    Ok(buf)
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
